//! Arrow schemas for LanceDB tables.

use arrow_schema::{DataType, Field, Schema};

pub fn memories_schema() -> Schema {
    Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("external_alias", DataType::Utf8, true),
        Field::new("title", DataType::Utf8, false),
        Field::new("fragment", DataType::Utf8, false),
        Field::new("description", DataType::Utf8, false),
        Field::new("fragment_type", DataType::Utf8, false),
        Field::new("project", DataType::Utf8, true),
        Field::new("source", DataType::Utf8, false),
        Field::new("confidence", DataType::Float64, false),
        Field::new("quality_score", DataType::Float64, true),
        Field::new("lifecycle", DataType::Utf8, false),
        Field::new("tags", DataType::Utf8, false),
        Field::new("parent_id", DataType::Utf8, true),
        Field::new("session_id", DataType::Utf8, true),
        Field::new("task_type", DataType::Utf8, true),
        Field::new("related_guides", DataType::Utf8, false),
        Field::new("access_count", DataType::UInt64, false),
        Field::new("last_accessed_at", DataType::UInt64, true),
        Field::new("positive_feedback", DataType::UInt64, false),
        Field::new("negative_feedback", DataType::UInt64, false),
        Field::new("negative_hits", DataType::UInt64, false),
        Field::new("refinement_count", DataType::UInt64, false),
        Field::new("distill_candidate", DataType::Boolean, false),
        Field::new("entity_revision", DataType::UInt64, false),
        Field::new("document_revision", DataType::UInt64, false),
        Field::new("eligibility_revision", DataType::UInt64, false),
        Field::new("created_at", DataType::UInt64, false),
        Field::new("updated_at", DataType::UInt64, false),
        Field::new("raw_created", DataType::Utf8, true),
        Field::new("unknown_fields", DataType::Utf8, false),
    ])
}

pub fn relations_schema() -> Schema {
    Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("source_id", DataType::Utf8, false),
        Field::new("target_id", DataType::Utf8, false),
        Field::new("relation_type", DataType::Utf8, false),
        Field::new("note", DataType::Utf8, true),
        Field::new("created_at", DataType::UInt64, false),
    ])
}

pub fn receipts_schema() -> Schema {
    Schema::new(vec![
        Field::new("store_generation", DataType::UInt64, false),
        Field::new("operation_id", DataType::Utf8, false),
        Field::new("frontend_id", DataType::Utf8, false),
        Field::new("channel_id", DataType::Utf8, false),
        Field::new("request_digest", DataType::Utf8, false),
        Field::new("outcome", DataType::Utf8, false),
        Field::new("affected_ids", DataType::Utf8, false),
    ])
}
