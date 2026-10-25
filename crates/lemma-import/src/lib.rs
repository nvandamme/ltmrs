//! Isolated Lemma `.lemma-backup` importer (Wave B5c).
//!
//! This crate owns the only `rusqlite` edge in the project and builds as a
//! `cdylib` loaded at runtime by the daemon. It never touches Fjall and is
//! excluded from the main workspace so `rusqlite` never enters the main
//! dependency tree.

use std::fmt;
use std::path::Path;

/// Expected Lemma backup envelope marker (detected, never produced).
pub const LEMMA_FORMAT: &str = "lemma-backup";
/// Single supported Lemma envelope version (mirrors the upstream writer).
const ENVELOPE_VERSION: u64 = 1;
/// Highest Lemma database schema this importer reads (upstream 0.21.0).
const SCHEMA_VERSION: u64 = 8;
/// Upper bound for a decoded SQLite image (mirrors the upstream 128 MiB cap).
const MAX_DATABASE_BYTES: u64 = 128 * 1024 * 1024;
/// Upper bound for the envelope file itself (base64 inflates the image).
const MAX_FILE_BYTES: u64 = MAX_DATABASE_BYTES / 3 * 4 + 4096;

/// Import failure (IO, format, integrity — never silent, never a partial read).
#[derive(Debug)]
pub struct ImportError(String);

impl fmt::Display for ImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "lemma import failed: {}", self.0)
    }
}

impl std::error::Error for ImportError {}

/// A validated Lemma envelope: the decoded SQLite image plus its metadata.
struct ValidEnvelope {
    lemma_version: String,
    schema_version: u64,
    image: Vec<u8>,
}

/// Read and validate a `.lemma-backup` file. Mirrors the upstream
/// `loadBackup` checks: regular-file bound, envelope shape, schema range,
/// base64 round-trip stability, SQLite magic, rollback-journal mode bytes
/// and the sha256 over the image — in that order, failing loudly.
fn validate_envelope(path: &Path) -> Result<ValidEnvelope, ImportError> {
    let fail = |m: &str| ImportError(m.to_string());
    let size = std::fs::metadata(path)
        .map_err(|e| fail(&format!("cannot stat {}: {e}", path.display())))?
        .len();
    if size > MAX_FILE_BYTES {
        return Err(fail("backup exceeds the supported size limit"));
    }
    let bytes =
        std::fs::read(path).map_err(|e| fail(&format!("cannot read {}: {e}", path.display())))?;
    let v: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| fail(&format!("not a Lemma backup file: {e}")))?;
    let envelope = |key: &str| v.get(key);
    if envelope("format").and_then(|f| f.as_str()) != Some(LEMMA_FORMAT) {
        return Err(fail(&format!(
            "not a Lemma backup envelope (format {:?}; expected {LEMMA_FORMAT:?})",
            envelope("format").and_then(|f| f.as_str())
        )));
    }
    if envelope("format_version").and_then(|f| f.as_u64()) != Some(ENVELOPE_VERSION) {
        return Err(fail("unsupported Lemma backup format version"));
    }
    let schema_version = envelope("schema_version")
        .and_then(|f| f.as_u64())
        .ok_or_else(|| fail("invalid Lemma backup metadata: missing schema_version"))?;
    if !(1..=SCHEMA_VERSION).contains(&schema_version) {
        return Err(fail(&format!(
            "unsupported Lemma database schema {schema_version}; this importer reads schemas 1-{SCHEMA_VERSION}"
        )));
    }
    let lemma_version = envelope("lemma_version")
        .and_then(|f| f.as_str())
        .filter(|s| s.len() <= 64)
        .ok_or_else(|| fail("invalid Lemma backup metadata: bad lemma_version"))?
        .to_string();
    if envelope("created_at")
        .and_then(|f| f.as_str())
        .is_none_or(|s| s.is_empty())
    {
        return Err(fail("invalid Lemma backup metadata: bad created_at"));
    }
    let expected_sha = envelope("database_sha256")
        .and_then(|f| f.as_str())
        .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| fail("invalid Lemma backup metadata: bad database_sha256"))?;
    let encoded = envelope("database")
        .and_then(|f| f.as_str())
        .ok_or_else(|| fail("invalid Lemma backup metadata: missing database"))?;
    let image = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded)
        .map_err(|e| fail(&format!("invalid Lemma backup database payload: {e}")))?;
    if image.len() < 100 || image.len() as u64 > MAX_DATABASE_BYTES {
        return Err(fail("backup database image has an impossible size"));
    }
    // Base64 must round-trip (canonical encoding, no trailing data games).
    {
        let re = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &image);
        if re != encoded {
            return Err(fail("backup database payload is not canonically encoded"));
        }
    }
    if image[0..16] != *b"SQLite format 3\0" || image[18] != 1 || image[19] != 1 {
        // A raw WAL-mode (2,2) image is refused explicitly, not misread: the
        // envelope format only ever carries rollback-journal (1,1) snapshots
        // (the upstream writer normalizes its private serialized copy; the
        // un-checkpointed frames of a WAL image live outside the image, so
        // importing it would silently drop recent rows).
        if image[0..16] == *b"SQLite format 3\0" && image[18] == 2 && image[19] == 2 {
            return Err(fail(
                "backup database image is in WAL mode: checkpoint the live database and re-export (serialize the live view, as the upstream writer does) instead of enveloping the raw file",
            ));
        }
        return Err(fail(
            "backup database payload is not a readable SQLite image",
        ));
    }
    use sha2::Digest;
    let actual_sha = hex_of(&sha2::Sha256::digest(&image));
    if actual_sha != expected_sha.to_ascii_lowercase() {
        return Err(fail(
            "backup checksum mismatch: database_sha256 does not match the database payload",
        ));
    }
    Ok(ValidEnvelope {
        lemma_version,
        schema_version,
        image,
    })
}

fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// C ABI: the daemon loads this cdylib at runtime and calls these two symbols.
// `unsafe` is confined to this boundary: argument validation, UTF-8
// restoration and panic containment. The returned string is heap-allocated
// and owned by the caller, freed exactly once with
// `ltmrs_lemma_free_string`.
// ---------------------------------------------------------------------------

use std::ffi::{CStr, CString};
use std::os::raw::c_char;

fn error_json(message: &str) -> String {
    serde_json::json!({"ok": false, "error": message}).to_string()
}

/// Read a `.lemma-backup` file and return loss-accounted staging JSON.
///
/// # Safety
/// `path` must be non-null and point to a valid NUL-terminated C string for
/// the duration of the call. The returned pointer is non-null, heap-owned
/// and must be freed exactly once with `ltmrs_lemma_free_string`. Never
/// panics across the boundary: all failures become `{"ok":false}` JSON.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ltmrs_lemma_import_json(path: *const c_char) -> *mut c_char {
    let out = std::panic::catch_unwind(|| {
        if path.is_null() {
            return error_json("lemma import failed: null path");
        }
        // SAFETY: upheld by the caller contract above.
        let path_str = match unsafe { CStr::from_ptr(path) }.to_str() {
            Ok(s) => s,
            Err(_) => return error_json("lemma import failed: path is not valid UTF-8"),
        };
        match import_to_json(Path::new(path_str)) {
            Ok(staging) => staging,
            Err(e) => error_json(&e.to_string()),
        }
    })
    .unwrap_or_else(|_| error_json("lemma import failed: internal panic (caught at ABI boundary)"));
    CString::new(out)
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

/// Free a string returned by `ltmrs_lemma_import_json`.
///
/// # Safety
/// `s` must be either null (no-op) or a pointer previously returned by
/// `ltmrs_lemma_import_json` and not yet freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ltmrs_lemma_free_string(s: *mut c_char) {
    if s.is_null() {
        return;
    }
    // SAFETY: upheld by the caller contract above (single ownership).
    drop(unsafe { CString::from_raw(s) });
}

// ---------------------------------------------------------------------------
// Staging: Lemma rows -> CanonicalExport (+ loss accounting)
// ---------------------------------------------------------------------------
// Column discipline (derived from the pinned upstream DDL): CHECK-constrained
// enum columns fail loudly on unknown values (a genuine backup cannot hold
// them); unconstrained free-text columns (source strings, JSON blobs,
// timestamps) map tolerantly and every alteration is counted as loss.

