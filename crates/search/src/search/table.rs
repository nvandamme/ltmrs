//! Lance search projection table: publish/read of SearchRows (WP-05 tasks 2, 3).

use std::sync::Arc;

use lancedb::arrow::arrow_array::{StringArray, UInt64Array};
use lancedb::arrow::arrow_schema::{DataType, Field, SchemaRef};
use uuid::Uuid;

use crate::search::row::SearchRow;
use ltmrs_domain::command::{DomainError, DomainErrorCode, DomainResult};
use ltmrs_domain::id::{ChunkId, DocumentRevision, EntityId, ModelFingerprint, StoreGeneration};

mod maintenance;
#[cfg(test)]
mod maintenance_tests;
mod reads;
#[cfg(test)]
mod reads_tests;
#[cfg(test)]
mod test_support;
mod writes;

pub const SEARCH_TABLE: &str = "search";
/// 384 matches the E5-small candidate dimension (AD-04).
const EMBEDDING_DIM: u32 = 384;

/// Explicit resource budgets for maintenance optimization and retention
/// (task 10, RQ-22): nothing here allocates beyond these limits or removes a
/// dataset version another reader may still hold. Every tunable is pinned so
/// the daemon never depends on library defaults it did not choose.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MaintenanceBudget {
    /// Max parallel compaction tasks (CPU bound).
    pub compaction_threads: usize,
    /// Max bytes per file when rewriting fragments during compaction.
    pub max_compact_bytes_per_file: usize,
    /// Retain dataset versions at least this long so a live reader pinned to an
    /// older version keeps its files (snapshot protection). Lance itself never
    /// deletes unverified files newer than 7 days; this must not undercut that.
    pub retain_millis: u64,
}

impl Default for MaintenanceBudget {
    fn default() -> Self {
        Self {
            compaction_threads: 2,
            max_compact_bytes_per_file: 1024 * 1024 * 1024, // 1 GiB
            retain_millis: 7 * 24 * 60 * 60 * 1000,         // 7 days (Lance's safety floor)
        }
    }
}

/// Arrow schema for the search projection table. The vector column is a
/// nullable fixed-size list so lexical-ready rows carry no embedding (AD-06).
/// `chunker_version` is appended last so all pre-existing column positions
/// stay stable; tables predating it are refused at open with a rebuild
/// directive (the projection is derived state, rebuilt from canonical).
pub fn search_schema(dim: u32) -> SchemaRef {
    Arc::new(lancedb::arrow::arrow_schema::Schema::new(vec![
        Field::new("store_generation", DataType::UInt64, false),
        Field::new("memory_id", DataType::Utf8, false),
        Field::new("document_revision", DataType::UInt64, false),
        Field::new("model_fingerprint", DataType::UInt64, false),
        Field::new("chunk_id", DataType::UInt32, false),
        Field::new("lexical_text", DataType::Utf8, false),
        Field::new("char_start", DataType::UInt64, false),
        Field::new("char_end", DataType::UInt64, false),
        Field::new("project", DataType::Utf8, true),
        Field::new("fragment_type", DataType::Utf8, false),
        Field::new("created_at_millis", DataType::UInt64, false),
        Field::new("updated_at_millis", DataType::UInt64, false),
        Field::new(
            "embedding",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                dim as i32,
            ),
            true,
        ),
        Field::new("chunker_version", DataType::Utf8, false),
        // Confidence rides last for the same positional-stability reason:
        // it enables pre-filtering at the source (RV-13), and tables
        // predating it rebuild from canonical like chunker_version did.
        Field::new("confidence", DataType::Float64, false),
    ]))
}

#[derive(Clone)]
pub struct SearchTable {
    db: lancedb::connection::Connection,
    table: lancedb::table::Table,
}

impl SearchTable {}

