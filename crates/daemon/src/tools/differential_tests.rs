//! Differential wire-contract tests vs the upstream Lemma 0.21.0 oracle (moved verbatim from `tools.rs`).

use super::recall::{render_detail, render_summary_index};
use super::test_support::*;
use crate::envelope::DomainPayload;
use ltmrs_compat::lemma::tool_args::{
    GuideCreateArgs, GuidePracticeArgs, MemoryAddArgs, MemoryReadArgs, MemoryStatsArgs,
    SessionEndArgs, SessionStartArgs, ToolArgs,
};
use ltmrs_domain::id::EntityId;
use ltmrs_domain::memory::{FragmentType, Instant, Memory, MemorySource};
use serde_json::Value;
use uuid::Uuid;

// ---- Differential wire-contract tests (T-MCP-02 legacy_oracle) ----
//
// These replay an anonymized fixture derived from the real upstream
// Lemma 0.21.0 database (structure preserved: legacy IDs, relations,
// confidence, dates, counts, projects, tags; private text replaced).
// The upstream rendering of that data is the oracle reference, captured
// by running the pinned upstream code. ltmrs must reproduce it
// byte-for-byte.

fn fixture_path(name: &str) -> std::path::PathBuf {
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("../../tests/compat/lemma_0_21_0");
    p.push(name);
    p
}

/// Parse a "YYYY-MM-DD" date into UTC epoch millis.
fn parse_date_only(s: &str) -> u64 {
    let parts: Vec<&str> = s.split('-').collect();
    let y: i64 = parts[0].parse().unwrap();
    let m: i64 = parts[1].parse().unwrap();
    let d: i64 = parts[2].parse().unwrap();
    // Days from civil date (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + (mp * 306 + 5) / 10 + (d - 1);
    let doe = doe.max(0);
    (era * 146_097 + doe - 719_468) as u64 * 86_400_000
}

fn fixture_memory(rec: &Value, id_map: &std::collections::HashMap<String, EntityId>) -> Memory {
    let f = &rec["fields"];
    let eid = id_map[&f["id"].as_str().unwrap().to_string()];
    let created_millis = parse_date_only(f["created"].as_str().unwrap());
    Memory {
        id: eid,
        external_alias: Some(ltmrs_domain::id::ExternalAlias::new(
            f["id"].as_str().unwrap().to_string(),
        )),
        title: f["title"].as_str().unwrap_or("").to_string(),
        fragment: f["fragment"].as_str().unwrap_or("").to_string(),
        description: f["description"].as_str().unwrap_or("").to_string(),
        fragment_type: FragmentType::Fact,
        project: f["project"].as_str().map(|s| s.to_string()),
        // Upstream icon: "ai" → 🤖, anything else → 👤. Map accordingly.
        source: if f["source"].as_str() == Some("ai") {
            MemorySource::Ai
        } else {
            MemorySource::User
        },
        confidence: f["confidence"].as_f64().unwrap_or(0.5),
        quality_score: None,
        lifecycle: ltmrs_domain::memory::MemoryLifecycle::Live,
        tags: f["tags"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|t| t.as_str().unwrap_or("").to_string())
                    .collect()
            })
            .unwrap_or_default(),
        associated_with: f["associatedWith"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|t| t.as_str().unwrap_or("").to_string())
                    .collect()
            })
            .unwrap_or_default(),
        relations: f["relations"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|r| {
                        let target = id_map[&r["id"].as_str().unwrap().to_string()];
                        ltmrs_domain::relation::Relation::new(
                            EntityId::new(Uuid::new_v5(
                                &Uuid::NAMESPACE_URL,
                                format!("rel:{}", r["id"].as_str().unwrap()).as_bytes(),
                            )),
                            eid,
                            target,
                            ltmrs_domain::relation::RelationType::parse(
                                r["type"].as_str().unwrap_or("related_to"),
                            )
                            .unwrap_or(ltmrs_domain::relation::RelationType::RelatedTo),
                            r["note"].as_str().map(|s| s.to_string()),
                            Instant::new(created_millis),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default(),
        parent_id: f["parent_id"].as_str().and_then(|p| id_map.get(p).copied()),
        child_ids: f["child_ids"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|c| id_map.get(c.as_str().unwrap_or("")).copied())
                    .collect()
            })
            .unwrap_or_default(),
        session_id: None,
        task_type: None,
        related_guides: Vec::new(),
        evidence: Vec::new(),
        access_count: 0,
        last_accessed_at: None,
        positive_feedback: f["positive_feedback"].as_u64().unwrap_or(0),
        negative_feedback: f["negative_feedback"].as_u64().unwrap_or(0),
        negative_hits: 0,
        refinement_count: f["refinement_count"].as_u64().unwrap_or(0),
        distill_candidate: false,
        entity_revision: ltmrs_domain::id::EntityRevision::new(1),
        document_revision: ltmrs_domain::id::DocumentRevision::new(1),
        eligibility_revision: ltmrs_domain::id::EligibilityRevision::new(1),
        created_at: Instant::new(created_millis),
        updated_at: Instant::new(created_millis),
        raw_created: None,
        unknown_fields: std::collections::BTreeMap::new(),
    }
}