use std::collections::{BTreeMap, HashMap, HashSet};

use ltmrs_domain::export::CanonicalExport;

/// One counted caveat: dropped rows, defaulted values, folded fields.
#[derive(Debug, Clone, serde::Serialize)]
struct LossEntry {
    kind: String,
    count: u64,
    detail: String,
}

#[derive(Default)]
struct LossLog {
    entries: Vec<LossEntry>,
}

impl LossLog {
    fn add(&mut self, kind: &str, detail: String) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.kind == kind) {
            entry.count += 1;
        } else {
            self.entries.push(LossEntry {
                kind: kind.to_string(),
                count: 1,
                detail,
            });
        }
    }
}

/// Schema version that introduced a table (from the pinned migration list).
fn table_introduced_in(table: &str) -> u64 {
    match table {
        "session_attempts" | "improvement_suggestions" => 2,
        "fragment_history" => 5,
        "fragments_archive" => 6,
        "memory_evidence" => 7,
        "tfidf_cache" => 8,
        _ => 1,
    }
}

/// Schema version that introduced a column (additive migrations only).
fn column_introduced_in(table: &str, column: &str) -> u64 {
    match (table, column) {
        ("memories", "access_window") => 4,
        ("memories", "invalidated_at") => 5,
        _ => 1,
    }
}

fn fail_row(table: &str, id: &str, what: &str) -> ImportError {
    ImportError(format!("lemma {table} row {id}: {what}"))
}

fn table_exists(conn: &rusqlite::Connection, table: &str) -> Result<bool, ImportError> {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?",
        [table],
        |_| Ok(true),
    )
    .or_else(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => Ok(false),
        other => Err(ImportError(format!("cannot inspect lemma schema: {other}"))),
    })
}

fn existing_columns(
    conn: &rusqlite::Connection,
    table: &str,
) -> Result<HashSet<String>, ImportError> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|e| ImportError(format!("cannot inspect lemma table {table}: {e}")))?;
    let cols = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| ImportError(format!("cannot inspect lemma table {table}: {e}")))?
        .collect::<Result<HashSet<_>, _>>()
        .map_err(|e| ImportError(format!("cannot inspect lemma table {table}: {e}")))?;
    Ok(cols)
}

/// Read every row of `table` restricted to `wanted` columns that exist.
/// A missing table is empty when introduced after `schema_version`, corrupt
/// when the version promises it.
fn read_table(
    conn: &rusqlite::Connection,
    schema_version: u64,
    table: &str,
    wanted: &[&str],
) -> Result<Vec<BTreeMap<String, rusqlite::types::Value>>, ImportError> {
    if !table_exists(conn, table)? {
        if table_introduced_in(table) <= schema_version {
            return Err(ImportError(format!(
                "lemma backup is corrupt: table {table} missing at schema {schema_version}"
            )));
        }
        return Ok(Vec::new());
    }
    let present = existing_columns(conn, table)?;
    let cols: Vec<&str> = wanted
        .iter()
        .filter(|c| present.contains(**c) || column_introduced_in(table, c) > schema_version)
        .copied()
        .collect();
    // Every wanted column is either present or introduced later; a column
    // that should exist but does not is a corrupt table.
    for column in wanted {
        if !present.contains(*column) && column_introduced_in(table, column) <= schema_version {
            return Err(ImportError(format!(
                "lemma backup is corrupt: column {table}.{column} missing at schema {schema_version}"
            )));
        }
    }
    let sql = format!(
        "SELECT {} FROM \"{}\" ORDER BY rowid",
        cols.iter()
            .map(|c| format!("\"{c}\""))
            .collect::<Vec<_>>()
            .join(", "),
        table.replace('"', "\"\"")
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| ImportError(format!("cannot read lemma table {table}: {e}")))?;
    let rows = stmt
        .query_map([], |row| {
            let mut map = BTreeMap::new();
            for (i, col) in cols.iter().enumerate() {
                map.insert((*col).to_string(), row.get(i)?);
            }
            Ok(map)
        })
        .map_err(|e| ImportError(format!("cannot read lemma table {table}: {e}")))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| ImportError(format!("cannot read lemma table {table}: {e}")))?;
    Ok(rows)
}

type SqlValue = rusqlite::types::Value;
type Row = BTreeMap<String, SqlValue>;

fn get_i64(row: &Row, table: &str, id: &str, column: &str) -> Result<i64, ImportError> {
    match row.get(column) {
        Some(SqlValue::Integer(n)) => Ok(*n),
        other => Err(fail_row(
            table,
            id,
            &format!("bad integer {column}: {other:?}"),
        )),
    }
}

fn opt_i64(row: &Row, column: &str) -> Option<i64> {
    match row.get(column) {
        Some(SqlValue::Integer(n)) => Some(*n),
        _ => None,
    }
}

fn opt_f64(row: &Row, column: &str) -> Option<f64> {
    match row.get(column) {
        Some(SqlValue::Real(n)) => Some(*n),
        Some(SqlValue::Integer(n)) => Some(*n as f64),
        _ => None,
    }
}

fn opt_text(row: &Row, column: &str) -> Option<String> {
    match row.get(column) {
        Some(SqlValue::Text(s)) => Some(s.clone()),
        _ => None,
    }
}

fn req_text(row: &Row, table: &str, id: &str, column: &str) -> Result<String, ImportError> {
    opt_text(row, column).ok_or_else(|| fail_row(table, id, &format!("missing {column}")))
}

/// Parse a JSON array of strings; unparseable values become empty and are
/// counted under `kind` (the domain field name, stable across upstream
/// column renames).
fn json_strings(row: &Row, column: &str, kind: &str, loss: &mut LossLog) -> Vec<String> {
    let Some(text) = opt_text(row, column) else {
        return Vec::new();
    };
    if text.trim().is_empty() {
        return Vec::new();
    }
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(serde_json::Value::Array(items)) => items
            .into_iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect(),
        _ => {
            loss.add(kind, format!("{column}={text:?}"));
            Vec::new()
        }
    }
}

/// Millis since epoch for upstream timestamps: `datetime('now')`
/// (`YYYY-MM-DD HH:MM:SS`, UTC) and ISO-8601 (`T` separator, optional
/// fractional seconds, `Z` or numeric offset).
fn parse_instant(s: &str) -> Option<u64> {
    let (date_part, time_part) = s.split_once(['T', ' '])?;
    let mut date = date_part.split('-');
    let (year, month, day): (i64, i64, i64) = (
        date.next()?.parse().ok()?,
        date.next()?.parse().ok()?,
        date.next()?.parse().ok()?,
    );
    if date.next().is_some() || !(1970..=2100).contains(&year) {
        return None;
    }
    // Split the numeric time from an optional zone suffix.
    let time_end = time_part.find(['Z', '+', '-']).unwrap_or(time_part.len());
    let (clock, zone) = (&time_part[..time_end], &time_part[time_end..]);
    let mut clock_parts = clock.split(':');
    let (hour, minute, second): (i64, i64, i64) = (
        clock_parts.next()?.parse().ok()?,
        clock_parts.next()?.parse().ok()?,
        clock_parts.next()?.split('.').next()?.parse().ok()?,
    );
    if clock_parts.next().is_some() {
        return None;
    }
    let offset_secs: i64 = if zone.is_empty() || zone == "Z" {
        0
    } else {
        let sign = if zone.starts_with('-') { -1 } else { 1 };
        let digits: String = zone[1..].chars().filter(|c| *c != ':').collect();
        if digits.len() != 4 && digits.len() != 2 {
            return None;
        }
        let hours: i64 = digits[..2].parse().ok()?;
        let minutes: i64 = if digits.len() == 4 {
            digits[2..].parse().ok()?
        } else {
            0
        };
        sign * (hours * 3600 + minutes * 60)
    };
    let days = days_from_civil(year, month, day)?;
    let millis = days * 86_400_000 + hour * 3_600_000 + minute * 60_000 + second * 1_000
        - offset_secs * 1_000;
    u64::try_from(millis).ok()
}

