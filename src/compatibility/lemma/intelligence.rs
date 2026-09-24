//! Intelligence layer: conflict detection, proactive analysis, project analytics.
//!
//! Ports the upstream Lemma 0.21.0 intelligence modules (conflict.ts, proactive.ts,
//! session-analytics.ts, scoring.ts, consistency.ts) to match the frozen behavioral
//! contract. These are heuristic/advisory — they never mutate canonical state.

use std::collections::{HashMap, HashSet};

use crate::domain::guide::Guide;
use crate::domain::memory::Memory;
use crate::domain::session::Session;

// ---- Conflict detection (conflict.ts) ----

const NEGATION_PATTERNS: &[&str] = &[
    r"\b(not|don'?t|doesn'?t|didn'?t|won'?t|wouldn'?t|shouldn'?t|can'?t|cannot|never|no\s)\b",
    r"\b(however|but|instead|rather|conversely|on the contrary|actually)\b",
];

const CONTRADICTION_SIGNALS: &[(&str, &str, f64)] = &[
    (r"\balways\b", r"\bnever\b", 0.9),
    (r"\bgood\b", r"\bbad\b", 0.7),
    (r"\bfast\b", r"\bslow\b", 0.6),
    (r"\bsimple\b", r"\bcomplex\b", 0.6),
    (r"\bbest\b", r"\bworst\b", 0.8),
    (r"\brecommended\b", r"\bavoid\b", 0.8),
    (r"\buse\b", r"\bdon'?t use\b", 0.9),
    (r"\bprefer\b", r"\bavoid\b", 0.8),
];

const CONFLICT_MIN_OVERLAP: f64 = 0.3;
const CONFLICT_EMIT_THRESHOLD: f64 = 0.4;

#[derive(Debug, Clone, serde::Serialize)]
pub struct ConflictPair {
    pub memory_a_id: String,
    pub memory_a_title: String,
    pub memory_b_id: String,
    pub memory_b_title: String,
    pub reason: String,
    pub overlap_score: f64,
}

fn has_negation(text: &str) -> bool {
    let lower = text.to_lowercase();
    NEGATION_PATTERNS.iter().any(|pattern| {
        regex::Regex::new(pattern)
            .map(|re| re.is_match(&lower))
            .unwrap_or(false)
    })
}