#[test]
fn differential_detail_matches_upstream_wire() {
    let raw = std::fs::read_to_string(fixture_path("rendering_fixture.json"))
        .expect("rendering_fixture.json must exist");
    let fixture: Value = serde_json::from_str(&raw).expect("valid JSON fixture");
    let recs = fixture.as_array().expect("fixture is an array");

    // Map each legacy ID to a deterministic EntityId.
    let mut id_map: std::collections::HashMap<String, EntityId> = std::collections::HashMap::new();
    for rec in recs {
        let id = rec["fields"]["id"].as_str().unwrap().to_string();
        id_map.entry(id.clone()).or_insert_with(|| {
            EntityId::new(Uuid::new_v5(
                &Uuid::NAMESPACE_URL,
                format!("ltmrs:entity:{}", id).as_bytes(),
            ))
        });
    }

    let mut checked = 0;
    for rec in recs {
        let m = fixture_memory(rec, &id_map);
        let legacy_id = m.external_alias.as_ref().unwrap().as_str().to_string();
        let resolver = |eid: &EntityId| {
            id_map
                .iter()
                .find(|(_, e)| **e == *eid)
                .map(|(s, _)| s.clone())
                .unwrap_or_else(|| eid.as_uuid().to_string())
        };
        let actual = render_detail(&legacy_id, &m, &resolver);
        let expected = rec["detail"].as_str().unwrap();
        assert_eq!(actual, expected, "detail mismatch for {}", legacy_id);
        checked += 1;
    }
    assert!(
        checked >= 100,
        "fixture should cover many fragments, got {checked}"
    );
}

#[test]
fn differential_detail_matches_traffic_log_wire() {
    // Uses actual upstream wire responses from traffic logs as the oracle
    let raw = std::fs::read_to_string(fixture_path("traffic_fixture.json"))
        .expect("traffic_fixture.json must exist");
    let fixture: Value = serde_json::from_str(&raw).expect("valid JSON fixture");
    let recs = fixture.as_array().expect("fixture is an array");

    // Build id_map for all fragments AND all referenced IDs (relation targets, etc.)
    let mut id_map: std::collections::HashMap<String, EntityId> = std::collections::HashMap::new();
    let add_id = |id_map: &mut std::collections::HashMap<String, EntityId>, id: &str| {
        id_map.entry(id.to_string()).or_insert_with(|| {
            EntityId::new(Uuid::new_v5(
                &Uuid::NAMESPACE_URL,
                format!("ltmrs:entity:{}", id).as_bytes(),
            ))
        });
    };

    for rec in recs {
        let f = &rec["fields"];
        add_id(&mut id_map, f["id"].as_str().unwrap());
        // Add relation targets
        if let Some(rels) = f["relations"].as_array() {
            for r in rels {
                add_id(&mut id_map, r["id"].as_str().unwrap());
            }
        }
        // Add parent_id
        if let Some(parent) = f["parent_id"].as_str() {
            add_id(&mut id_map, parent);
        }
        // Add child_ids
        if let Some(children) = f["child_ids"].as_array() {
            for c in children {
                add_id(&mut id_map, c.as_str().unwrap());
            }
        }
    }

    for rec in recs {
        let m = fixture_memory(rec, &id_map);
        let legacy_id = m.external_alias.as_ref().unwrap().as_str().to_string();
        let resolver = |eid: &EntityId| {
            id_map
                .iter()
                .find(|(_, e)| **e == *eid)
                .map(|(s, _)| s.clone())
                .unwrap_or_else(|| eid.as_uuid().to_string())
        };
        let actual = render_detail(&legacy_id, &m, &resolver);
        let expected = rec["detail"].as_str().unwrap();
        assert_eq!(
            actual, expected,
            "traffic log detail mismatch for {}",
            legacy_id
        );
    }
}