fn days_from_civil(year: i64, month: i64, day: i64) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let adjusted_year = if month <= 2 { year - 1 } else { year };
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    } / 400;
    let year_of_era = adjusted_year - era * 400;
    let month_prime = (month + 9) % 12;
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some(era * 146097 + day_of_era - 719468)
}

fn uuid5(tag: &str, key: &str) -> ltmrs_domain::id::EntityId {
    ltmrs_domain::id::EntityId::new(uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        format!("ltmrs:lemma-import:{tag}:{key}").as_bytes(),
    ))
}

/// Staging builder: one pass per collection, loss aggregated by kind.
struct Staging {
    schema_version: u64,
    loss: LossLog,
    export: CanonicalExport,
    id_map: HashMap<i64, ltmrs_domain::id::EntityId>,
    legacy_map: HashMap<String, ltmrs_domain::id::EntityId>,
    /// Integer row id back to its legacy id (for session link reporting).
    row_legacy: HashMap<i64, String>,
}

impl Staging {
    fn new(schema_version: u64) -> Self {
        Self {
            schema_version,
            loss: LossLog::default(),
            export: CanonicalExport::default(),
            id_map: HashMap::new(),
            legacy_map: HashMap::new(),
            row_legacy: HashMap::new(),
        }
    }

    fn read_all(&mut self, conn: &rusqlite::Connection) -> Result<(), ImportError> {
        self.read_memories(conn)?;
        self.read_relations(conn)?;
        self.read_guides(conn)?;
        self.read_sessions(conn)?;
        self.read_feedback(conn)?;
        self.read_suggestions(conn)?;
        self.read_archives(conn)?;
        self.read_history(conn)?;
        self.read_internal(conn)?;
        Ok(())
    }

    fn read_memories(&mut self, conn: &rusqlite::Connection) -> Result<(), ImportError> {
        use ltmrs_domain::id::{
            DocumentRevision, EligibilityRevision, EntityRevision, ExternalAlias,
        };
        use ltmrs_domain::memory::{FragmentType, Instant, Memory, MemoryLifecycle, MemorySource};
        let rows = read_table(
            conn,
            self.schema_version,
            "memories",
            &[
                "id",
                "legacy_id",
                "title",
                "fragment",
                "description",
                "type",
                "project",
                "source",
                "confidence",
                "context_tags",
                "access_count",
                "last_accessed_at",
                "negative_hits",
                "positive_feedback",
                "negative_feedback",
                "quality_score",
                "refinement_count",
                "parent_id",
                "session_id",
                "task_type",
                "distill_candidate",
                "related_guides",
                "associated_with",
                "last_refined",
                "created_at",
                "updated_at",
                "access_window",
                "invalidated_at",
            ],
        )?;
        // Evidence is a side table keyed by memory id.
        let evidence_rows = read_table(
            conn,
            self.schema_version,
            "memory_evidence",
            &[
                "memory_id",
                "file_path",
                "symbol",
                "snippet",
                "snippet_hash",
                "created_at",
            ],
        )?;
        let mut evidence_by_memory: HashMap<i64, Vec<ltmrs_domain::memory::Evidence>> =
            HashMap::new();
        for row in &evidence_rows {
            let memory_id = get_i64(row, "memory_evidence", "?", "memory_id")?;
            let evidence = ltmrs_domain::memory::Evidence {
                file: req_text(row, "memory_evidence", &memory_id.to_string(), "file_path")?,
                symbol: opt_text(row, "symbol"),
                snippet: req_text(row, "memory_evidence", &memory_id.to_string(), "snippet")?,
                snippet_sha256: req_text(
                    row,
                    "memory_evidence",
                    &memory_id.to_string(),
                    "snippet_hash",
                )?,
            };
            if opt_text(row, "created_at").is_some() {
                // No domain slot for the evidence timestamp: counted, not kept.
                self.loss.add(
                    "dropped_operational_fields",
                    "memory_evidence.created_at".to_string(),
                );
            }
            evidence_by_memory
                .entry(memory_id)
                .or_default()
                .push(evidence);
        }
        // Related-guide links from the link table (unioned with the column).
        let link_rows = read_table(
            conn,
            self.schema_version,
            "guide_memory_links",
            &["guide_id", "memory_id", "link_type"],
        )?;
        let guide_names: HashMap<i64, String> =
            read_table(conn, self.schema_version, "guides", &["id", "guide"])?
                .into_iter()
                .map(|row| {
                    let id = get_i64(&row, "guides", "?", "id")?;
                    let name = req_text(&row, "guides", &id.to_string(), "guide")?;
                    Ok((id, name))
                })
                .collect::<Result<_, ImportError>>()?;
        let mut related_by_memory: HashMap<i64, Vec<String>> = HashMap::new();
        for row in &link_rows {
            if opt_text(row, "link_type").as_deref() != Some("related") {
                continue;
            }
            let memory_id = get_i64(row, "guide_memory_links", "?", "memory_id")?;
            let guide_id = get_i64(
                row,
                "guide_memory_links",
                &memory_id.to_string(),
                "guide_id",
            )?;
            if let Some(name) = guide_names.get(&guide_id) {
                related_by_memory
                    .entry(memory_id)
                    .or_default()
                    .push(name.clone());
            } else {
                self.loss.add(
                    "dangling_guide_link",
                    format!("guide_memory_links guide {guide_id} missing"),
                );
            }
        }
        // First pass: identities and scalar fields.
        struct Partial {
            row_id: i64,
            parent_row_id: Option<i64>,
            associated_raw: Vec<String>,
        }
        let mut partials = Vec::new();
        for row in &rows {
            let row_id = get_i64(row, "memories", "?", "id")?;
            let id = uuid5("memory", &row_id.to_string());
            self.id_map.insert(row_id, id);
            if let Some(legacy) = opt_text(row, "legacy_id") {
                self.legacy_map.insert(legacy.clone(), id);
                self.row_legacy.insert(row_id, legacy);
            }
            let fragment_type = match opt_text(row, "type").as_deref() {
                Some("fact") => FragmentType::Fact,
                Some("pattern") => FragmentType::Pattern,
                Some("lesson") => FragmentType::Lesson,
                Some("warning") => FragmentType::Warning,
                Some("context") => FragmentType::Context,
                other => {
                    return Err(fail_row(
                        "memories",
                        &row_id.to_string(),
                        &format!("unknown fragment type {other:?}"),
                    ));
                }
            };
            // Free-text source: tolerant default with loss (upstream leaves
            // it unconstrained).
            let source = match opt_text(row, "source").as_deref() {
                None | Some("ai") => MemorySource::Ai,
                Some("user") => MemorySource::User,
                Some("web") => MemorySource::Web,
                Some("paper") => MemorySource::Paper,
                Some("book") => MemorySource::Book,
                Some("code") => MemorySource::Code,
                Some(other) => {
                    self.loss
                        .add("unknown_source", format!("memories.source={other:?}"));
                    MemorySource::Ai
                }
            };
            let created_at = parse_instant(&req_text(
                row,
                "memories",
                &row_id.to_string(),
                "created_at",
            )?)
            .ok_or_else(|| fail_row("memories", &row_id.to_string(), "unparseable created_at"))?;
            let updated_at = parse_instant(&req_text(
                row,
                "memories",
                &row_id.to_string(),
                "updated_at",
            )?)
            .ok_or_else(|| fail_row("memories", &row_id.to_string(), "unparseable updated_at"))?;
            let lifecycle = match opt_text(row, "invalidated_at") {
                None => MemoryLifecycle::Live,
                Some(raw) => match parse_instant(&raw) {
                    Some(millis) => MemoryLifecycle::Invalidated {
                        at: Instant::new(millis),
                    },
                    None => {
                        self.loss.add(
                            "unparseable_timestamp",
                            format!("memories.invalidated_at={raw:?}"),
                        );
                        MemoryLifecycle::Live
                    }
                },
            };
            if let Some(window) = opt_i64(row, "access_window")
                && window != 0
            {
                self.loss.add(
                    "dropped_operational_fields",
                    format!("memories.access_window={window}"),
                );
            }
            if opt_text(row, "last_refined").is_some() {
                self.loss.add(
                    "dropped_operational_fields",
                    "memories.last_refined".to_string(),
                );
            }
            let mut related_guides = json_strings(
                row,
                "related_guides",
                "unparseable_related_guides",
                &mut self.loss,
            );
            if let Some(extra) = related_by_memory.get(&row_id) {
                related_guides.extend(extra.iter().cloned());
            }
            related_guides.sort();
            related_guides.dedup();
            let memory = Memory {
                id,
                external_alias: opt_text(row, "legacy_id").map(ExternalAlias::new),
                title: req_text(row, "memories", &row_id.to_string(), "title")?,
                fragment: req_text(row, "memories", &row_id.to_string(), "fragment")?,
                description: opt_text(row, "description").unwrap_or_default(),
                fragment_type,
                project: opt_text(row, "project"),
                project_id: None,
                source,
                confidence: opt_f64(row, "confidence").unwrap_or(0.0),
                quality_score: opt_f64(row, "quality_score"),
                lifecycle,
                tags: json_strings(row, "context_tags", "unparseable_tags", &mut self.loss),
                associated_with: Vec::new(),
                relations: Vec::new(),
                parent_id: None,
                child_ids: Vec::new(),
                session_id: opt_text(row, "session_id"),
                task_type: opt_text(row, "task_type"),
                related_guides,
                evidence: evidence_by_memory.remove(&row_id).unwrap_or_default(),
                access_count: opt_i64(row, "access_count").unwrap_or(0).max(0) as u64,
                last_accessed_at: opt_text(row, "last_accessed_at")
                    .and_then(|raw| {
                        parse_instant(&raw).or_else(|| {
                            self.loss.add(
                                "unparseable_timestamp",
                                format!("memories.last_accessed_at={raw:?}"),
                            );
                            None
                        })
                    })
                    .map(Instant::new),
                positive_feedback: opt_i64(row, "positive_feedback").unwrap_or(0).max(0) as u64,
                negative_feedback: opt_i64(row, "negative_feedback").unwrap_or(0).max(0) as u64,
                negative_hits: opt_i64(row, "negative_hits").unwrap_or(0).max(0) as u64,
                refinement_count: opt_i64(row, "refinement_count").unwrap_or(0).max(0) as u64,
                distill_candidate: opt_i64(row, "distill_candidate").unwrap_or(0) != 0,
                entity_revision: EntityRevision::new(1),
                document_revision: DocumentRevision::new(1),
                eligibility_revision: EligibilityRevision::new(1),
                created_at: Instant::new(created_at),
                updated_at: Instant::new(updated_at),
                raw_created: None,
                unknown_fields: BTreeMap::new(),
            };
            partials.push((
                memory,
                Partial {
                    row_id,
                    parent_row_id: opt_i64(row, "parent_id"),
                    associated_raw: json_strings(
                        row,
                        "associated_with",
                        "unparseable_associated_with",
                        &mut self.loss,
                    ),
                },
            ));
        }
        // Second pass: parent/child and associated links need the full map.
        let mut children: HashMap<ltmrs_domain::id::EntityId, Vec<ltmrs_domain::id::EntityId>> =
            HashMap::new();
        for (memory, partial) in &mut partials {
            if let Some(parent_row) = partial.parent_row_id {
                if let Some(parent_id) = self.id_map.get(&parent_row) {
                    memory.parent_id = Some(*parent_id);
                    children.entry(*parent_id).or_default().push(memory.id);
                } else {
                    self.loss.add(
                        "dangling_memory_link",
                        format!("memories.parent_id={parent_row} missing"),
                    );
                }
            }
            for raw in &partial.associated_raw {
                if let Some(target) = self.legacy_map.get(raw).copied().or_else(|| {
                    raw.parse::<i64>()
                        .ok()
                        .and_then(|n| self.id_map.get(&n).copied())
                }) {
                    memory.associated_with.push(target.to_string());
                } else {
                    self.loss
                        .add("unresolvable_associated_with", format!("{raw:?}"));
                }
            }
        }
        for (memory, partial) in &mut partials {
            if let Some(kids) = children.get(&memory.id) {
                let mut kids = kids.clone();
                kids.sort();
                memory.child_ids = kids;
            }
            let _ = partial.row_id;
        }
        self.export
            .memories
            .extend(partials.into_iter().map(|(memory, _)| memory));
        Ok(())
    }

