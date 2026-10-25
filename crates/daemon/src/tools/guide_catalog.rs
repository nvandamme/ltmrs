//! Guide catalog: task map, matching, suggestions (moved verbatim from `tools.rs`).

use ltmrs_domain::guide::Guide;
use std::collections::BTreeSet;

/// A guide suggestion entry (upstream GuideSuggestion).
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct GuideSuggestion {
    pub(crate) guide: String,
    pub(crate) category: String,
    pub(crate) keywords: Vec<String>,
    pub(crate) tracked: bool,
    pub(crate) usage_count: u32,
    pub(crate) last_used: Option<String>,
    pub(crate) learnings: Vec<String>,
    pub(crate) contexts: Vec<String>,
}

/// Max dense-only guides appended after the token matches (heuristic cap:
/// tail cosine candidates are noise; pinned by tests).
pub(crate) const DENSE_GUIDE_APPEND_CAP: usize = 5;

/// The upstream task→guide keyword map (guides/task-map.ts).
pub(crate) fn task_guide_defs() -> Vec<(&'static str, &'static str, &'static [&'static str])> {
    vec![
        (
            "html",
            "web-frontend",
            &["web", "sayfa", "ui", "arayüz", "html"],
        ),
        (
            "css",
            "web-frontend",
            &["stil", "style", "tasarım", "design", "css"],
        ),
        (
            "javascript",
            "programming-language",
            &["js", "web", "frontend"],
        ),
        (
            "react",
            "web-frontend",
            &["component", "jsx", "hook", "state", "react"],
        ),
        ("vue", "web-frontend", &["vue", "component", "template"]),
        (
            "angular",
            "web-frontend",
            &["angular", "component", "service"],
        ),
        ("tailwind", "web-frontend", &["tailwind", "css", "utility"]),
        (
            "nextjs",
            "web-frontend",
            &["next", "nextjs", "ssr", "app router"],
        ),
        (
            "typescript",
            "programming-language",
            &["ts", "tip", "type", "interface"],
        ),
        (
            "nodejs",
            "web-backend",
            &["node", "server", "api", "express"],
        ),
        (
            "express",
            "web-backend",
            &["express", "router", "middleware"],
        ),
        (
            "nestjs",
            "web-backend",
            &["nestjs", "module", "controller", "service"],
        ),
        (
            "python",
            "programming-language",
            &["py", "django", "flask", "fastapi"],
        ),
        ("fastapi", "web-backend", &["fastapi", "async", "python"]),
        ("django", "web-backend", &["django", "orm", "python"]),
        ("rest", "web-backend", &["api", "rest", "endpoint", "http"]),
        (
            "graphql",
            "web-backend",
            &["graphql", "query", "mutation", "schema"],
        ),
        ("trpc", "web-backend", &["trpc", "typescript", "rpc"]),
        (
            "postgresql",
            "data-storage",
            &["postgres", "sql", "relational", "pg"],
        ),
        ("mongodb", "data-storage", &["mongo", "nosql", "document"]),
        ("redis", "data-storage", &["redis", "cache", "key-value"]),
        ("prisma", "data-storage", &["prisma", "orm", "schema"]),
        ("sqlite", "data-storage", &["sqlite", "local", "embedded"]),
        (
            "supabase",
            "data-storage",
            &["supabase", "postgres", "auth", "storage"],
        ),
        (
            "pinecone",
            "data-storage",
            &["pinecone", "vector", "embedding"],
        ),
        (
            "elasticsearch",
            "data-storage",
            &["elastic", "search", "index"],
        ),
        ("git", "dev-tool", &["git", "commit", "branch", "merge"]),
        ("docker", "infra-devops", &["docker", "container", "image"]),
        ("webpack", "dev-tool", &["webpack", "bundle", "build"]),
        ("vite", "dev-tool", &["vite", "build", "dev", "hmr"]),
        ("jest", "dev-tool", &["jest", "test", "unit", "spec"]),
        ("vitest", "dev-tool", &["vitest", "test", "vite"]),
        (
            "playwright",
            "dev-tool",
            &["playwright", "e2e", "browser", "test"],
        ),
        ("eslint", "dev-tool", &["eslint", "lint", "format"]),
        (
            "react-native",
            "mobile-frontend",
            &["react native", "mobile", "expo", "rn"],
        ),
        (
            "flutter",
            "mobile-frontend",
            &["flutter", "dart", "mobile", "widget"],
        ),
        (
            "expo",
            "mobile-frontend",
            &["expo", "react native", "mobile"],
        ),
        (
            "swift",
            "mobile-frontend",
            &["swift", "ios", "iphone", "swiftui"],
        ),
        (
            "kotlin",
            "mobile-frontend",
            &["kotlin", "android", "jetpack"],
        ),
        (
            "threejs",
            "game-frontend",
            &["threejs", "three.js", "webgl", "3d"],
        ),
        (
            "canvas",
            "game-frontend",
            &["canvas", "html5", "2d", "drawing"],
        ),
        (
            "phaser",
            "game-frontend",
            &["phaser", "game", "html5", "2d"],
        ),
        ("webgl", "game-frontend", &["webgl", "shader", "gpu", "3d"]),
        (
            "godot",
            "game-backend",
            &["godot", "gdscript", "game engine"],
        ),
        (
            "game-loop",
            "game-backend",
            &["game loop", "update", "render", "fixed timestep"],
        ),
        (
            "state-machine",
            "game-backend",
            &["state", "fsm", "transition"],
        ),
        (
            "ecs",
            "game-backend",
            &["ecs", "entity", "component", "system"],
        ),
        (
            "object-pooling",
            "game-backend",
            &["pool", "reuse", "spawn", "bullet"],
        ),
        (
            "ai-art-generation",
            "game-tool",
            &["ai art", "stable diffusion", "flux", "dalle"],
        ),
        (
            "pixel-art",
            "game-design",
            &["pixel", "sprite", "8bit", "16bit", "retro"],
        ),
        (
            "aseprite",
            "game-tool",
            &["aseprite", "sprite", "animation"],
        ),
        (
            "spritesheet",
            "game-tool",
            &["spritesheet", "atlas", "texture", "export"],
        ),
        (
            "background-removal",
            "game-tool",
            &["bg remove", "transparent", "cutout"],
        ),
        (
            "image-upscaling",
            "game-tool",
            &["upscale", "esrgan", "hd", "4k"],
        ),
        (
            "level-design",
            "game-design",
            &["level", "map", "blockout", "flow"],
        ),
        (
            "character-design",
            "game-design",
            &["character", "silhouette", "shape language"],
        ),
        (
            "texture-art",
            "game-design",
            &["texture", "pbr", "normal map", "material"],
        ),
        (
            "animation",
            "game-design",
            &["animation", "walk cycle", "frame", "sprite"],
        ),
        (
            "tileset",
            "game-design",
            &["tileset", "tile", "autotile", "seamless"],
        ),
        (
            "oauth",
            "app-security",
            &["oauth", "auth", "login", "token"],
        ),
        ("jwt", "app-security", &["jwt", "token", "authentication"]),
        (
            "owasp",
            "app-security",
            &["owasp", "security", "vulnerability", "xss", "sql injection"],
        ),
        (
            "cryptography",
            "app-security",
            &["crypto", "encrypt", "hash", "ssl", "tls"],
        ),
        (
            "clerk",
            "app-security",
            &["clerk", "auth", "user management"],
        ),
        (
            "figma",
            "ui-design",
            &["figma", "design", "prototype", "ui"],
        ),
        (
            "accessibility",
            "ui-design",
            &["a11y", "accessibility", "wcag", "aria"],
        ),
        (
            "design-system",
            "ui-design",
            &["design system", "tokens", "components"],
        ),
        (
            "ci-cd",
            "infra-devops",
            &["ci", "cd", "pipeline", "github actions"],
        ),
        (
            "kubernetes",
            "infra-devops",
            &["k8s", "kubernetes", "pod", "deployment"],
        ),
        ("aws", "infra-devops", &["aws", "s3", "lambda", "ec2"]),
        (
            "vercel",
            "infra-devops",
            &["vercel", "deploy", "edge", "serverless"],
        ),
        (
            "terraform",
            "infra-devops",
            &["terraform", "iac", "infrastructure"],
        ),
        ("rust", "programming-language", &["rust", "rustlang"]),
        ("golang", "programming-language", &["go", "golang"]),
        ("java", "programming-language", &["java", "jvm"]),
    ]
}

