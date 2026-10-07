//! Host activation recipes (WP-10 task 8; T-HOST-01).
//!
//! Records how a host activates the ltmrs workflow: which skill path it must
//! discover and which command serves the tools. Entries are mechanism-based,
//! not host-version claims: per-host discovery behavior is host-defined and
//! can only be recorded per deployment. What IS pinned by tests: the skill
//! path (identical to the installer's layout) and the activation commands
//! (identical to the CLI surface).

/// How a host activates the ltmrs workflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostRecipe {
    /// Activation mechanism: `managed-skill-dir`, `mcp-stdio`, `visualizer`.
    pub mechanism: &'static str,
    /// Home-relative skill path the host must discover, or `None` when the
    /// mechanism serves tools/UI without a skill file.
    pub skill_path: Option<&'static str>,
    /// Exact command (or discovery rule) that activates the workflow.
    pub activation: &'static str,
    /// How the workflow runs once activated.
    pub workflow: &'static str,
    /// Evidence status: recipes document the supported activation; they are
    /// NOT verified host behavior (see module docs).
    pub status: &'static str,
}

/// Evidence status carried by every recipe (never a verified-host claim).
pub const RECIPE_STATUS: &str = "documented (supported recipe, not verified host behavior)";

/// The known activation recipes.
pub const HOST_RECIPES: &[HostRecipe] = &[
    HostRecipe {
        mechanism: "managed-skill-dir",
        skill_path: Some(".agents/skills/ltmrs/SKILL.md"),
        activation: "host discovers <home>/.agents/skills/ltmrs/SKILL.md",
        workflow: "skill teaches recall → act → persist; tools come from mcp-stdio",
        status: RECIPE_STATUS,
    },
    HostRecipe {
        mechanism: "mcp-stdio",
        skill_path: None,
        activation: "ltmrs [--socket PATH]",
        workflow: "host spawns `ltmrs` (no args) over stdio; serverInfo.name is `ltmrs`, tool namespaces are host-configured",
        status: RECIPE_STATUS,
    },
    HostRecipe {
        mechanism: "visualizer",
        skill_path: None,
        activation: "ltmrs -vis [--fg] [-p PORT]",
        workflow: "human opens the loopback URL the command prints",
        status: RECIPE_STATUS,
    },
];

/// Render the recipes as human-readable text (one block per mechanism).
pub fn host_recipes_text() -> String {
    let mut out = String::from("ltmrs host recipes\n");
    for recipe in HOST_RECIPES {
        out.push_str(&format!("\n## {}\n", recipe.mechanism));
        match recipe.skill_path {
            Some(path) => out.push_str(&format!("skill path: <home>/{path}\n")),
            None => out.push_str("skill path: (none — tools/UI only)\n"),
        }
        out.push_str(&format!("activation: {}\n", recipe.activation));
        out.push_str(&format!("workflow: {}\n", recipe.workflow));
        out.push_str(&format!("status: {}\n", recipe.status));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Recipe skill paths are identical to the installer's layout (a drift
    /// here means hosts look where we never install).
    #[test]
    fn recipes_pin_skill_path_to_installer() {
        let dir = tempfile::tempdir().unwrap();
        let installed = crate::skills::installer::skill_path(
            dir.path(),
            crate::skills::installer::LTMRS_SKILL.name,
        );
        for recipe in HOST_RECIPES.iter().filter(|r| r.skill_path.is_some()) {
            let claimed = dir.path().join(recipe.skill_path.unwrap());
            assert_eq!(
                claimed, installed,
                "recipe {} points where the installer writes",
                recipe.mechanism
            );
        }
        assert!(
            HOST_RECIPES.iter().any(|r| r.skill_path.is_some()),
            "at least one recipe must cover the managed skill"
        );
    }

    /// Every recipe names its activation and workflow (no placeholder rows).
    #[test]
    fn every_recipe_has_activation_and_workflow() {
        assert!(!HOST_RECIPES.is_empty());
        for recipe in HOST_RECIPES {
            assert!(!recipe.mechanism.is_empty(), "mechanism must be named");
            assert!(!recipe.activation.is_empty(), "activation must be named");
            assert!(!recipe.workflow.is_empty(), "workflow must be named");
        }
    }

    /// Statuses are documented recipes, never verified-host claims.
    #[test]
    fn statuses_are_documented_not_verified() {
        for recipe in HOST_RECIPES {
            assert_eq!(recipe.status, RECIPE_STATUS);
            assert!(
                recipe.status.contains("not verified"),
                "must disclaim verified host behavior, got: {}",
                recipe.status
            );
        }
    }

    /// The rendered text lists every mechanism plus the skill path.
    #[test]
    fn recipes_text_lists_all_mechanisms() {
        let text = host_recipes_text();
        for recipe in HOST_RECIPES {
            assert!(text.contains(recipe.mechanism), "got: {text}");
            assert!(text.contains(recipe.activation), "got: {text}");
        }
        assert!(
            text.contains(".agents/skills/ltmrs/SKILL.md"),
            "got: {text}"
        );
    }
}