    fn read_relations(&mut self, conn: &rusqlite::Connection) -> Result<(), ImportError> {
        use ltmrs_domain::memory::Instant;
        use ltmrs_domain::relation::{Relation, RelationType};
        let rows = read_table(
            conn,
            self.schema_version,
            "relations",
            &["id", "source_id", "target_id", "type", "note", "created_at"],
        )?;
        for row in &rows {
            let row_id = get_i64(row, "relations", "?", "id")?;
            // Endpoints derive deterministically even when dangling: the
            // restore quarantine (not the importer) decides what survives.
            let source = uuid5(
                "memory",
                &get_i64(row, "relations", &row_id.to_string(), "source_id")?.to_string(),
            );
            let target = uuid5(
                "memory",
                &get_i64(row, "relations", &row_id.to_string(), "target_id")?.to_string(),
            );
            let relation_type = match opt_text(row, "type").as_deref() {
                Some("supports") => RelationType::Supports,
                Some("contradicts") => RelationType::Contradicts,
                Some("supersedes") => RelationType::Supersedes,
                Some("superseded_by") => RelationType::SupersededBy,
                Some("related_to") => RelationType::RelatedTo,
                other => {
                    return Err(fail_row(
                        "relations",
                        &row_id.to_string(),
                        &format!("unknown relation type {other:?}"),
                    ));
                }
            };
            let created_at = parse_instant(&req_text(
                row,
                "relations",
                &row_id.to_string(),
                "created_at",
            )?)
            .ok_or_else(|| fail_row("relations", &row_id.to_string(), "unparseable created_at"))?;
            self.export.relations.push(Relation {
                id: uuid5("relation", &row_id.to_string()),
                source,
                target,
                relation_type,
                note: opt_text(row, "note"),
                created_at: Instant::new(created_at),
            });
        }
        Ok(())
    }