/// Schema gate shared by open and refresh: the chunker_version column must
/// exist at its exact positional slot with the exact type, because reads
/// are positional. Anything else risks silent column misalignment, so it is
/// refused with a rebuild directive (the projection is derived state).
fn require_chunker_version(schema: &SchemaRef) -> DomainResult<()> {
    let rebuild = || {
        DomainError::new(
            DomainErrorCode::Validation,
            "search table predates chunker versioning; rebuild the projection",
        )
    };
    let field = schema
        .field_with_name("chunker_version")
        .map_err(|_| rebuild())?;
    let at_slot = schema.index_of("chunker_version").map_err(|_| rebuild())?;
    if at_slot != 13 || field.data_type() != &DataType::Utf8 || field.is_nullable() {
        return Err(DomainError::new(
            DomainErrorCode::Validation,
            "search table has an incompatible chunker_version column; rebuild the projection",
        ));
    }
    let confidence = schema.field_with_name("confidence").map_err(|_| {
        DomainError::new(
            DomainErrorCode::Validation,
            "search table predates confidence pre-filtering; rebuild the projection",
        )
    })?;
    let at_slot = schema.index_of("confidence").map_err(|_| {
        DomainError::new(
            DomainErrorCode::Validation,
            "search table predates confidence pre-filtering; rebuild the projection",
        )
    })?;
    if at_slot != 14 || confidence.data_type() != &DataType::Float64 || confidence.is_nullable() {
        return Err(DomainError::new(
            DomainErrorCode::Validation,
            "search table has an incompatible confidence column; rebuild the projection",
        ));
    }
    Ok(())
}

fn row_batch(
    rows: &[SearchRow],
    schema: &SchemaRef,
) -> DomainResult<lancedb::arrow::arrow_array::RecordBatch> {
    use lancedb::arrow::arrow_array::{FixedSizeListArray, Float32Array, UInt32Array};
    use lancedb::arrow::arrow_buffer::NullBufferBuilder;

    let dim = match schema.field_with_name("embedding") {
        Ok(field) => match field.data_type() {
            DataType::FixedSizeList(f, size) => (f.clone(), *size),
            _ => {
                return Err(DomainError::new(
                    DomainErrorCode::Validation,
                    "embedding column is not a fixed-size list",
                ));
            }
        },
        Err(_) => {
            return Err(DomainError::new(
                DomainErrorCode::Validation,
                "no embedding column in schema",
            ));
        }
    };

    let mut values: Vec<Option<f32>> = Vec::with_capacity(rows.len() * dim.1 as usize);
    let mut null_builder = NullBufferBuilder::new(rows.len());
    for row in rows {
        match &row.embedding {
            Some(vec) if vec.len() == dim.1 as usize => {
                values.extend_from_slice(&vec.iter().map(|v| Some(*v)).collect::<Vec<_>>());
                null_builder.append(true);
            }
            _ => {
                // Null vector: fill with None so the FixedSizeList stays well-formed.
                values.resize(values.len() + dim.1 as usize, None);
                null_builder.append(false);
            }
        }
    }

    let flat = Float32Array::from(values.clone());
    let list_array =
        FixedSizeListArray::try_new(dim.0, dim.1, Arc::new(flat), null_builder.finish())
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;

    lancedb::arrow::arrow_array::RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from_iter_values(
                rows.iter().map(|r| r.store_generation.as_u64()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.memory_id.as_uuid().to_string()),
            )),
            Arc::new(UInt64Array::from_iter_values(
                rows.iter().map(|r| r.document_revision.as_u64()),
            )),
            Arc::new(UInt64Array::from_iter_values(
                rows.iter().map(|r| r.model_fingerprint.as_u64()),
            )),
            Arc::new(UInt32Array::from_iter_values(
                rows.iter().map(|r| r.chunk_id.as_u32()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.lexical_text.clone()),
            )),
            Arc::new(UInt64Array::from_iter_values(
                rows.iter().map(|r| r.char_start),
            )),
            Arc::new(UInt64Array::from_iter_values(
                rows.iter().map(|r| r.char_end),
            )),
            Arc::new(StringArray::from(
                rows.iter().map(|r| r.project.clone()).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.fragment_type.clone()),
            )),
            Arc::new(UInt64Array::from_iter_values(
                rows.iter().map(|r| r.created_at_millis),
            )),
            Arc::new(UInt64Array::from_iter_values(
                rows.iter().map(|r| r.updated_at_millis),
            )),
            Arc::new(list_array),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.chunker_version.clone()),
            )),
            Arc::new(lancedb::arrow::arrow_array::Float64Array::from_iter_values(
                rows.iter().map(|r| r.confidence),
            )),
        ],
    )
    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))
}