/// Normalize a replayed text the same way the fixture was normalized:
/// m-hex ids -> $Mn (appearance order in `ids`), UUIDs and upstream
/// session ids -> $SID, datetimes -> $TS, projects -> $PROJ shape.
fn wf_normalize(text: &str, ids: &mut Vec<String>) -> String {
    let mid = regex::Regex::new(r"\bm[0-9a-f]{12}\b").unwrap();
    let mut out = mid
        .replace_all(text, |caps: &regex::Captures| {
            let hit = caps[0].to_string();
            let pos = match ids.iter().position(|id| *id == hit) {
                Some(i) => i + 1,
                None => {
                    ids.push(hit);
                    ids.len()
                }
            };
            format!("$M{pos}")
        })
        .into_owned();
    let uuid =
        regex::Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").unwrap();
    out = uuid.replace_all(&out, "$$SID").into_owned();
    let sess = regex::Regex::new(r"\bs[a-z][0-9a-f]{11}\b").unwrap();
    out = sess.replace_all(&out, "$$SID").into_owned();
    // Any 4-digit year: replay runs under a frozen test clock (1970).
    let ts = regex::Regex::new(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d+)?Z?").unwrap();
    out = ts.replace_all(&out, "$$TS").into_owned();
    out = out.replace("upstream-lemma", "$PROJ");
    // Project attribution differs by design (cwd-derived vs explicit):
    // compare the segment shape, not the project name.
    out = out.replace("(global)", "(project: $PROJ)");
    out
}