    fn read_guides(&mut self, conn: &rusqlite::Connection) -> Result<(), ImportError> {
        use ltmrs_domain::id::EntityRevision;
        use ltmrs_domain::memory::Instant;
        let rows = read_table(
            conn,
            self.schema_version,
            "guides",
            &[
                "id",
                "guide",
                "category",
                "description",
                "mission",
                "protocol",
                "anti_patterns",
                "pitfalls",
                "deprecated",
                "superseded_by",
                "usage_count",
                "success_count",
                "failure_count",
                "last_used_at",
                "depends_on",
                "enables",
                "source_memories",
                "validated_by",
                "last_refined",
                "created_at",
                "updated_at",
            ],
        )?;
        let contexts = read_table(
            conn,
            self.schema_version,
            "guide_contexts",
            &["guide_id", "context"],
        )?;
        let learnings = read_table(
            conn,
            self.schema_version,
            "guide_learnings",
            &["guide_id", "learning"],
        )?;
        let memory_links = read_table(
            conn,
            self.schema_version,
            "guide_memory_links",
            &["guide_id", "memory_id", "link_type"],
        )?;
        let mut contexts_by_guide: HashMap<i64, Vec<String>> = HashMap::new();
        for row in &contexts {
            let guide_id = get_i64(row, "guide_contexts", "?", "guide_id")?;
            contexts_by_guide
                .entry(guide_id)
                .or_default()
                .push(req_text(
                    row,
                    "guide_contexts",
                    &guide_id.to_string(),
                    "context",
                )?);
        }
        let mut learnings_by_guide: HashMap<i64, Vec<String>> = HashMap::new();
        for row in &learnings {
            let guide_id = get_i64(row, "guide_learnings", "?", "guide_id")?;
            learnings_by_guide
                .entry(guide_id)
                .or_default()
                .push(req_text(
                    row,
                    "guide_learnings",
                    &guide_id.to_string(),
                    "learning",
                )?);
        }
        for row in &rows {
            let row_id = get_i64(row, "guides", "?", "id")?;
            let name = req_text(row, "guides", &row_id.to_string(), "guide")?;
            // Mission/protocol have no domain slot: fold them into the
            // description with markers (disclosed below, never dropped).
            let mut description = req_text(row, "guides", &row_id.to_string(), "description")?;
            let mission = opt_text(row, "mission").unwrap_or_default();
            let protocol = opt_text(row, "protocol").unwrap_or_default();
            if !mission.trim().is_empty() || !protocol.trim().is_empty() {
                if !mission.trim().is_empty() {
                    description.push_str(&format!("\n\n## Mission\n{mission}"));
                }
                if !protocol.trim().is_empty() {
                    description.push_str(&format!("\n\n## Protocol\n{protocol}"));
                }
                self.loss
                    .add("folded_guide_manual", format!("guide={name:?}"));
            }
            let mut source_memories = Vec::new();
            for legacy in json_strings(
                row,
                "source_memories",
                "unparseable_source_memories",
                &mut self.loss,
            ) {
                match self.legacy_map.get(&legacy) {
                    Some(id) => source_memories.push(*id),
                    None => self.loss.add(
                        "dangling_guide_link",
                        format!("guide={name:?} source_memories={legacy:?} missing"),
                    ),
                }
            }
            let validated_by = json_strings(
                row,
                "validated_by",
                "unparseable_validated_by",
                &mut self.loss,
            );
            for legacy in &validated_by {
                if !self.legacy_map.contains_key(legacy) {
                    self.loss.add(
                        "dangling_guide_link",
                        format!("guide={name:?} validated_by={legacy:?} missing"),
                    );
                }
            }
            for row in &memory_links {
                // guide_id is an INTEGER column: read it as an integer (a
                // text read would always miss and silently drop source links).
                let guide_id = get_i64(row, "guide_memory_links", "?", "guide_id")?;
                if guide_id != row_id {
                    continue;
                }
                if opt_text(row, "link_type").as_deref() == Some("source") {
                    let memory_id = get_i64(
                        row,
                        "guide_memory_links",
                        &guide_id.to_string(),
                        "memory_id",
                    )?;
                    match self.id_map.get(&memory_id) {
                        Some(id) => {
                            if !source_memories.contains(id) {
                                source_memories.push(*id);
                            }
                        }
                        None => self.loss.add(
                            "dangling_guide_link",
                            format!("guide={name:?} source link memory {memory_id} missing"),
                        ),
                    }
                }
            }
            if opt_text(row, "last_refined").is_some() {
                self.loss.add(
                    "dropped_operational_fields",
                    format!("guide={name:?} last_refined"),
                );
            }
            let created_at =
                parse_instant(&req_text(row, "guides", &row_id.to_string(), "created_at")?)
                    .ok_or_else(|| {
                        fail_row("guides", &row_id.to_string(), "unparseable created_at")
                    })?;
            let updated_at =
                parse_instant(&req_text(row, "guides", &row_id.to_string(), "updated_at")?)
                    .ok_or_else(|| {
                        fail_row("guides", &row_id.to_string(), "unparseable updated_at")
                    })?;
            source_memories.sort();
            self.export.guides.push(ltmrs_domain::guide::Guide {
                name: name.clone(),
                category: req_text(row, "guides", &row_id.to_string(), "category")?,
                description,
                contexts: contexts_by_guide.remove(&row_id).unwrap_or_default(),
                learnings: learnings_by_guide.remove(&row_id).unwrap_or_default(),
                usage_count: u32::try_from(opt_i64(row, "usage_count").unwrap_or(0).max(0))
                    .map_err(|_| fail_row("guides", &row_id.to_string(), "bad usage_count"))?,
                last_used: opt_text(row, "last_used_at")
                    .and_then(|raw| {
                        parse_instant(&raw).or_else(|| {
                            self.loss.add(
                                "unparseable_timestamp",
                                format!("guides.last_used_at={raw:?}"),
                            );
                            None
                        })
                    })
                    .map(Instant::new),
                success_count: u32::try_from(opt_i64(row, "success_count").unwrap_or(0).max(0))
                    .map_err(|_| fail_row("guides", &row_id.to_string(), "bad success_count"))?,
                failure_count: u32::try_from(opt_i64(row, "failure_count").unwrap_or(0).max(0))
                    .map_err(|_| fail_row("guides", &row_id.to_string(), "bad failure_count"))?,
                anti_patterns: json_strings(
                    row,
                    "anti_patterns",
                    "unparseable_anti_patterns",
                    &mut self.loss,
                ),
                pitfalls: json_strings(row, "pitfalls", "unparseable_pitfalls", &mut self.loss),
                depends_on: json_strings(
                    row,
                    "depends_on",
                    "unparseable_depends_on",
                    &mut self.loss,
                ),
                enables: json_strings(row, "enables", "unparseable_enables", &mut self.loss),
                source_memories,
                validated_by,
                superseded_by: opt_text(row, "superseded_by"),
                deprecated: opt_i64(row, "deprecated").unwrap_or(0) != 0,
                entity_revision: EntityRevision::new(1),
                created_at: Instant::new(created_at),
                updated_at: Instant::new(updated_at),
            });
        }
        Ok(())
    }