pub(crate) fn tokenize(str_: &str) -> BTreeSet<String> {
    str_.to_lowercase()
        .chars()
        .map(|c| if c == '-' || c == '_' { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .filter(|t| t.len() >= 2)
        .map(|t| t.to_string())
        .collect()
}

pub(crate) fn has_token_match(text: &str, target: &str) -> bool {
    let text_tokens = tokenize(text);
    let target_tokens = tokenize(target);
    for token in &text_tokens {
        if target_tokens.contains(token) {
            return true;
        }
    }
    for text_token in &text_tokens {
        for target_token in &target_tokens {
            if text_token.contains(target_token) || target_token.contains(text_token) {
                return true;
            }
        }
    }
    false
}

/// Render a guide as passage-role embedding input: catalog text only.
/// Usage counters and timestamps are deliberately excluded — they are not
/// relevance signals.
pub(crate) fn guide_catalog_text(guide: &Guide) -> String {
    let mut parts = vec![guide.name.clone(), guide.description.clone()];
    parts.extend(guide.contexts.iter().cloned());
    parts.extend(guide.learnings.iter().cloned());
    parts
        .into_iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Dense candidate proposal over the guide catalog (WP-09): cosine-rank the
/// task (Query role) against each guide (Passage role) and return up to
/// `DENSE_GUIDE_APPEND_CAP` positively-similar guides not already
/// suggested. Scores propose candidates only — they are never displayed
/// nor treated as proof of anything. Returns empty when the task is blank,
/// the catalog is empty, dimensions mismatch, or any embedding fails, so
/// callers fall back to the token-only suggestions byte-identically.
pub(crate) fn suggest_guides_dense(
    backend: &ltmrs_search::search::backend::SearchBackend,
    task: &str,
    existing: &[Guide],
    seen: &std::collections::BTreeSet<String>,
) -> Vec<GuideSuggestion> {
    if task.trim().is_empty() || existing.is_empty() {
        return Vec::new();
    }
    let task_vec = match backend.embed_query_sync(task) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let texts: Vec<String> = existing.iter().map(guide_catalog_text).collect();
    let guide_vecs = match backend.embed_passages_sync(&texts) {
        Ok(v) if v.len() == existing.len() => v,
        _ => return Vec::new(),
    };
    let mut ranked: Vec<(usize, f64)> = guide_vecs
        .iter()
        .enumerate()
        .filter(|(i, _)| !seen.contains(&existing[*i].name))
        .map(|(i, g)| {
            (
                i,
                ltmrs_search::retrieval::ranking::cosine(Some(&task_vec), Some(g)),
            )
        })
        .filter(|(_, score)| *score > 0.0)
        .collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    ranked
        .into_iter()
        .take(DENSE_GUIDE_APPEND_CAP)
        .map(|(i, _)| {
            let g = &existing[i];
            GuideSuggestion {
                guide: g.name.clone(),
                category: g.category.clone(),
                keywords: g.contexts.clone(),
                tracked: true,
                usage_count: g.usage_count,
                last_used: g.last_used.map(|t| super::recall::date_only(t.as_millis())),
                learnings: g.learnings.clone(),
                contexts: g.contexts.clone(),
            }
        })
        .collect()
}

/// Suggest guides for a task description (upstream guides.suggestGuides).
pub(crate) fn suggest_guides(
    task_description: &str,
    existing_guides: &[Guide],
) -> Vec<GuideSuggestion> {
    let mut suggestions: Vec<GuideSuggestion> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let desc_lower = task_description.to_lowercase();

    for (guide, category, keywords) in task_guide_defs() {
        if seen.contains(guide) {
            continue;
        }
        let guide_match = desc_lower.contains(guide);
        let keyword_match = keywords
            .iter()
            .any(|kw| desc_lower.contains(&kw.to_lowercase()));
        if guide_match || keyword_match {
            seen.insert(guide.to_string());
            let existing = existing_guides.iter().find(|g| g.name == guide);
            suggestions.push(GuideSuggestion {
                guide: guide.to_string(),
                category: category.to_string(),
                keywords: keywords.iter().map(|s| s.to_string()).collect(),
                tracked: existing.is_some(),
                usage_count: existing.map(|g| g.usage_count).unwrap_or(0),
                last_used: existing
                    .and_then(|g| g.last_used.map(|i| super::recall::date_only(i.as_millis()))),
                learnings: existing.map(|g| g.learnings.clone()).unwrap_or_default(),
                contexts: existing.map(|g| g.contexts.clone()).unwrap_or_default(),
            });
        }
    }

    for existing in existing_guides {
        if seen.contains(&existing.name) {
            continue;
        }
        if has_token_match(&desc_lower, &existing.name)
            || existing
                .contexts
                .iter()
                .any(|ctx| has_token_match(&desc_lower, ctx))
            || existing
                .learnings
                .iter()
                .any(|l| has_token_match(&desc_lower, l))
        {
            seen.insert(existing.name.clone());
            suggestions.push(GuideSuggestion {
                guide: existing.name.clone(),
                category: existing.category.clone(),
                keywords: existing.contexts.clone(),
                tracked: true,
                usage_count: existing.usage_count,
                last_used: existing
                    .last_used
                    .map(|i| super::recall::date_only(i.as_millis())),
                learnings: existing.learnings.clone(),
                contexts: existing.contexts.clone(),
            });
        }
    }
    suggestions
}

pub(crate) fn format_guide_suggestions(suggestions: &[GuideSuggestion]) -> String {
    let tracked: Vec<&GuideSuggestion> = suggestions.iter().filter(|s| s.tracked).collect();
    let missing: Vec<&GuideSuggestion> = suggestions.iter().filter(|s| !s.tracked).collect();
    let summary = format!(
        "Found {} relevant guides ({} tracked, {} new)",
        suggestions.len(),
        tracked.len(),
        missing.len()
    );
    let mut output = String::from("=== GUIDE SUGGESTIONS ===\n");
    output.push_str(&format!("{summary}\n\n"));
    if !tracked.is_empty() {
        output.push_str("TRACKED (you have experience):\n");
        for s in &tracked {
            output.push_str(&format!(
                "  ✓ [{}] {} ({}x, last: {})\n",
                s.category,
                s.guide,
                s.usage_count,
                s.last_used.as_deref().unwrap_or("n/a")
            ));
            if !s.learnings.is_empty() {
                for l in s.learnings.iter().take(3) {
                    output.push_str(&format!("      💡 {l}\n"));
                }
                if s.learnings.len() > 3 {
                    output.push_str(&format!(
                        "      ... and {} more learnings\n",
                        s.learnings.len() - 3
                    ));
                }
            }
        }
        output.push('\n');
    }
    if !missing.is_empty() {
        output.push_str("SUGGESTED (not tracked yet):\n");
        for s in &missing {
            output.push_str(&format!("  + [{}] {}\n", s.category, s.guide));
            if !s.keywords.is_empty() {
                output.push_str(&format!(
                    "      keywords: {}\n",
                    s.keywords
                        .iter()
                        .take(5)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
        output.push('\n');
    }
    if suggestions.is_empty() {
        output.push_str("No relevant guides found for this task.\n");
        output.push_str("Try describing the task with more specific terms.\n");
    }
    output.push_str("========================");
    output
}
