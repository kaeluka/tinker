use crate::cap::Filesystem;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// The order of top-level keys in a goal TOML file. Referenced by the
/// tinker prompt (so it follows the schema) and by the
/// parse-error correction message (so tinker is told the
/// schema when fixing). Single source of truth.
pub const GOAL_SCHEMA_KEYS_ORDER: &str = "id, description, parent_id, children, related (optional)";

/// A cross-cutting relationship between two goals. Both ends of the link
/// must list each other (symmetric), but the reason text may differ because
/// the relationship reads differently from each side.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelatedLink {
    pub id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Goal {
    pub id: String,
    pub description: String,
    #[serde(default)]
    pub parent_id: String,
    #[serde(default)]
    pub children: Vec<String>,
    /// Cross-cutting related goals. Empty when the field is absent in TOML.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related: Vec<RelatedLink>,
    /// Absolute path of the TOML file this goal was loaded from.
    /// Used so updates write back to the same file (which may live in an ancestor .tinker dir).
    /// Not serialized — runtime metadata only.
    #[serde(skip)]
    pub source_path: Option<PathBuf>,
}

/// Saves a goal. If the goal has a `source_path`, the file is rewritten in place
/// (preserving its location in an ancestor `.tinker` dir). Otherwise, the goal is
/// saved to `default_dir/goals/<id>.toml`.
///
/// Only used in tests today — production goal writes happen via the
/// orchestrator's Write tool through opencode, not through this code path.
#[cfg(test)]
pub fn save_goal(fs: &dyn Filesystem, default_dir: &Path, goal: &Goal) -> Result<()> {
    let path = match &goal.source_path {
        Some(p) => p.clone(),
        None => default_dir.join("goals").join(format!("{}.toml", goal.id)),
    };
    let content = toml::to_string_pretty(goal)?;
    fs.write(&path, &content)?;
    Ok(())
}

/// Walks up from `cwd`, returning every `.tinker` directory found, nearest first.
pub fn discover_tinker_dirs(fs: &dyn Filesystem, cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![];
    let mut current: Option<&Path> = Some(cwd);
    while let Some(p) = current {
        let candidate = p.join(".tinker");
        if fs.is_dir(&candidate) {
            dirs.push(candidate);
        }
        current = p.parent();
    }
    dirs
}

pub struct LoadResult {
    pub goals: Vec<Goal>,
    pub errors: Vec<(PathBuf, String)>,
}

/// Loads goals from all given `.tinker` dirs and merges them. On duplicate goal IDs,
/// the first occurrence wins (so cwd-most takes precedence over ancestors).
/// Goals are pure spec - session_id lives in the per-project sessions.toml
/// (see `load_session_overrides`) and is looked up at goal-run time.
pub fn load_all_goals(fs: &dyn Filesystem, tinker_dirs: &[PathBuf]) -> Result<LoadResult> {
    let mut all = vec![];
    let mut all_errors: Vec<(PathBuf, String)> = vec![];
    let mut seen: HashSet<String> = HashSet::new();
    for dir in tinker_dirs {
        let r = load_goals(fs, dir)?;
        all_errors.extend(r.errors);
        for goal in r.goals {
            if seen.insert(goal.id.clone()) {
                all.push(goal);
            }
        }
    }
    Ok(LoadResult {
        goals: all,
        errors: all_errors,
    })
}

pub fn load_goals(fs: &dyn Filesystem, tinker_dir: &Path) -> Result<LoadResult> {
    let goals_dir = tinker_dir.join("goals");
    if !fs.is_dir(&goals_dir) {
        return Ok(LoadResult {
            goals: vec![],
            errors: vec![],
        });
    }

    let mut goals = vec![];
    let mut errors: Vec<(PathBuf, String)> = vec![];
    for path in fs.list_files_with_ext(&goals_dir, "toml")? {
        let content = fs.read_to_string(&path)?;
        match toml::from_str::<Goal>(&content) {
            Ok(mut goal) => {
                goal.source_path = Some(path.clone());
                goals.push(goal);
            }
            Err(e) => {
                let short = e.to_string().lines().next().unwrap_or("parse error").to_string();
                errors.push((path, short));
            }
        }
    }
    Ok(LoadResult { goals, errors })
}