    fn read_sessions(&mut self, conn: &rusqlite::Connection) -> Result<(), ImportError> {
        use ltmrs_domain::id::{ChannelId, SessionHandle};
        use ltmrs_domain::memory::Instant;
        use ltmrs_domain::session::{Attempt, AttemptOutcome, Session, SessionStatus, TaskOutcome};
        let rows = read_table(
            conn,
            self.schema_version,
            "sessions",
            &[
                "id",
                "task_type",
                "technologies",
                "initial_approach",
                "final_approach",
                "approach_changed",
                "outcome",
                "refinement_attempts",
                "self_critique_count",
                "lessons",
                "tool_calls",
                "memory_count",
                "status",
                "project",
                "started_at",
                "ended_at",
            ],
        )?;
        let attempts = read_table(
            conn,
            self.schema_version,
            "session_attempts",
            &[
                "session_id",
                "seq",
                "approach",
                "rationale",
                "outcome",
                "critique",
                "related_memory_id",
                "confidence",
                "last_accessed_at",
                "access_count",
                "created_at",
            ],
        )?;
        let guide_usage = read_table(
            conn,
            self.schema_version,
            "session_guide_usage",
            &["session_id", "guide_id"],
        )?;
        let memory_links = read_table(
            conn,
            self.schema_version,
            "session_memory_links",
            &["session_id", "memory_id", "interaction_type"],
        )?;
        let guide_names: HashMap<i64, String> =
            read_table(conn, self.schema_version, "guides", &["id", "guide"])?
                .into_iter()
                .map(|row| {
                    let id = get_i64(&row, "guides", "?", "id")?;
                    let name = req_text(&row, "guides", &id.to_string(), "guide")?;
                    Ok((id, name))
                })
                .collect::<Result<_, ImportError>>()?;
        for row in &rows {
            let session_id = req_text(row, "sessions", "?", "id")?;
            let handle = SessionHandle::new(uuid::Uuid::new_v5(
                &uuid::Uuid::NAMESPACE_URL,
                format!("ltmrs:lemma-import:session:{session_id}").as_bytes(),
            ));
            let channel = ChannelId::new(uuid::Uuid::new_v5(
                &uuid::Uuid::NAMESPACE_URL,
                format!("ltmrs:lemma-import:session-channel:{session_id}").as_bytes(),
            ));
            // Status/outcome columns are unconstrained upstream: tolerate
            // with loss instead of failing the import.
            let status = match opt_text(row, "status").as_deref() {
                None | Some("active") => SessionStatus::Active,
                Some("completed") => SessionStatus::Ended,
                Some("abandoned") => SessionStatus::Abandoned,
                Some(other) => {
                    self.loss.add(
                        "unknown_session_status",
                        format!("sessions.status={other:?}"),
                    );
                    SessionStatus::Active
                }
            };
            let outcome = match opt_text(row, "outcome").as_deref() {
                None => None,
                Some("success") => Some(TaskOutcome::Success),
                Some("partial") => Some(TaskOutcome::Partial),
                Some("failure") => Some(TaskOutcome::Failure),
                Some("abandoned") => Some(TaskOutcome::Abandoned),
                Some(other) => {
                    self.loss.add(
                        "unknown_session_outcome",
                        format!("sessions.outcome={other:?}"),
                    );
                    None
                }
            };
            for (column, value) in [
                ("tool_calls", opt_i64(row, "tool_calls").unwrap_or(0)),
                ("memory_count", opt_i64(row, "memory_count").unwrap_or(0)),
                (
                    "approach_changed",
                    opt_i64(row, "approach_changed").unwrap_or(0),
                ),
            ] {
                if value != 0 {
                    self.loss.add(
                        "dropped_operational_fields",
                        format!("sessions.{column}={value}"),
                    );
                }
            }
            let started_at = parse_instant(&req_text(row, "sessions", &session_id, "started_at")?)
                .ok_or_else(|| fail_row("sessions", &session_id, "unparseable started_at"))?;
            let ended_at = opt_text(row, "ended_at")
                .and_then(|raw| {
                    parse_instant(&raw).or_else(|| {
                        self.loss.add(
                            "unparseable_timestamp",
                            format!("sessions.ended_at={raw:?}"),
                        );
                        None
                    })
                })
                .map(Instant::new);
            let mut session_attempts = Vec::new();
            for attempt in attempts
                .iter()
                .filter(|r| opt_text(r, "session_id").as_deref() == Some(session_id.as_str()))
            {
                let seq = get_i64(attempt, "session_attempts", &session_id, "seq")?;
                let attempt_outcome = match opt_text(attempt, "outcome").as_deref() {
                    Some("rejected") => AttemptOutcome::Rejected,
                    Some("partial") => AttemptOutcome::Partial,
                    Some("promising") => AttemptOutcome::Promising,
                    other => {
                        return Err(fail_row(
                            "session_attempts",
                            &seq.to_string(),
                            &format!("unknown attempt outcome {other:?}"),
                        ));
                    }
                };
                let related_memory_id = match attempt.get("related_memory_id") {
                    Some(SqlValue::Integer(memory_row)) => match self.id_map.get(memory_row) {
                        Some(id) => Some(*id),
                        None => {
                            self.loss.add(
                                "dangling_attempt_memory",
                                format!("session_attempts related_memory_id={memory_row} missing"),
                            );
                            None
                        }
                    },
                    _ => None,
                };
                let attempt_created = parse_instant(&req_text(
                    attempt,
                    "session_attempts",
                    &seq.to_string(),
                    "created_at",
                )?)
                .ok_or_else(|| {
                    fail_row(
                        "session_attempts",
                        &seq.to_string(),
                        "unparseable created_at",
                    )
                })?;
                session_attempts.push(Attempt {
                    id: uuid5("attempt", &format!("{session_id}:{seq}")),
                    session_id: handle,
                    seq: u32::try_from(seq.max(0))
                        .map_err(|_| fail_row("session_attempts", &seq.to_string(), "bad seq"))?,
                    approach: req_text(attempt, "session_attempts", &seq.to_string(), "approach")?,
                    outcome: attempt_outcome,
                    critique: opt_text(attempt, "critique"),
                    rationale: opt_text(attempt, "rationale"),
                    related_memory_id,
                    confidence: opt_f64(attempt, "confidence").unwrap_or(0.5),
                    access_count: opt_i64(attempt, "access_count").unwrap_or(0).max(0) as u32,
                    last_accessed_at: opt_text(attempt, "last_accessed_at")
                        .and_then(|raw| parse_instant(&raw))
                        .map(Instant::new),
                    created_at: Instant::new(attempt_created),
                });
            }
            session_attempts.sort_by_key(|a| a.seq);
            let mut guides_used = Vec::new();
            for usage in guide_usage
                .iter()
                .filter(|r| opt_text(r, "session_id").as_deref() == Some(session_id.as_str()))
            {
                let guide_id = get_i64(usage, "session_guide_usage", &session_id, "guide_id")?;
                match guide_names.get(&guide_id) {
                    Some(name) => guides_used.push(name.clone()),
                    None => self.loss.add(
                        "dangling_session_guide",
                        format!("session_guide_usage guide {guide_id} missing"),
                    ),
                }
            }
            let mut memories_read = Vec::new();
            let mut memories_created = Vec::new();
            for link in memory_links
                .iter()
                .filter(|r| opt_text(r, "session_id").as_deref() == Some(session_id.as_str()))
            {
                let memory_row = get_i64(link, "session_memory_links", &session_id, "memory_id")?;
                let interaction = opt_text(link, "interaction_type").unwrap_or_default();
                match self.row_legacy.get(&memory_row) {
                    Some(legacy) => match interaction.as_str() {
                        "created" | "updated" => memories_created.push(legacy.clone()),
                        _ => memories_read.push(legacy.clone()),
                    },
                    None => self.loss.add(
                        "dangling_session_memory",
                        format!("session_memory_links memory {memory_row} missing"),
                    ),
                }
            }
            self.export.sessions.push(Session {
                handle,
                channel_id: channel,
                project: opt_text(row, "project"),
                project_id: None,
                task_type: opt_text(row, "task_type"),
                technologies: json_strings(
                    row,
                    "technologies",
                    "unparseable_technologies",
                    &mut self.loss,
                ),
                status,
                attempts: session_attempts,
                outcome,
                final_approach: opt_text(row, "final_approach"),
                lessons: json_strings(row, "lessons", "unparseable_lessons", &mut self.loss),
                initial_approach: opt_text(row, "initial_approach"),
                guides_used,
                memories_read,
                memories_created,
                refinement_attempts: opt_i64(row, "refinement_attempts").unwrap_or(0).max(0) as u32,
                self_critique_count: opt_i64(row, "self_critique_count").unwrap_or(0).max(0) as u32,
                started_at: Instant::new(started_at),
                ended_at,
                is_virtual: false,
            });
        }
        Ok(())
    }

    fn read_feedback(&mut self, conn: &rusqlite::Connection) -> Result<(), ImportError> {
        use ltmrs_domain::memory::Instant;
        use ltmrs_domain::session::FeedbackEvent;
        let rows = read_table(
            conn,
            self.schema_version,
            "feedback_log",
            &["id", "memory_id", "useful", "context", "created_at"],
        )?;
        for row in &rows {
            let row_id = get_i64(row, "feedback_log", "?", "id")?;
            let memory_row = get_i64(row, "feedback_log", &row_id.to_string(), "memory_id")?;
            let Some(memory_id) = self.id_map.get(&memory_row).copied() else {
                self.loss.add(
                    "dangling_feedback_memory",
                    format!("feedback_log memory {memory_row} missing"),
                );
                continue;
            };
            let useful = match get_i64(row, "feedback_log", &row_id.to_string(), "useful")? {
                0 => false,
                1 => true,
                other => {
                    return Err(fail_row(
                        "feedback_log",
                        &row_id.to_string(),
                        &format!("bad useful flag {other}"),
                    ));
                }
            };
            if opt_text(row, "context").is_some() {
                self.loss.add(
                    "dropped_operational_fields",
                    "feedback_log.context".to_string(),
                );
            }
            let timestamp = parse_instant(&req_text(
                row,
                "feedback_log",
                &row_id.to_string(),
                "created_at",
            )?)
            .ok_or_else(|| {
                fail_row(
                    "feedback_log",
                    &row_id.to_string(),
                    "unparseable created_at",
                )
            })?;
            self.export.feedback.push(FeedbackEvent {
                id: uuid5("feedback", &row_id.to_string()),
                memory_id,
                useful,
                timestamp: Instant::new(timestamp),
            });
        }
        Ok(())
    }