fn batches_to_rows(
    batches: &[lancedb::arrow::arrow_array::RecordBatch],
) -> DomainResult<Vec<SearchRow>> {
    use lancedb::arrow::arrow_array::{
        Array, FixedSizeListArray, Float32Array, Float64Array, UInt32Array,
    };
    let corrupt = |row: usize, column: &str| {
        DomainError::new(
            DomainErrorCode::Validation,
            format!("corrupt search row {row}: column {column} has an unexpected type or value"),
        )
    };
    let mut out = Vec::new();
    for batch in batches {
        if batch.num_rows() == 0 {
            continue;
        }
        // Resolve columns by name once per batch: positional reads silently
        // misalign when the schema evolves (appended _distance/_score today,
        // anything tomorrow). A missing column fails the read loudly.
        let schema = batch.schema();
        let col = |row: usize, name: &'static str| -> DomainResult<usize> {
            schema.index_of(name).map_err(|_| corrupt(row, name))
        };
        let c_store_generation = col(0, "store_generation")?;
        let c_memory_id = col(0, "memory_id")?;
        let c_document_revision = col(0, "document_revision")?;
        let c_model_fingerprint = col(0, "model_fingerprint")?;
        let c_chunk_id = col(0, "chunk_id")?;
        let c_lexical_text = col(0, "lexical_text")?;
        let c_char_start = col(0, "char_start")?;
        let c_char_end = col(0, "char_end")?;
        let c_project = col(0, "project")?;
        let c_fragment_type = col(0, "fragment_type")?;
        let c_created_at = col(0, "created_at_millis")?;
        let c_updated_at = col(0, "updated_at_millis")?;
        let c_embedding = col(0, "embedding")?;
        let c_chunker_version = col(0, "chunker_version")?;
        let c_confidence = col(0, "confidence")?;
        for i in 0..batch.num_rows() {
            let s = |idx: usize, name: &'static str| -> DomainResult<String> {
                Ok(batch
                    .column(idx)
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .ok_or_else(|| corrupt(i, name))?
                    .value(i)
                    .to_string())
            };
            let u64c = |idx: usize, name: &'static str| -> DomainResult<u64> {
                Ok(batch
                    .column(idx)
                    .as_any()
                    .downcast_ref::<UInt64Array>()
                    .ok_or_else(|| corrupt(i, name))?
                    .value(i))
            };

            let embedding = match batch.column(c_embedding).data_type() {
                DataType::FixedSizeList(_, size) => {
                    let list = batch
                        .column(c_embedding)
                        .as_any()
                        .downcast_ref::<FixedSizeListArray>()
                        .ok_or_else(|| corrupt(i, "embedding"))?;
                    if list.is_null(i) {
                        None
                    } else {
                        // The child buffer concatenates every row: offset by
                        // this row's start, or every row reads row 0's slice.
                        let flat = Float32Array::from(list.values().to_data());
                        let base = list.value_offset(i) as usize;
                        Some((0..*size as usize).map(|k| flat.value(base + k)).collect())
                    }
                }
                _ => None,
            };

            let memory_id = EntityId::new(
                Uuid::parse_str(&s(c_memory_id, "memory_id")?)
                    .map_err(|_| corrupt(i, "memory_id"))?,
            );
            let project = if batch
                .column(c_project)
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| corrupt(i, "project"))?
                .is_null(i)
            {
                None
            } else {
                Some(s(c_project, "project")?)
            };
            out.push(SearchRow {
                store_generation: StoreGeneration::new(u64c(
                    c_store_generation,
                    "store_generation",
                )?),
                memory_id,
                document_revision: DocumentRevision::new(u64c(
                    c_document_revision,
                    "document_revision",
                )?),
                model_fingerprint: ModelFingerprint::new(u64c(
                    c_model_fingerprint,
                    "model_fingerprint",
                )?),
                chunk_id: ChunkId::new(
                    batch
                        .column(c_chunk_id)
                        .as_any()
                        .downcast_ref::<UInt32Array>()
                        .ok_or_else(|| corrupt(i, "chunk_id"))?
                        .value(i),
                ),
                lexical_text: s(c_lexical_text, "lexical_text")?,
                char_start: u64c(c_char_start, "char_start")?,
                char_end: u64c(c_char_end, "char_end")?,
                project,
                fragment_type: s(c_fragment_type, "fragment_type")?,
                created_at_millis: u64c(c_created_at, "created_at_millis")?,
                updated_at_millis: u64c(c_updated_at, "updated_at_millis")?,
                embedding,
                chunker_version: s(c_chunker_version, "chunker_version")?,
                confidence: batch
                    .column(c_confidence)
                    .as_any()
                    .downcast_ref::<Float64Array>()
                    .ok_or_else(|| corrupt(i, "confidence"))?
                    .value(i),
            });
        }
    }
    Ok(out)
}