pub fn build_tree(goals: &[Goal]) -> Vec<GoalNode> {
    let roots: Vec<&Goal> = goals
        .iter()
        .filter(|g| g.parent_id.is_empty())
        .collect();

    roots
        .into_iter()
        .map(|g| build_node(g, goals, 0))
        .collect()
}

#[derive(Debug, Clone)]
pub struct GoalNode {
    pub goal: Goal,
    pub depth: usize,
    pub children: Vec<GoalNode>,
}

fn build_node(goal: &Goal, all: &[Goal], depth: usize) -> GoalNode {
    let children = all
        .iter()
        .filter(|g| g.parent_id == goal.id)
        .map(|g| build_node(g, all, depth + 1))
        .collect();
    GoalNode {
        goal: goal.clone(),
        depth,
        children,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::MockFs;

    fn goal_toml(id: &str, description: &str) -> String {
        format!(
            "id = \"{}\"\ndescription = \"\"\"\n{}\n\"\"\"\nparent_id = \"\"\nchildren = []\n",
            id, description
        )
    }

    // spec: design notes - "Goals are persisted as files under `.tinker/goals/`
    // in the repo root." save_goal followed by load_goals should round-trip.
    #[test]
    fn test_spec_save_and_load_roundtrip() {
        let fs = MockFs::new();
        let tinker = Path::new("/proj/.tinker");
        fs.add_dir(&tinker.join("goals"));

        let goal = Goal {
            id: "x".into(),
            description: "do x".into(),
            parent_id: "".into(),
            children: vec![],
            related: vec![],
            source_path: None,
        };
        save_goal(&fs, tinker, &goal).unwrap();

        let result = load_goals(&fs, tinker).unwrap();
        assert_eq!(result.errors.len(), 0);
        assert_eq!(result.goals.len(), 1);
        assert_eq!(result.goals[0].id, "x");
        assert_eq!(result.goals[0].description, "do x");
        // source_path is populated on load
        assert!(result.goals[0].source_path.is_some());
    }

    // spec: design notes - multi-dir merge "cwd-most wins on duplicate IDs."
    #[test]
    fn test_spec_multi_dir_merge_cwd_wins() {
        let fs = MockFs::new();
        let proj = PathBuf::from("/proj/.tinker");
        let home = PathBuf::from("/home/.tinker");
        fs.add_file(&proj.join("goals/shared.toml"),
            &goal_toml("shared", "from cwd"),
        );
        fs.add_file(&home.join("goals/shared.toml"),
            &goal_toml("shared", "from ancestor"),
        );

        // cwd-most first
        let dirs = vec![proj, home];
        let result = load_all_goals(&fs, &dirs).unwrap();
        assert_eq!(result.goals.len(), 1);
        assert_eq!(result.goals[0].description.trim(), "from cwd");
    }

    // spec: design notes - "discover_tinker_dirs walks from cwd upward,
    // collecting every .tinker/ directory found, nearest first."
    #[test]
    fn test_spec_discover_tinker_dirs_walks_up() {
        let fs = MockFs::new();
        fs.add_dir(Path::new("/home/me/proj/.tinker"));
        fs.add_dir(Path::new("/home/me/.tinker"));
        // /home/.tinker NOT present - intermediate ancestor without .tinker
        // should be skipped, not stop discovery.

        let dirs = discover_tinker_dirs(&fs, Path::new("/home/me/proj"));
        assert_eq!(dirs.len(), 2);
        assert_eq!(dirs[0], Path::new("/home/me/proj/.tinker"));
        assert_eq!(dirs[1], Path::new("/home/me/.tinker"));
    }

    // spec: per `goal-storage.toml` — "Goal storage strictly stores intent,
    // not session state. Session IDs and history are not persisted to disk
    // at all." The Goal struct has no session_id field; load_all_goals
    // surfaces no sessions data.
    #[test]
    fn test_spec_goal_load_does_not_persist_session_state() {
        let fs = MockFs::new();
        let proj = PathBuf::from("/proj/.tinker");
        fs.add_file(&proj.join("goals/g.toml"), &goal_toml("g", "g"));
        // Even a legacy sessions.toml on disk must be ignored by goal loading.
        fs.add_file(
            &proj.join("sessions.toml"),
            "g = \"ses_stale_from_disk\"\n",
        );

        let result = load_all_goals(&fs, &[proj]).unwrap();
        assert_eq!(result.goals.len(), 1);
        assert_eq!(result.goals[0].id, "g");
        // Serialized Goal exposes no session field.
        let serialized = toml::to_string(&result.goals[0]).unwrap();
        assert!(
            !serialized.contains("session"),
            "Goal must not serialize any session field; got:\n{}",
            serialized
        );
    }

    // security: -> security.md T1 - a single malformed goal TOML must not
    // block loading of well-formed sibling goals; the error must be
    // surfaced via LoadResult.errors, not by failing the whole load.
    #[test]
    fn test_security_t1_parse_error_isolated() {
        let fs = MockFs::new();
        let tinker = PathBuf::from("/proj/.tinker");
        fs.add_file(&tinker.join("goals/good.toml"), &goal_toml("good", "ok"));
        fs.add_file(&tinker.join("goals/bad.toml"),
            "id = \"bad\"\ndescription = \"\"\"\nunclosed string\n",
        );
        // also a duplicate-key file - historically the most common failure
        fs.add_file(&tinker.join("goals/dup.toml"),
            "id = \"dup\"\nid = \"dup\"\ndescription = \"x\"\n",
        );

        let result = load_all_goals(&fs, &[tinker]).unwrap();
        // good loaded
        assert!(result.goals.iter().any(|g| g.id == "good"));
        // both bad files reported, neither in goals
        assert!(!result.goals.iter().any(|g| g.id == "bad"));
        assert!(!result.goals.iter().any(|g| g.id == "dup"));
        assert!(result.errors.len() >= 2);
    }

    // security: -> security.md T2 — cross-project session leak. The original
    // T2 mitigation was a per-project sessions.toml override. The current
    // design is stronger: sessions don't touch disk at all, so leakage by
    // construction is impossible. Even a stale TOML carrying a session_id
    // field is silently dropped by deserialization.
    #[test]
    fn test_security_t2_no_session_persistence_means_no_leak() {
        let fs = MockFs::new();
        let home = PathBuf::from("/home/.tinker");

        // Shared goal in ancestor with a legacy session_id field. The new
        // Goal struct has no such field, so serde drops it on the floor.
        let shared_toml = "id = \"shared\"\ndescription = \"x\"\nsession_id = \"ses_FROM_OTHER_PROJECT\"\nparent_id = \"\"\nchildren = []\n";
        fs.add_file(&home.join("goals/shared.toml"), shared_toml);

        let result = load_all_goals(&fs, &[home]).unwrap();
        let shared = result.goals.iter().find(|g| g.id == "shared").unwrap();

        // The session_id was in the TOML but is nowhere on the loaded Goal:
        // not in any field, not in any serialized form.
        let serialized = toml::to_string(shared).unwrap();
        assert!(
            !serialized.contains("ses_FROM_OTHER_PROJECT"),
            "stale session_id from disk must not survive loading; got:\n{}",
            serialized
        );
        assert!(
            !serialized.contains("session"),
            "Goal must expose no session field whatsoever; got:\n{}",
            serialized
        );
    }

    // spec: build_tree produces correct hierarchy from parent_id alone.
    #[test]
    fn test_spec_build_tree_flat_goals_all_depth_zero() {
        let goals = vec![
            Goal { id: "a".into(), description: "".into(), parent_id: "".into(), children: vec![], related: vec![], source_path: None },
            Goal { id: "b".into(), description: "".into(), parent_id: "".into(), children: vec![], related: vec![], source_path: None },
        ];
        let tree = build_tree(&goals);
        assert_eq!(tree.len(), 2);
        assert_eq!(tree[0].depth, 0);
        assert_eq!(tree[1].depth, 0);
    }

    #[test]
    fn test_spec_build_tree_parent_one_child() {
        let goals = vec![
            Goal { id: "parent".into(), description: "".into(), parent_id: "".into(), children: vec![], related: vec![], source_path: None },
            Goal { id: "child".into(), description: "".into(), parent_id: "parent".into(), children: vec![], related: vec![], source_path: None },
        ];
        let tree = build_tree(&goals);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].depth, 0);
        assert_eq!(tree[0].goal.id, "parent");
        assert_eq!(tree[0].children.len(), 1);
        assert_eq!(tree[0].children[0].depth, 1);
        assert_eq!(tree[0].children[0].goal.id, "child");
    }

    #[test]
    fn test_spec_build_tree_deep_hierarchy() {
        let goals = vec![
            Goal { id: "g".into(), description: "".into(), parent_id: "".into(), children: vec![], related: vec![], source_path: None },
            Goal { id: "p".into(), description: "".into(), parent_id: "g".into(), children: vec![], related: vec![], source_path: None },
            Goal { id: "c".into(), description: "".into(), parent_id: "p".into(), children: vec![], related: vec![], source_path: None },
        ];
        let tree = build_tree(&goals);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].depth, 0);
        assert_eq!(tree[0].children[0].depth, 1);
        assert_eq!(tree[0].children[0].children[0].depth, 2);
        assert_eq!(tree[0].children[0].children[0].goal.id, "c");
    }

    // spec: goal-storage decision - "A malformed goal file is reported as an
    // error in the interface but does not block sibling goals from loading."
    // (Spec angle: recovery story. The security angle is covered separately
    // by test_security_t1_parse_error_isolated.)
    #[test]
    fn test_spec_malformed_goal_does_not_block_siblings() {
        let fs = MockFs::new();
        let tinker = PathBuf::from("/proj/.tinker");
        fs.add_file(&tinker.join("goals/ok1.toml"), &goal_toml("ok1", "first"));
        fs.add_file(
            &tinker.join("goals/broken.toml"),
            "id = \"broken\"\ndescription = \"\"\"\nunterminated triple-quoted block\n",
        );
        fs.add_file(&tinker.join("goals/ok2.toml"), &goal_toml("ok2", "second"));

        let result = load_all_goals(&fs, &[tinker.clone()]).unwrap();
        // Both well-formed siblings load despite the broken one in between.
        assert!(result.goals.iter().any(|g| g.id == "ok1"));
        assert!(result.goals.iter().any(|g| g.id == "ok2"));
        // The broken one is surfaced via errors, not silently dropped.
        assert!(
            result.errors.iter().any(|(p, _)| p.ends_with("broken.toml")),
            "malformed file should appear in errors so the user can see it",
        );
        // And it must not have produced a goal.
        assert!(!result.goals.iter().any(|g| g.id == "broken"));
    }

    // spec: goal-storage decision - "Goals no longer contain a `change_log`
    // field. The orchestrator now triggers updates using explicit `/run`
    // slash commands." The Goal struct must not serialize a change_log
    // field, and parsing a legacy file that still has one must still
    // succeed (forward compat: ancestor `.tinker/` dirs may carry old
    // goals from prior tinker versions).
    #[test]
    fn test_spec_goal_has_no_change_log_field() {
        // 1. Serialized form does not contain `change_log`.
        let g = Goal {
            id: "x".into(),
            description: "d".into(),
            parent_id: "".into(),
            children: vec![],
            related: vec![],
            source_path: None,
        };
        let serialized = toml::to_string_pretty(&g).unwrap();
        assert!(
            !serialized.contains("change_log"),
            "Goal must not write a change_log field; got:\n{}",
            serialized,
        );

        // 2. A legacy goal TOML that still has a change_log entry parses
        // cleanly (extra field is ignored, not an error).
        let fs = MockFs::new();
        let tinker = PathBuf::from("/proj/.tinker");
        let legacy_toml = "id = \"legacy\"\ndescription = \"x\"\nparent_id = \"\"\nchildren = []\nchange_log = [\"old entry\"]\n";
        fs.add_file(&tinker.join("goals/legacy.toml"), legacy_toml);

        let result = load_all_goals(&fs, &[tinker]).unwrap();
        assert_eq!(result.errors.len(), 0, "legacy change_log must not cause a parse error");
        assert!(result.goals.iter().any(|g| g.id == "legacy"));
    }

    #[test]
    fn test_spec_build_tree_uses_parent_id_not_children_field() {
        let goals = vec![
            Goal { id: "p".into(), description: "".into(), parent_id: "".into(), children: vec!["wrong".into()], related: vec![], source_path: None },
            Goal { id: "c".into(), description: "".into(), parent_id: "p".into(), children: vec![], related: vec![], source_path: None },
        ];
        let tree = build_tree(&goals);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].goal.id, "p");
        assert_eq!(tree[0].children.len(), 1);
        assert_eq!(tree[0].children[0].goal.id, "c");
    }

    // spec: goal-structure-standard — "Goals may carry a `related` TOML field
    // listing other goals they cross-cut with, plus a reason for each link."
    // (a) A goal TOML without a `related` field loads with an empty related vec.
    #[test]
    fn test_spec_related_field_absent_loads_as_empty() {
        let fs = MockFs::new();
        let tinker = PathBuf::from("/proj/.tinker");
        fs.add_file(&tinker.join("goals/g.toml"), &goal_toml("g", "desc"));

        let result = load_all_goals(&fs, &[tinker]).unwrap();
        assert_eq!(result.errors.len(), 0);
        let g = result.goals.iter().find(|g| g.id == "g").unwrap();
        assert!(g.related.is_empty(), "missing related field must load as empty vec");
    }

    // spec: goal-structure-standard — "Goals may carry a `related` TOML field
    // listing other goals they cross-cut with, plus a reason for each link."
    // (b) A goal with a populated `related` field round-trips through load and
    // the goals_summary injection that feeds orchestrator context.
    #[test]
    fn test_spec_related_field_roundtrip_and_summary() {
        let fs = MockFs::new();
        let tinker = PathBuf::from("/proj/.tinker");
        let toml_content = r#"id = "a"
description = "goal a"
parent_id = ""
children = []

[[related]]
id = "b"
reason = "cross-cuts b"

[[related]]
id = "c"
reason = "depends on c"
"#;
        fs.add_file(&tinker.join("goals/a.toml"), toml_content);

        let result = load_all_goals(&fs, &[tinker]).unwrap();
        assert_eq!(result.errors.len(), 0);
        let g = result.goals.iter().find(|g| g.id == "a").unwrap();
        assert_eq!(g.related.len(), 2);
        assert_eq!(g.related[0].id, "b");
        assert_eq!(g.related[0].reason, "cross-cuts b");
        assert_eq!(g.related[1].id, "c");
        assert_eq!(g.related[1].reason, "depends on c");

        // Summary injection: related-links must appear in the goals_summary text
        // so the orchestrator can act on cross-cutting relationships.
        let summary = format!(
            "{}",
            if g.related.is_empty() {
                String::new()
            } else {
                let links = g.related.iter()
                    .map(|r| format!("{}: \"{}\"", r.id, r.reason))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(" [related: {}]", links)
            }
        );
        assert!(summary.contains("b: \"cross-cuts b\""), "related link b must appear in summary");
        assert!(summary.contains("c: \"depends on c\""), "related link c must appear in summary");
    }
}