    fn read_suggestions(&mut self, conn: &rusqlite::Connection) -> Result<(), ImportError> {
        use ltmrs_domain::memory::Instant;
        use ltmrs_domain::session::{Suggestion, SuggestionStatus};
        let rows = read_table(
            conn,
            self.schema_version,
            "improvement_suggestions",
            &[
                "id",
                "session_id",
                "suggestion",
                "status",
                "created_at",
                "resolved_at",
            ],
        )?;
        for row in &rows {
            let row_id = get_i64(row, "improvement_suggestions", "?", "id")?;
            let status = match opt_text(row, "status").as_deref() {
                Some("offered") => SuggestionStatus::Pending,
                Some("accepted") => SuggestionStatus::Accepted,
                Some("dismissed") => SuggestionStatus::Dismissed,
                other => {
                    return Err(fail_row(
                        "improvement_suggestions",
                        &row_id.to_string(),
                        &format!("unknown status {other:?}"),
                    ));
                }
            };
            let created_at = parse_instant(&req_text(
                row,
                "improvement_suggestions",
                &row_id.to_string(),
                "created_at",
            )?)
            .ok_or_else(|| {
                fail_row(
                    "improvement_suggestions",
                    &row_id.to_string(),
                    "unparseable created_at",
                )
            })?;
            let resolved_at = opt_text(row, "resolved_at")
                .and_then(|raw| {
                    parse_instant(&raw).or_else(|| {
                        self.loss.add(
                            "unparseable_timestamp",
                            format!("improvement_suggestions.resolved_at={raw:?}"),
                        );
                        None
                    })
                })
                .map(Instant::new);
            self.export.suggestions.push(Suggestion {
                id: u64::try_from(row_id).map_err(|_| {
                    fail_row("improvement_suggestions", &row_id.to_string(), "bad id")
                })?,
                session_id: opt_text(row, "session_id"),
                suggestion: req_text(
                    row,
                    "improvement_suggestions",
                    &row_id.to_string(),
                    "suggestion",
                )?,
                status,
                created_at: Instant::new(created_at),
                resolved_at,
            });
        }
        Ok(())
    }

    fn read_archives(&mut self, conn: &rusqlite::Connection) -> Result<(), ImportError> {
        use ltmrs_domain::id::ExternalAlias;
        use ltmrs_domain::memory::{ArchivedFragment, FragmentType, Instant, MemorySource};
        let rows = read_table(
            conn,
            self.schema_version,
            "fragments_archive",
            &[
                "id",
                "legacy_id",
                "title",
                "fragment",
                "description",
                "type",
                "project",
                "confidence",
                "source",
                "context_tags",
                "created_at",
                "heat",
                "archived_at",
            ],
        )?;
        for row in &rows {
            let row_id = get_i64(row, "fragments_archive", "?", "id")?;
            let fragment_type = match opt_text(row, "type").as_deref() {
                Some("fact") => FragmentType::Fact,
                Some("pattern") => FragmentType::Pattern,
                Some("lesson") => FragmentType::Lesson,
                Some("warning") => FragmentType::Warning,
                Some("context") => FragmentType::Context,
                other => {
                    return Err(fail_row(
                        "fragments_archive",
                        &row_id.to_string(),
                        &format!("unknown fragment type {other:?}"),
                    ));
                }
            };
            let source = match opt_text(row, "source").as_deref() {
                None | Some("ai") => MemorySource::Ai,
                Some("user") => MemorySource::User,
                Some("web") => MemorySource::Web,
                Some("paper") => MemorySource::Paper,
                Some("book") => MemorySource::Book,
                Some("code") => MemorySource::Code,
                Some(other) => {
                    self.loss.add(
                        "unknown_source",
                        format!("fragments_archive.source={other:?}"),
                    );
                    MemorySource::Ai
                }
            };
            let archived_at = parse_instant(&req_text(
                row,
                "fragments_archive",
                &row_id.to_string(),
                "archived_at",
            )?)
            .ok_or_else(|| {
                fail_row(
                    "fragments_archive",
                    &row_id.to_string(),
                    "unparseable archived_at",
                )
            })?;
            self.export.archives.push(ArchivedFragment {
                id: uuid5("archive", &row_id.to_string()),
                legacy_id: opt_text(row, "legacy_id").map(ExternalAlias::new),
                title: req_text(row, "fragments_archive", &row_id.to_string(), "title")?,
                fragment: req_text(row, "fragments_archive", &row_id.to_string(), "fragment")?,
                description: opt_text(row, "description"),
                fragment_type,
                project: opt_text(row, "project"),
                confidence: opt_f64(row, "confidence").unwrap_or(0.0),
                source,
                tags: json_strings(row, "context_tags", "unparseable_tags", &mut self.loss),
                created_at: opt_text(row, "created_at")
                    .and_then(|raw| {
                        parse_instant(&raw).or_else(|| {
                            self.loss.add(
                                "unparseable_timestamp",
                                format!("fragments_archive.created_at={raw:?}"),
                            );
                            None
                        })
                    })
                    .map(Instant::new),
                heat: opt_f64(row, "heat"),
                archived_at: Instant::new(archived_at),
            });
        }
        Ok(())
    }

    fn read_history(&mut self, conn: &rusqlite::Connection) -> Result<(), ImportError> {
        use ltmrs_domain::memory::{FragmentHistory, FragmentType, Instant};
        let rows = read_table(
            conn,
            self.schema_version,
            "fragment_history",
            &[
                "id",
                "memory_id",
                "title",
                "fragment",
                "description",
                "confidence",
                "type",
                "changed_at",
            ],
        )?;
        for row in &rows {
            let row_id = get_i64(row, "fragment_history", "?", "id")?;
            let memory_row = get_i64(row, "fragment_history", &row_id.to_string(), "memory_id")?;
            let Some(memory_id) = self.id_map.get(&memory_row).copied() else {
                self.loss.add(
                    "dangling_history_memory",
                    format!("fragment_history memory {memory_row} missing"),
                );
                continue;
            };
            let fragment_type = match opt_text(row, "type").as_deref() {
                Some("fact") => FragmentType::Fact,
                Some("pattern") => FragmentType::Pattern,
                Some("lesson") => FragmentType::Lesson,
                Some("warning") => FragmentType::Warning,
                Some("context") => FragmentType::Context,
                other => {
                    return Err(fail_row(
                        "fragment_history",
                        &row_id.to_string(),
                        &format!("unknown fragment type {other:?}"),
                    ));
                }
            };
            let changed_at = parse_instant(&req_text(
                row,
                "fragment_history",
                &row_id.to_string(),
                "changed_at",
            )?)
            .ok_or_else(|| {
                fail_row(
                    "fragment_history",
                    &row_id.to_string(),
                    "unparseable changed_at",
                )
            })?;
            self.export.history.push(FragmentHistory {
                id: uuid5("history", &row_id.to_string()),
                memory_id,
                title: req_text(row, "fragment_history", &row_id.to_string(), "title")?,
                fragment: req_text(row, "fragment_history", &row_id.to_string(), "fragment")?,
                description: opt_text(row, "description"),
                confidence: opt_f64(row, "confidence").unwrap_or(0.0),
                fragment_type,
                changed_at: Instant::new(changed_at),
            });
        }
        Ok(())
    }

