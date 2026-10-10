//! Interpreter guide-mutation tests (moved verbatim from `interpreter.rs`).

use super::ReferenceInterpreter;
use super::test_support::*;
use crate::command::DomainCommand;
use crate::id::EntityRevision;

#[test]

fn guide_merge_removes_sources_and_forget_removes() {
    use crate::guide::Guide;

    use crate::memory::Instant;

    let mut it = ReferenceInterpreter::new(1, 0);

    for (i, g) in ["react", "hooks"].iter().enumerate() {
        it.apply(
            &ctx(opid(i as u64 + 1), &format!("g{}", i)),
            &DomainCommand::GuidePractice {
                guide: g.to_string(),

                category: "web".into(),

                contexts: vec![],

                learnings: vec![],

                outcome: None,
            },
        )
        .unwrap();
    }

    // Merge react + hooks into "react-complete".

    let now = it.clock.now_millis();

    let result = Guide {
        name: "react-complete".into(),

        category: "web-frontend".into(),

        description: String::new(),

        contexts: vec![],

        learnings: vec![],

        usage_count: 0,

        last_used: None,

        success_count: 0,

        failure_count: 0,

        anti_patterns: vec![],

        pitfalls: vec![],

        depends_on: vec![],

        enables: vec![],

        source_memories: vec![],

        validated_by: vec![],

        superseded_by: None,

        deprecated: false,

        entity_revision: EntityRevision::new(1),

        created_at: Instant::new(now),

        updated_at: Instant::new(now),
    };

    it.apply(
        &ctx(opid(10), "gm"),
        &DomainCommand::GuideMerge {
            source_names: vec!["react".into(), "hooks".into()],

            result,
        },
    )
    .unwrap();

    // Sources removed, merged guide present.

    assert!(!it.guides.contains_key("react"));

    assert!(!it.guides.contains_key("hooks"));

    assert!(it.guides.contains_key("react-complete"));

    // Forget the merged guide.

    it.apply(
        &ctx(opid(11), "gf"),
        &DomainCommand::GuideForget {
            name: "react-complete".into(),
        },
    )
    .unwrap();

    assert!(!it.guides.contains_key("react-complete"));
}