fn wf_normalize_value(value: &Value, ids: &mut Vec<String>) -> Value {
    match value {
        Value::String(s) => Value::String(wf_normalize(s, ids)),
        Value::Array(items) => {
            Value::Array(items.iter().map(|v| wf_normalize_value(v, ids)).collect())
        }
        Value::Object(map) => Value::Object(
            // Keys pass through untouched: the text-only project-shape
            // rule would otherwise rewrite aggregation buckets like
            // `(global)` (the fixture keeps them verbatim too).
            map.iter()
                .map(|(k, v)| (k.clone(), wf_normalize_value(v, ids)))
                .collect(),
        ),
        other => other.clone(),
    }
}
/// Differential workflow replay (WP-09 remaining task): the 8-step
/// recall -> act -> persist script from workflow_fixture.json (captured
/// from pinned upstream lemma 0.21.0) replayed through the public tool
/// handlers. Exact text+shape where parity is claimed; structural
/// sets/deltas with declared divergences everywhere else:
/// - upstream ships 4 seed fragments (counts, preload, read hits);
/// - upstream appends coaching blocks (`**[Lemma] ...**`) to session
///   lifecycle and stats texts;
/// - memory_read relevance order differs (sets compared, not order);
/// - upstream auto-detects technologies and suggests distill on end;
/// - project attribution is cwd-derived upstream, explicit here.
#[test]
fn differential_workflow_replay_matches_upstream() {
    let raw = std::fs::read_to_string(fixture_path("workflow_fixture.json"))
        .expect("workflow_fixture.json must exist");
    let fixture: Value = serde_json::from_str(&raw).expect("valid fixture");
    let steps = fixture["steps"].as_array().expect("steps array");
    assert_eq!(steps.len(), 8, "fixture must hold the 8-step workflow");
    let seed_count = fixture["provenance"]["upstream_seed_fragments"]
        .as_u64()
        .expect("seed count") as usize;

    let (disp, _dir) = test_dispatcher();
    let mut op = 1u64;
    let mut run_tool = |args: ToolArgs| -> DomainPayload {
        let env = tool_call(op, args.clone());
        op += 1;
        run(&disp, &env, &args)
    };
    // Replay the fixture args verbatim (same calls both sides).
    let mut replayed: Vec<(String, String, Value)> = Vec::new();
    for step in steps {
        let tool = step["tool"].as_str().unwrap();
        // Arguments come from the fixture (same calls both sides);
        // shapes absent from the capture stay None/defaulted.
        let str_vec = |key: &str| -> Vec<String> {
            step["args"][key]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default()
        };
        let req_str = |key: &str| -> String {
            step["args"][key]
                .as_str()
                .unwrap_or_else(|| panic!("fixture args lack {key}"))
                .to_string()
        };
        let args = match tool {
            "session_start" => ToolArgs::SessionStart(SessionStartArgs {
                task_type: req_str("task_type"),
                technologies: str_vec("technologies"),
                initial_approach: None,
            }),
            "memory_add" => ToolArgs::MemoryAdd(MemoryAddArgs {
                fragment: req_str("fragment"),
                ..Default::default()
            }),
            "memory_read" => ToolArgs::MemoryRead(MemoryReadArgs {
                query: Some(req_str("query")),
                ..Default::default()
            }),
            "guide_create" => ToolArgs::GuideCreate(GuideCreateArgs {
                guide: req_str("guide"),
                category: req_str("category"),
                description: req_str("description"),
                // Absent from the captured call: empty on both sides.
                contexts: Vec::new(),
                learnings: Vec::new(),
            }),
            "guide_practice" => ToolArgs::GuidePractice(GuidePracticeArgs {
                guide: req_str("guide"),
                category: req_str("category"),
                contexts: str_vec("contexts"),
                learnings: str_vec("learnings"),
                ..Default::default()
            }),
            "session_end" => ToolArgs::SessionEnd(SessionEndArgs {
                outcome: req_str("outcome"),
                ..Default::default()
            }),
            "memory_stats" => ToolArgs::MemoryStats(MemoryStatsArgs {
                ..Default::default()
            }),
            other => panic!("fixture holds an unexpected tool: {other}"),
        };
        let result = run_tool(args);
        assert!(
            !result_is_error(&result),
            "{tool} must succeed in replay: {}",
            result_text(&result)
        );
        let structured = result_structured(&result).unwrap_or(Value::Null);
        replayed.push((tool.to_string(), result_text(&result), structured));
    }

    // Normalize our side globally (same rules as the fixture).
    let mut ids: Vec<String> = Vec::new();
    let ours: Vec<(String, String, Value)> = replayed
        .into_iter()
        .map(|(tool, text, structured)| {
            (
                tool,
                wf_normalize(&text, &mut ids),
                wf_normalize_value(&structured, &mut ids),
            )
        })
        .collect();
    assert_eq!(
        ids.len(),
        2,
        "exactly the two added fragments take $M ids, got: {ids:?}"
    );

    // Step 0 session_start: same session line, techs and new guides;
    // seeds/coaching/tracked-debugging are declared divergences.
    assert!(ours[0].1.contains("Session started: $SID (research)"));
    assert!(ours[0].1.contains("Technologies: rust"));
    assert_eq!(ours[0].2["session_id"], Value::String("$SID".to_string()));
    assert_eq!(
        ours[0].2["guides"],
        serde_json::json!(["elasticsearch", "rust"])
    );
    let fx_guides: Vec<String> = steps[0]["structured"]["guides"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .filter(|g| g != "debugging")
        .collect();
    assert_eq!(
        fx_guides,
        vec!["elasticsearch".to_string(), "rust".to_string()],
        "upstream new-guide suggestions match ours (tracked debugging excluded)"
    );
    assert_eq!(ours[0].2["preloaded_memories"], Value::Array(vec![]));
    assert_eq!(
        steps[0]["structured"]["preloaded_memories"][0],
        Value::String("$SEED".to_string())
    );

    // Steps 1-2 memory_add: exact normalized parity (text + shape).
    for i in [1usize, 2] {
        assert_eq!(
            ours[i].1,
            steps[i]["text"].as_str().unwrap(),
            "add text parity"
        );
        assert_eq!(ours[i].2, steps[i]["structured"], "add shape parity");
    }

    // Step 3 memory_read: same added-fragment set (order differs by
    // design); upstream additionally hits one seed.
    let ours_ids: std::collections::BTreeSet<String> = ours[3].2["fragments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        ours_ids,
        std::collections::BTreeSet::from(["$M1".to_string(), "$M2".to_string()])
    );
    let fx_ids: std::collections::BTreeSet<String> = steps[3]["structured"]["fragments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        fx_ids,
        std::collections::BTreeSet::from([
            "$M1".to_string(),
            "$M2".to_string(),
            "$SEED".to_string()
        ])
    );
    assert_eq!(ours[3].2["count"], serde_json::json!(2));
    assert_eq!(steps[3]["structured"]["count"], serde_json::json!(3));

    // Steps 4-5 guide_create/practice: exact normalized parity.
    for i in [4usize, 5] {
        assert_eq!(
            ours[i].1,
            steps[i]["text"].as_str().unwrap(),
            "guide text parity"
        );
        assert_eq!(ours[i].2, steps[i]["structured"], "guide shape parity");
    }

    // Step 6 session_end: ours equals the upstream text minus the
    // coaching tail; same memories/guides attribution.
    let fx_end = steps[6]["text"].as_str().unwrap();
    let (fx_head, _) = fx_end
        .split_once("\nAuto-detected technologies:")
        .expect("upstream end carries auto-detected techs");
    assert_eq!(ours[6].1, fx_head, "end text parity before coaching tail");
    assert_eq!(ours[6].2["outcome_recorded"], Value::Bool(true));
    assert_eq!(ours[6].2["suggestions"], Value::Array(vec![]));
    assert!(
        steps[6]["structured"]["suggestions"]
            .as_array()
            .unwrap()
            .len()
            == 1,
        "upstream suggests distill on end (declared divergence)"
    );

    // Step 7 memory_stats: totals differ by exactly the seed count.
    let fx_total = steps[7]["structured"]["total"].as_u64().unwrap() as usize;
    let ours_total = ours[7].2["total"].as_u64().unwrap() as usize;
    assert_eq!(fx_total - ours_total, seed_count);
    assert_eq!(ours_total, 2, "our two adds, no seeds");
    assert_eq!(ours[7].2["by_source"]["ai"], serde_json::json!(2));
    assert_eq!(ours[7].2["by_project"]["(global)"], serde_json::json!(2));
    assert_eq!(ours[7].2["avg_confidence"], serde_json::json!(1.0));
    assert!(ours[7].1.contains("Total: 2 fragments"));
}

#[test]
fn differential_summary_matches_upstream_wire() {
    let raw = std::fs::read_to_string(fixture_path("rendering_fixture.json"))
        .expect("rendering_fixture.json must exist");
    let fixture: Value = serde_json::from_str(&raw).expect("valid JSON fixture");
    let recs = fixture.as_array().expect("fixture is an array");

    let mut id_map: std::collections::HashMap<String, EntityId> = std::collections::HashMap::new();
    for rec in recs {
        let id = rec["fields"]["id"].as_str().unwrap().to_string();
        id_map.entry(id.clone()).or_insert_with(|| {
            EntityId::new(Uuid::new_v5(
                &Uuid::NAMESPACE_URL,
                format!("ltmrs:entity:{}", id).as_bytes(),
            ))
        });
    }

    for rec in recs {
        let m = fixture_memory(rec, &id_map);
        let project = m.project.as_deref();
        let scope_info = match project {
            Some(p) => p.to_string(),
            None => "global".to_string(),
        };
        let lid = |m: &Memory| m.external_alias.as_ref().unwrap().as_str().to_string();
        let actual = render_summary_index(std::slice::from_ref(&m), &scope_info, &lid);
        let expected = rec["summary"].as_str().unwrap();
        assert_eq!(
            actual,
            expected,
            "summary mismatch for {}",
            m.external_alias.as_ref().unwrap().as_str()
        );
    }
}