    /// Internal bookkeeping: schema-version rows are counted (not knowledge);
    /// the TF-IDF cache is droppable by design (recomputable); genuinely
    /// unknown tables are counted, never mapped.
    fn read_internal(&mut self, conn: &rusqlite::Connection) -> Result<(), ImportError> {
        const KNOWN: &[&str] = &[
            "schema_version",
            "memories",
            "relations",
            "guides",
            "guide_contexts",
            "guide_learnings",
            "guide_memory_links",
            "sessions",
            "session_attempts",
            "session_guide_usage",
            "session_memory_links",
            "feedback_log",
            "improvement_suggestions",
            "fragments_archive",
            "fragment_history",
            "memory_evidence",
            "tfidf_cache",
            "memory_fts",
            "guides_fts",
        ];
        for table in KNOWN {
            if !table_exists(conn, table)? {
                continue;
            }
            match *table {
                "schema_version" => {
                    let count: i64 = conn
                        .query_row("SELECT count(*) FROM \"schema_version\"", [], |r| r.get(0))
                        .map_err(|e| ImportError(format!("cannot read schema_version: {e}")))?;
                    if count > 0 {
                        for _ in 0..count {
                            self.loss
                                .add("internal_schema_version", "version-history row".to_string());
                        }
                    }
                }
                "tfidf_cache" => {
                    let count: i64 = conn
                        .query_row("SELECT count(*) FROM \"tfidf_cache\"", [], |r| r.get(0))
                        .map_err(|e| ImportError(format!("cannot read tfidf_cache: {e}")))?;
                    for _ in 0..count.max(0) {
                        self.loss.add(
                            "dropped_tfidf_cache",
                            "recomputable vector cache".to_string(),
                        );
                    }
                }
                _ => {}
            }
        }
        // Unknown tables (and FTS shadow tables, which are derived indexes):
        // FTS shadows are derived and ignored; anything else is counted.
        let mut stmt = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            )
            .map_err(|e| ImportError(format!("cannot list lemma tables: {e}")))?;
        let names = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| ImportError(format!("cannot list lemma tables: {e}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| ImportError(format!("cannot list lemma tables: {e}")))?;
        for name in names {
            if KNOWN.contains(&name.as_str())
                || name.starts_with("memory_fts_")
                || name.starts_with("guides_fts_")
            {
                continue;
            }
            let count: i64 = conn
                .query_row(
                    &format!("SELECT count(*) FROM \"{}\"", name.replace('"', "\"\"")),
                    [],
                    |r| r.get(0),
                )
                .map_err(|e| ImportError(format!("cannot count lemma table {name}: {e}")))?;
            for _ in 0..count.max(0) {
                self.loss.add("unknown_table", format!("table={name:?}"));
            }
        }
        Ok(())
    }

    fn finish(self, envelope: &ValidEnvelope) -> String {
        let export = &self.export;
        let mut counts = BTreeMap::new();
        counts.insert("memories".to_string(), export.memories.len() as u64);
        counts.insert("relations".to_string(), export.relations.len() as u64);
        counts.insert("guides".to_string(), export.guides.len() as u64);
        counts.insert("sessions".to_string(), export.sessions.len() as u64);
        counts.insert("feedback".to_string(), export.feedback.len() as u64);
        counts.insert("suggestions".to_string(), export.suggestions.len() as u64);
        let mut projects = std::collections::BTreeSet::new();
        for memory in &export.memories {
            if let Some(project) = &memory.project {
                projects.insert(project.clone());
            }
        }
        for session in &export.sessions {
            if let Some(project) = &session.project {
                projects.insert(project.clone());
            }
        }
        let projects: Vec<ltmrs_domain::project::Project> = projects
            .into_iter()
            .map(|name| ltmrs_domain::project::Project {
                id: uuid5("project", &name),
                legacy_name: Some(name.clone()),
                is_global: false,
                name,
            })
            .collect();
        counts.insert("projects".to_string(), projects.len() as u64);
        counts.insert("archives".to_string(), export.archives.len() as u64);
        counts.insert("history".to_string(), export.history.len() as u64);
        let mut export = self.export;
        export.projects = projects;
        let digest = export.digest();
        serde_json::json!({
            "ok": true,
            "lemma_version": envelope.lemma_version,
            "schema_version": envelope.schema_version,
            "digest": digest,
            "counts": counts,
            "snapshot": export,
            "loss": self.loss.entries,
        })
        .to_string()
    }
}

/// Import a `.lemma-backup` file into loss-accounted staging JSON shaped
/// like the interchange snapshot. The source file is only read: the image
/// is copied to a mode-0600 temp file opened `SQLITE_OPEN_READ_ONLY` and
/// deleted on return.
pub fn import_to_json(path: &Path) -> Result<String, ImportError> {
    let envelope = validate_envelope(path)?;
    let mut staging = Staging::new(envelope.schema_version);
    let tmp = tempfile::NamedTempFile::new()
        .map_err(|e| ImportError(format!("cannot stage lemma image: {e}")))?;
    std::fs::write(tmp.path(), &envelope.image)
        .map_err(|e| ImportError(format!("cannot stage lemma image: {e}")))?;
    let conn = rusqlite::Connection::open_with_flags(
        tmp.path(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(|e| ImportError(format!("cannot open lemma image read-only: {e}")))?;
    staging.read_all(&conn)?;
    Ok(staging.finish(&envelope))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::import_to_json;
    /// Non-lemma envelopes are refused with a naming error (never silently
    /// converted, never relabeled).
    #[test]
    fn rejects_non_lemma_envelope() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.ltmrs-backup");
        std::fs::write(
            &path,
            r#"{"format":"ltmrs-backup","format_version":1,"snapshot":{}}"#,
        )
        .unwrap();
        let err = import_to_json(Path::new(&path)).unwrap_err();
        assert!(
            err.to_string().contains("lemma-backup"),
            "must name the expected format, got: {err}"
        );
    }

    /// Schema versions outside the supported 1..=8 range fail explicitly
    /// (never migrated silently).
    #[test]
    fn rejects_unsupported_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("future.lemma-backup");
        std::fs::write(
            &path,
            r#"{"format":"lemma-backup","format_version":1,"lemma_version":"0.21.0","schema_version":99,"created_at":"2026-01-01T00:00:00.000Z","database_sha256":"abc","database":"e30="}"#,
        )
        .unwrap();
        let err = import_to_json(Path::new(&path)).unwrap_err();
        assert!(
            err.to_string().contains("99"),
            "must name the offending version, got: {err}"
        );
    }

    /// A payload whose checksum or SQLite identity does not verify fails
    /// (never a partial parse).
    #[test]
    fn rejects_tampered_payload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tampered.lemma-backup");
        // Valid envelope shape, but the sha256 does not match the payload.
        std::fs::write(
            &path,
            r#"{"format":"lemma-backup","format_version":1,"lemma_version":"0.21.0","schema_version":8,"created_at":"2026-01-01T00:00:00.000Z","database_sha256":"0000000000000000000000000000000000000000000000000000000000000000","database":"e30="}"#,
        )
        .unwrap();
        let err = import_to_json(Path::new(&path)).unwrap_err();
        assert!(
            err.to_string().contains("checksum")
                || err.to_string().contains("sha256")
                || err.to_string().contains("database"),
            "must blame the payload, got: {err}"
        );
    }

    /// The ABI never crashes on null: it yields an error envelope instead.
    #[test]
    fn abi_null_path_yields_error_json() {
        use crate::{ltmrs_lemma_free_string, ltmrs_lemma_import_json};
        let ptr = unsafe { ltmrs_lemma_import_json(std::ptr::null()) };
        assert!(!ptr.is_null(), "ABI must return a readable string");
        let text = unsafe { std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned() };
        unsafe { ltmrs_lemma_free_string(ptr) };
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["ok"], false);
    }

    /// A missing source file becomes an error envelope naming the path.
    #[test]
    fn abi_missing_file_yields_error_json() {
        use crate::{ltmrs_lemma_free_string, ltmrs_lemma_import_json};
        let arg = std::ffi::CString::new("/nonexistent/x.lemma-backup").unwrap();
        let ptr = unsafe { ltmrs_lemma_import_json(arg.as_ptr()) };
        assert!(!ptr.is_null());
        let text = unsafe { std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned() };
        unsafe { ltmrs_lemma_free_string(ptr) };
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["ok"], false);
        assert!(
            v["error"].as_str().unwrap().contains("x.lemma-backup"),
            "must name the path, got: {text}"
        );
    }

    /// Envelope failures travel the ABI as error JSON (plumbing check).
    #[test]
    fn abi_tampered_envelope_yields_error_json() {
        use crate::{ltmrs_lemma_free_string, ltmrs_lemma_import_json};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tampered.lemma-backup");
        std::fs::write(
            &path,
            r#"{"format":"lemma-backup","format_version":1,"lemma_version":"0.21.0","schema_version":8,"created_at":"2026-01-01T00:00:00.000Z","database_sha256":"0000000000000000000000000000000000000000000000000000000000000000","database":"e30="}"#,
        )
        .unwrap();
        let arg = std::ffi::CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let ptr = unsafe { ltmrs_lemma_import_json(arg.as_ptr()) };
        assert!(!ptr.is_null());
        let text = unsafe { std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned() };
        unsafe { ltmrs_lemma_free_string(ptr) };
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["ok"], false);
    }

    /// Freeing null is a no-op (callers may free unconditionally).
    #[test]
    fn abi_free_null_is_noop() {
        use crate::ltmrs_lemma_free_string;
        unsafe { ltmrs_lemma_free_string(std::ptr::null_mut()) };
    }
}