fn extract_topic_signature(text: &str) -> HashSet<String> {
    let stop_words = STOP_WORDS_CONFLICT;
    let lower = text.to_lowercase();
    lower
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c.is_whitespace() {
                c
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .filter(|w| w.len() > 2 && !stop_words.contains(w))
        .map(|w| w.to_string())
        .collect()
}

const STOP_WORDS_CONFLICT: &[&str] = &[
    "the", "a", "an", "is", "are", "was", "were", "be", "been", "being", "have", "has", "had",
    "do", "does", "did", "will", "would", "could", "should", "may", "might", "can", "shall", "to",
    "of", "in", "for", "on", "with", "at", "by", "from", "as", "into", "through", "during",
    "before", "after", "above", "below", "between", "out", "off", "over", "under", "again",
    "further", "then", "once", "here", "there", "when", "where", "why", "how", "all", "each",
    "every", "both", "few", "more", "most", "other", "some", "such", "no", "nor", "not", "only",
    "own", "same", "so", "than", "too", "very", "just", "because", "but", "and", "or", "if",
    "while", "about", "up", "it", "its", "this", "that", "these", "those", "i", "me", "my", "we",
    "our", "you", "your", "he", "him", "his", "she", "her", "they", "them", "their", "what",
    "which", "who", "whom", "am",
];

fn topic_overlap(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let overlap = a.iter().filter(|t| b.contains(*t)).count();
    overlap as f64 / a.len().min(b.len()) as f64
}

fn score_conflict(fragment_a: &str, fragment_b: &str, overlap: f64, negation_differs: bool) -> f64 {
    let mut conflict_score = 0.0;
    if negation_differs && overlap >= 0.5 {
        conflict_score = 0.6 + (overlap - 0.5) * 0.4;
    }
    let signal_score = detect_contradiction_signals(fragment_a, fragment_b);
    conflict_score.max(signal_score * overlap)
}

fn detect_contradiction_signals(text_a: &str, text_b: &str) -> f64 {
    let mut max_score: f64 = 0.0;
    for (pattern_a, pattern_b, weight) in CONTRADICTION_SIGNALS {
        let re_a = regex::Regex::new(pattern_a).ok();
        let re_b = regex::Regex::new(pattern_b).ok();
        if let (Some(re_a), Some(re_b)) = (re_a, re_b) {
            let a_has = re_a.is_match(text_a) && re_b.is_match(text_b);
            let b_has = re_b.is_match(text_a) && re_a.is_match(text_b);
            if a_has || b_has {
                max_score = max_score.max(*weight);
            }
        }
    }
    max_score
}

pub fn scan_for_conflicts(
    memories: &[Memory],
    legacy_id_of: impl Fn(&Memory) -> String,
) -> Vec<ConflictPair> {
    let mut conflicts = Vec::new();
    let n = memories.len();
    if n < 2 {
        return conflicts;
    }

    let signatures: Vec<HashSet<String>> = memories
        .iter()
        .map(|m| extract_topic_signature(&m.fragment))
        .collect();
    let negations: Vec<bool> = memories.iter().map(|m| has_negation(&m.fragment)).collect();

    for i in 0..n {
        for j in (i + 1)..n {
            let overlap = topic_overlap(&signatures[i], &signatures[j]);
            if overlap < CONFLICT_MIN_OVERLAP {
                continue;
            }
            let neg_differs = negations[i] != negations[j];
            let conflict_score = score_conflict(
                &memories[i].fragment,
                &memories[j].fragment,
                overlap,
                neg_differs,
            );
            if conflict_score >= CONFLICT_EMIT_THRESHOLD {
                conflicts.push(ConflictPair {
                    memory_a_id: legacy_id_of(&memories[i]),
                    memory_a_title: memories[i].title.clone(),
                    memory_b_id: legacy_id_of(&memories[j]),
                    memory_b_title: memories[j].title.clone(),
                    reason: if neg_differs {
                        "Opposing sentiment on same topic".to_string()
                    } else {
                        "Contradiction signals detected".to_string()
                    },
                    overlap_score: (conflict_score * 100.0).round() / 100.0,
                });
            }
        }
    }

    conflicts.sort_by(|a, b| {
        b.overlap_score
            .partial_cmp(&a.overlap_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    conflicts
}

pub fn format_conflict_results(conflicts: &[ConflictPair]) -> String {
    if conflicts.is_empty() {
        return "No conflicts detected.".to_string();
    }
    let mut output = format!(
        "=== CONFLICT DETECTION ===\nFound {} potential conflict(s):\n\n",
        conflicts.len()
    );
    for c in conflicts {
        output.push_str(&format!(
            "  [{}] [{}] \"{}\" vs [{}] \"{}\"\n",
            c.overlap_score, c.memory_a_id, c.memory_a_title, c.memory_b_id, c.memory_b_title
        ));
        output.push_str(&format!("    Reason: {}\n", c.reason));
    }
    output.push_str("\nUse memory_relate with type \"contradicts\" to link these.");
    output
}

// ---- Proactive analysis (proactive.ts + scoring.ts) ----

#[derive(Debug, Clone, serde::Serialize)]
pub struct ProactiveSuggestion {
    pub r#type: String,
    pub priority: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_action: Option<String>,
}

const QUALITY_SUGGESTION_THRESHOLD: f64 = 0.35;

fn calculate_quality_score(m: &Memory, now_millis: u64) -> f64 {
    let confidence = m.confidence;
    let pos = m.positive_feedback as f64;
    let neg = m.negative_feedback as f64;
    let feedback_total = pos + neg;
    let feedback_ratio = if feedback_total > 0.0 {
        pos / feedback_total
    } else {
        0.5
    };
    let accessed = m.access_count as f64;
    let usage_score = (accessed / 10.0).min(1.0);
    let refinement = m.refinement_count as f64;
    let refinement_score = (refinement / 3.0).min(1.0);
    let neg_hits = m.negative_hits as f64;
    let neg_hit_penalty = (neg_hits / 4.0).min(1.0);

    let last_accessed_ms = m
        .last_accessed_at
        .map(|t| t.as_millis())
        .unwrap_or(now_millis);
    let days_stale = (now_millis.saturating_sub(last_accessed_ms) as f64) / 86_400_000.0;
    let staleness_score = (days_stale / 180.0).clamp(0.0, 1.0);

    let score = 0.40 * confidence
        + 0.20 * feedback_ratio
        + 0.15 * usage_score
        + 0.10 * refinement_score
        + 0.15 * (1.0 - staleness_score)
        - 0.35 * neg_hit_penalty;

    (score.clamp(0.0, 1.0) * 1000.0).round() / 1000.0
}

fn quality_score_reasons(m: &Memory, now_millis: u64) -> Vec<String> {
    let mut reasons = Vec::new();
    let pos = m.positive_feedback;
    let neg = m.negative_feedback;
    let neg_hits = m.negative_hits;
    let accessed = m.access_count;
    let confidence = m.confidence;

    if neg > pos {
        reasons.push(format!("{neg} negative vs {pos} positive feedback"));
    }
    if neg_hits >= 2 {
        reasons.push(format!("{neg_hits} negative recall hits"));
    }
    if accessed >= 3 && confidence < 0.4 {
        reasons.push(format!(
            "accessed {accessed}x but confidence still {confidence:.2}"
        ));
    }

    let last_accessed_ms = m
        .last_accessed_at
        .map(|t| t.as_millis())
        .unwrap_or(now_millis);
    let days_stale = (now_millis.saturating_sub(last_accessed_ms) as f64) / 86_400_000.0;
    if days_stale > 120.0 && accessed >= 1 {
        reasons.push(format!("untouched for {} days", days_stale.round()));
    }

    reasons
}

fn is_low_quality(m: &Memory, now_millis: u64) -> bool {
    !quality_score_reasons(m, now_millis).is_empty()
        && calculate_quality_score(m, now_millis) < QUALITY_SUGGESTION_THRESHOLD
}

pub fn run_full_analysis(
    memories: &[Memory],
    guides: &[Guide],
    now_millis: u64,
    legacy_id_of: impl Fn(&Memory) -> String,
) -> Vec<ProactiveSuggestion> {
    let mut suggestions = Vec::new();

    let stale_count = memories.iter().filter(|m| m.confidence < 0.2).count();
    if stale_count > 0 {
        suggestions.push(ProactiveSuggestion {
            r#type: "archive".into(),
            priority: if stale_count > 10 { "high".into() } else { "medium".into() },
            message: format!("{stale_count} memories have very low confidence (<0.2). Consider cleanup with memory_forget or memory_feedback."),
            suggested_action: None,
        });
    }

    let orphan_count = memories
        .iter()
        .filter(|m| m.relations.is_empty() && m.access_count < 2)
        .count();
    if !memories.is_empty() && orphan_count as f64 > memories.len() as f64 * 0.5 {
        suggestions.push(ProactiveSuggestion {
            r#type: "relate".into(),
            priority: "medium".into(),
            message: format!("{orphan_count} of {} memories are isolated (no relations, rarely accessed). Consider reading and linking them.", memories.len()),
            suggested_action: None,
        });
    }

    let deprecated_guides: Vec<&Guide> = guides.iter().filter(|g| g.deprecated).collect();
    if !deprecated_guides.is_empty() {
        let names: Vec<&str> = deprecated_guides.iter().map(|g| g.name.as_str()).collect();
        suggestions.push(ProactiveSuggestion {
            r#type: "archive".into(),
            priority: "low".into(),
            message: format!(
                "{} deprecated guide(s): {}. Consider guide_forget to clean up.",
                names.len(),
                names.join(", ")
            ),
            suggested_action: None,
        });
    }

    let unpracticed: Vec<&Guide> = guides
        .iter()
        .filter(|g| g.usage_count >= 3 && g.learnings.is_empty())
        .collect();
    if !unpracticed.is_empty() {
        let names: Vec<&str> = unpracticed
            .iter()
            .take(3)
            .map(|g| g.name.as_str())
            .collect();
        suggestions.push(ProactiveSuggestion {
            r#type: "refine".into(),
            priority: "medium".into(),
            message: format!("{} guide(s) used 3+ times without learnings: {}. Add learnings via guide_practice.", unpracticed.len(), names.join(", ")),
            suggested_action: None,
        });
    }

    let hot_distill: Vec<&Memory> = memories
        .iter()
        .filter(|m| {
            matches!(
                m.fragment_type,
                crate::domain::memory::FragmentType::Pattern
                    | crate::domain::memory::FragmentType::Lesson
            ) && m.access_count >= 5
                && m.related_guides.is_empty()
        })
        .collect();
    if !hot_distill.is_empty() {
        let examples: Vec<String> = hot_distill
            .iter()
            .take(3)
            .map(|m| format!("\"{}\" ({}x)", m.title, m.access_count))
            .collect();
        suggestions.push(ProactiveSuggestion {
            r#type: "distill".into(),
            priority: if hot_distill.iter().any(|m| m.access_count >= 10) {
                "high".into()
            } else {
                "medium".into()
            },
            message: format!("{} frequently accessed pattern(s)/lesson(s) without guides: {}. These are reused knowledge — distill into guides.", hot_distill.len(), examples.join(", ")),
            suggested_action: Some("guide_distill for each hot fragment".into()),
        });
    }

    let low_quality: Vec<&Memory> = memories
        .iter()
        .filter(|m| is_low_quality(m, now_millis))
        .collect();
    if !low_quality.is_empty() {
        let examples: Vec<String> = low_quality
            .iter()
            .take(3)
            .map(|m| {
                format!(
                    "[{}] \"{}\" ({})",
                    legacy_id_of(m),
                    m.title,
                    quality_score_reasons(m, now_millis).join(", ")
                )
            })
            .collect();
        suggestions.push(ProactiveSuggestion {
            r#type: "refine".into(),
            priority: if low_quality.len() > 5 {
                "high".into()
            } else {
                "medium".into()
            },
            message: format!(
                "{} memory(ies) score below the quality threshold: {}. Refine or prune them.",
                low_quality.len(),
                examples.join("; ")
            ),
            suggested_action: Some(
                "memory_update / memory_feedback / memory_forget on the weakest fragments".into(),
            ),
        });
    }

    suggestions
}

pub fn format_suggestions(suggestions: &[ProactiveSuggestion]) -> String {
    if suggestions.is_empty() {
        return String::new();
    }

    let high: Vec<&ProactiveSuggestion> = suggestions
        .iter()
        .filter(|s| s.priority == "high")
        .collect();
    let medium: Vec<&ProactiveSuggestion> = suggestions
        .iter()
        .filter(|s| s.priority == "medium")
        .collect();
    let low: Vec<&ProactiveSuggestion> =
        suggestions.iter().filter(|s| s.priority == "low").collect();

    let mut output = String::from("\n--- SUGGESTIONS ---\n");
    for s in high {
        output.push_str(&format!("  [!] {}\n", s.message));
        if let Some(action) = &s.suggested_action {
            output.push_str(&format!("      → {action}\n"));
        }
    }
    for s in medium {
        output.push_str(&format!("  [*] {}\n", s.message));
        if let Some(action) = &s.suggested_action {
            output.push_str(&format!("      → {action}\n"));
        }
    }
    for s in low {
        output.push_str(&format!("  [ ] {}\n", s.message));
    }
    output.push_str("---\n");
    output
}

// ---- Project analytics (session-analytics.ts) ----

fn date_only(millis: u64) -> String {
    let days = millis / 86_400_000;
    let (y, mo, d) = civil_from_days(days as i64);
    format!("{y:04}-{mo:02}-{d:02}")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    (if mo <= 2 { y + 1 } else { y }, mo as u32, d as u32)
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ProjectProgress {
    pub project: String,
    pub total_sessions: usize,
    pub total_memories: usize,
    pub total_guides: usize,
    pub knowledge_growth_rate: f64,
    pub skill_coverage: Vec<SkillCoverage>,
    pub recent_insights: Vec<String>,
    pub health_score: f64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SkillCoverage {
    pub category: String,
    pub count: u32,
    pub trend: String,
}

pub fn get_project_analytics(
    project: &str,
    sessions: &[Session],
    memories: &[Memory],
    guides: &[Guide],
    now_millis: u64,
) -> ProjectProgress {
    let project_lower = project.to_lowercase();

    let mut project_sessions: Vec<&Session> = sessions
        .iter()
        .filter(|s| {
            s.project
                .as_ref()
                .map(|p| p.to_lowercase() == project_lower)
                .unwrap_or(false)
        })
        .collect();
    project_sessions.sort_by_key(|s| std::cmp::Reverse(s.started_at.as_millis()));

    let mut project_memories: Vec<&Memory> = memories
        .iter()
        .filter(|m| {
            m.project
                .as_ref()
                .map(|p| p.to_lowercase() == project_lower)
                .unwrap_or(false)
        })
        .collect();
    project_memories.sort_by_key(|m| std::cmp::Reverse(m.created_at.as_millis()));

    let seven_days = 7 * 86400 * 1000u64;
    let fourteen_days = 14 * 86400 * 1000u64;

    let recent_count = project_memories
        .iter()
        .filter(|m| now_millis.saturating_sub(m.created_at.as_millis()) < seven_days)
        .count();
    let older_count = project_memories
        .iter()
        .filter(|m| {
            let age = now_millis.saturating_sub(m.created_at.as_millis());
            age >= seven_days && age < fourteen_days
        })
        .count();

    let knowledge_growth_rate = if project_memories.len() < 2 {
        0.0
    } else if older_count == 0 {
        if recent_count > 0 { 1.0 } else { 0.0 }
    } else {
        ((recent_count as f64 / older_count as f64) * 100.0).round() / 100.0
    };

    let mut category_map: HashMap<String, (u32, u32)> = HashMap::new();
    for g in guides {
        let cat = if g.category.is_empty() {
            "uncategorized".to_string()
        } else {
            g.category.clone()
        };
        let entry = category_map.entry(cat).or_insert((0, 0));
        entry.0 += g.usage_count;
    }

    for s in &project_sessions {
        if now_millis.saturating_sub(s.started_at.as_millis()) < seven_days {
            for tech in &s.technologies {
                if let Some(guide) = guides
                    .iter()
                    .find(|g| g.name == *tech || g.contexts.iter().any(|c| c == tech))
                {
                    let cat = if guide.category.is_empty() {
                        "uncategorized".to_string()
                    } else {
                        guide.category.clone()
                    };
                    if let Some(entry) = category_map.get_mut(&cat) {
                        entry.1 += 1;
                    }
                }
            }
        }
    }

    let mut skill_coverage: Vec<SkillCoverage> = category_map
        .into_iter()
        .map(|(category, (total, recent))| {
            let trend = if total > 0 {
                let ratio = recent as f64 / total.max(1) as f64;
                if ratio > 0.3 {
                    "growing".to_string()
                } else if ratio < 0.05 && total > 5 {
                    "declining".to_string()
                } else {
                    "stable".to_string()
                }
            } else {
                "stable".to_string()
            };
            SkillCoverage {
                category,
                count: total,
                trend,
            }
        })
        .collect();
    skill_coverage.sort_by_key(|s| std::cmp::Reverse(s.count));

    let mut recent_insights = Vec::new();
    let success_sessions: Vec<&Session> = project_sessions
        .iter()
        .filter(|s| s.outcome == Some(crate::domain::session::TaskOutcome::Success))
        .copied()
        .collect();
    for s in success_sessions.iter().take(3) {
        if let Some(task_type) = &s.task_type {
            let date = date_only(s.started_at.as_millis());
            recent_insights.push(format!("Completed {task_type} task successfully ({date})"));
        }
    }

    let high_conf: Vec<&Memory> = project_memories
        .iter()
        .filter(|m| m.confidence > 0.8)
        .copied()
        .collect();
    if !high_conf.is_empty() {
        let mut type_counts: HashMap<String, usize> = HashMap::new();
        for m in &high_conf {
            let type_name = match m.fragment_type {
                crate::domain::memory::FragmentType::Fact => "fact",
                crate::domain::memory::FragmentType::Pattern => "pattern",
                crate::domain::memory::FragmentType::Lesson => "lesson",
                crate::domain::memory::FragmentType::Warning => "warning",
                crate::domain::memory::FragmentType::Context => "context",
            };
            *type_counts.entry(type_name.to_string()).or_insert(0) += 1;
        }
        if let Some((dominant, count)) = type_counts.into_iter().max_by_key(|(_, c)| *c) {
            recent_insights.push(format!(
                "Strong knowledge base in {dominant} ({count} high-confidence fragments)"
            ));
        }
    }

    for m in project_memories.iter().take(3) {
        let type_name = match m.fragment_type {
            crate::domain::memory::FragmentType::Fact => "fact",
            crate::domain::memory::FragmentType::Pattern => "pattern",
            crate::domain::memory::FragmentType::Lesson => "lesson",
            crate::domain::memory::FragmentType::Warning => "warning",
            crate::domain::memory::FragmentType::Context => "context",
        };
        recent_insights.push(format!("Recent: \"{}\" ({type_name})", m.title));
    }
    recent_insights.truncate(8);

    let mut health_score = 0.5;
    if !project_memories.is_empty() {
        let avg_conf: f64 = project_memories.iter().map(|m| m.confidence).sum::<f64>()
            / project_memories.len() as f64;
        health_score += avg_conf * 0.2;
    }
    if !project_sessions.is_empty() {
        let success_rate = success_sessions.len() as f64 / project_sessions.len() as f64;
        health_score += success_rate * 0.15;
    }
    if !guides.is_empty() {
        let practiced = guides.iter().filter(|g| g.usage_count > 1).count() as f64;
        health_score += (practiced / guides.len() as f64).min(1.0) * 0.15;
    }
    health_score = (health_score * 100.0).round() / 100.0;
    health_score = health_score.min(1.0);

    ProjectProgress {
        project: project.to_string(),
        total_sessions: project_sessions.len(),
        total_memories: project_memories.len(),
        total_guides: guides.len(),
        knowledge_growth_rate,
        skill_coverage,
        recent_insights,
        health_score,
    }
}

pub fn get_all_projects_analytics(
    sessions: &[Session],
    memories: &[Memory],
    guides: &[Guide],
    now_millis: u64,
) -> Vec<ProjectProgress> {
    let mut projects: Vec<String> = Vec::new();
    for s in sessions {
        if let Some(p) = &s.project
            && !projects.contains(p)
        {
            projects.push(p.clone());
        }
    }
    for m in memories {
        if let Some(p) = &m.project
            && !projects.contains(p)
        {
            projects.push(p.clone());
        }
    }
    projects
        .into_iter()
        .map(|p| get_project_analytics(&p, sessions, memories, guides, now_millis))
        .collect()
}

pub fn format_project_progress(progress: &ProjectProgress) -> String {
    let mut output = format!("=== PROJECT ANALYTICS: {} ===\n\n", progress.project);
    output.push_str(&format!(
        "Health Score: {:.0}%\n",
        progress.health_score * 100.0
    ));
    output.push_str(&format!(
        "Sessions: {} | Memories: {} | Guides: {}\n",
        progress.total_sessions, progress.total_memories, progress.total_guides
    ));
    output.push_str(&format!(
        "Knowledge Growth Rate: {}x (last 7 days vs prior 7 days)\n",
        progress.knowledge_growth_rate
    ));

    if !progress.skill_coverage.is_empty() {
        output.push_str("\nSkill Coverage:\n");
        for skill in progress.skill_coverage.iter().take(10) {
            let icon = match skill.trend.as_str() {
                "growing" => "↑",
                "declining" => "↓",
                _ => "→",
            };
            output.push_str(&format!(
                "  {icon} {}: {}x usage\n",
                skill.category, skill.count
            ));
        }
    }

    if !progress.recent_insights.is_empty() {
        output.push_str("\nRecent Activity:\n");
        for insight in &progress.recent_insights {
            output.push_str(&format!("  - {insight}\n"));
        }
    }

    output.push_str("\n====================");
    output
}
