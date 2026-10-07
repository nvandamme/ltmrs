//! Legacy field mapping and classification.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldDestination {
    Canonical,
    Envelope,
    Derived,
    LossReport,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FieldMapping {
    pub legacy_field: &'static str,
    pub destination: FieldDestination,
    pub canonical_field: Option<&'static str>,
    pub notes: Option<&'static str>,
}

pub const MEMORY_FIELDS: &[FieldMapping] = &[
    FieldMapping {
        legacy_field: "id",
        destination: FieldDestination::Canonical,
        canonical_field: Some("id"),
        notes: None,
    },
    FieldMapping {
        legacy_field: "title",
        destination: FieldDestination::Canonical,
        canonical_field: Some("title"),
        notes: None,
    },
    FieldMapping {
        legacy_field: "fragment",
        destination: FieldDestination::Canonical,
        canonical_field: Some("fragment"),
        notes: None,
    },
    FieldMapping {
        legacy_field: "description",
        destination: FieldDestination::Canonical,
        canonical_field: Some("description"),
        notes: None,
    },
    FieldMapping {
        legacy_field: "type",
        destination: FieldDestination::Canonical,
        canonical_field: Some("fragment_type"),
        notes: None,
    },
    FieldMapping {
        legacy_field: "project",
        destination: FieldDestination::Canonical,
        canonical_field: Some("project"),
        notes: None,
    },
    FieldMapping {
        legacy_field: "confidence",
        destination: FieldDestination::Canonical,
        canonical_field: Some("confidence"),
        notes: None,
    },
    FieldMapping {
        legacy_field: "quality_score",
        destination: FieldDestination::Canonical,
        canonical_field: Some("quality_score"),
        notes: Some("Optional; unknown stays None, not zero"),
    },
    FieldMapping {
        legacy_field: "source",
        destination: FieldDestination::Canonical,
        canonical_field: Some("source"),
        notes: Some("Upstream enum preserved verbatim"),
    },
    FieldMapping {
        legacy_field: "tags",
        destination: FieldDestination::Canonical,
        canonical_field: Some("tags"),
        notes: None,
    },
    FieldMapping {
        legacy_field: "created_at",
        destination: FieldDestination::Canonical,
        canonical_field: Some("created_at"),
        notes: None,
    },
    FieldMapping {
        legacy_field: "updated_at",
        destination: FieldDestination::Canonical,
        canonical_field: Some("updated_at"),
        notes: None,
    },
    FieldMapping {
        legacy_field: "raw_created",
        destination: FieldDestination::Canonical,
        canonical_field: Some("raw_created"),
        notes: Some("Preserved when normalization would lose precision"),
    },
];

pub const GUIDE_FIELDS: &[FieldMapping] = &[
    FieldMapping {
        legacy_field: "name",
        destination: FieldDestination::Canonical,
        canonical_field: Some("name"),
        notes: None,
    },
    FieldMapping {
        legacy_field: "category",
        destination: FieldDestination::Canonical,
        canonical_field: Some("category"),
        notes: None,
    },
    FieldMapping {
        legacy_field: "description",
        destination: FieldDestination::Canonical,
        canonical_field: Some("description"),
        notes: None,
    },
    FieldMapping {
        legacy_field: "contexts",
        destination: FieldDestination::Canonical,
        canonical_field: Some("contexts"),
        notes: None,
    },
    FieldMapping {
        legacy_field: "learnings",
        destination: FieldDestination::Canonical,
        canonical_field: Some("learnings"),
        notes: None,
    },
    FieldMapping {
        legacy_field: "usage_count",
        destination: FieldDestination::Canonical,
        canonical_field: Some("usage_count"),
        notes: None,
    },
    FieldMapping {
        legacy_field: "last_used",
        destination: FieldDestination::Canonical,
        canonical_field: Some("last_used"),
        notes: None,
    },
    FieldMapping {
        legacy_field: "success_count",
        destination: FieldDestination::Canonical,
        canonical_field: Some("success_count"),
        notes: None,
    },
    FieldMapping {
        legacy_field: "failure_count",
        destination: FieldDestination::Canonical,
        canonical_field: Some("failure_count"),
        notes: None,
    },
    FieldMapping {
        legacy_field: "anti_patterns",
        destination: FieldDestination::Canonical,
        canonical_field: Some("anti_patterns"),
        notes: None,
    },
    FieldMapping {
        legacy_field: "pitfalls",
        destination: FieldDestination::Canonical,
        canonical_field: Some("pitfalls"),
        notes: None,
    },
    FieldMapping {
        legacy_field: "depends_on",
        destination: FieldDestination::Canonical,
        canonical_field: Some("depends_on"),
        notes: None,
    },
    FieldMapping {
        legacy_field: "enables",
        destination: FieldDestination::Canonical,
        canonical_field: Some("enables"),
        notes: None,
    },
    FieldMapping {
        legacy_field: "superseded_by",
        destination: FieldDestination::Canonical,
        canonical_field: Some("superseded_by"),
        notes: None,
    },
    FieldMapping {
        legacy_field: "deprecated",
        destination: FieldDestination::Canonical,
        canonical_field: Some("deprecated"),
        notes: None,
    },
];

pub fn all_mappings() -> Vec<&'static FieldMapping> {
    MEMORY_FIELDS.iter().chain(GUIDE_FIELDS.iter()).collect()
}

pub fn classify_unknown_field(_field: &str) -> FieldDestination {
    FieldDestination::Envelope
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_known_field_has_a_destination() {
        for m in all_mappings() {
            assert!(
                !matches!(m.destination, FieldDestination::LossReport)
                    || m.legacy_field == "outcome",
                "unexpected loss report for {}",
                m.legacy_field
            );
        }
    }

    #[test]
    fn unknown_fields_go_to_envelope() {
        assert_eq!(
            classify_unknown_field("some_future_field"),
            FieldDestination::Envelope
        );
    }

    #[test]
    fn quality_score_is_canonical_optional() {
        let m = MEMORY_FIELDS
            .iter()
            .find(|m| m.legacy_field == "quality_score")
            .unwrap();
        assert_eq!(m.destination, FieldDestination::Canonical);
        assert_eq!(m.canonical_field, Some("quality_score"));
    }
}
