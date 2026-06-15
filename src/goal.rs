use crate::cap::Filesystem;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// The order of top-level keys in a goal TOML file. Referenced by the
/// tinker prompt (so it follows the schema) and by the
/// parse-error correction message (so tinker is told the
/// schema when fixing). Single source of truth.
pub const GOAL_SCHEMA_KEYS_ORDER: &str = "id, kind (optional), summary (optional), description, parent_id, children, related (optional), tier (optional)";

/// A cross-cutting relationship between two goals. Both ends of the link
/// must list each other (symmetric), but the reason text may differ because
/// the relationship reads differently from each side.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelatedLink {
    pub id: String,
    pub reason: String,
}

/// A child goal link. Serializes as `[[children]]` array-of-tables.
/// Backward-compat: old `children = ["id"]` plain-string arrays are accepted
/// during deserialization and loaded as `ChildLink { id, reason: "" }`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChildLink {
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
}

/// Accepts both old `children = ["id"]` (string) and new
/// `[[children]] { id, reason }` (table) TOML formats.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum ChildLinkRaw {
    String(String),
    Table(ChildLink),
}

fn deserialize_children<'de, D>(deserializer: D) -> Result<Vec<ChildLink>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    let raw: Vec<ChildLinkRaw> = Vec::deserialize(deserializer)?;
    Ok(raw
        .into_iter()
        .map(|r| match r {
            ChildLinkRaw::String(id) => ChildLink {
                id,
                reason: String::new(),
            },
            ChildLinkRaw::Table(link) => link,
        })
        .collect())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Goal {
    pub id: String,
    /// Goal kind: "feature" (owns a region of the artifact) or "behavior"
    /// (triggered — runs at certain points). Absent on unclassified/legacy
    /// goals. Surfaced in the compact index so the orchestrator can find the
    /// behavior goals and evaluate their triggers. See `goal-structure-standard`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// LLM-legible index entry in natural language, kind-flavored: a feature
    /// goal states what it owns ("This goal owns…"), a behavior goal states its
    /// trigger ("This goal runs when… / checks that… / ensures…"). Empty when absent.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub summary: String,
    pub description: String,
    #[serde(default)]
    pub parent_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty", deserialize_with = "deserialize_children")]
    pub children: Vec<ChildLink>,
    /// Cross-cutting related goals. Empty when the field is absent in TOML.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related: Vec<RelatedLink>,
    /// Model tier for sessions running this goal: "high", "mid", "low", or absent.
    /// When absent, behavior goals default to "high" and feature goals to "mid".
    /// Resolved at session start via model-config for the current backend.
    /// Tend, rummage, and jog declare `tier = "high"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
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

/// Builds a full-text listing of all goals for use with --tend-full-goal-context.
/// Goals are rendered in tree order with their complete description text.
pub fn build_full_text_index(goals: &[Goal]) -> String {
    let tree = build_tree(goals);
    let mut result = String::new();
    walk_full_text(&tree, &mut result, 0);
    result
}

fn walk_full_text(nodes: &[GoalNode], out: &mut String, depth: usize) {
    for node in nodes {
        let heading = "#".repeat(depth + 3);
        out.push_str(&format!("{} {}\n", heading, node.goal.id));
        if let Some(path) = &node.goal.source_path {
            out.push_str(&format!("Path: {}\n", path.display()));
        }
        if !node.goal.related.is_empty() {
            let rel = node.goal.related.iter()
                .map(|r| format!("{} ({})", r.id, r.reason))
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&format!("Related: {}\n", rel));
        }
        out.push('\n');
        out.push_str(node.goal.description.trim());
        out.push_str("\n\n");
        walk_full_text(&node.children, out, depth + 1);
    }
}

/// Builds the compact JSON index injected into tinker's context each turn.
/// One entry per root goal, children nested recursively. Only the `summary`
/// field is used as per-goal content; full text is available on demand via
/// the `path` field.
pub fn build_compact_index(goals: &[Goal]) -> String {
    #[derive(Serialize)]
    struct Entry {
        id: String,
        parent_id: String,
        #[serde(skip_serializing_if = "String::is_empty")]
        reason: String,
        #[serde(skip_serializing_if = "String::is_empty")]
        kind: String,
        summary: String,
        path: String,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        children: Vec<Entry>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        related: Vec<Related>,
    }
    #[derive(Serialize)]
    struct Related {
        id: String,
        reason: String,
    }

    fn node_to_entry(node: &GoalNode, parent_reason: &str) -> Entry {
        Entry {
            id: node.goal.id.clone(),
            parent_id: node.goal.parent_id.clone(),
            reason: parent_reason.to_string(),
            kind: node.goal.kind.clone().unwrap_or_default(),
            summary: node.goal.summary.clone(),
            path: node.goal.source_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| format!(".tinker/goals/{}.toml", node.goal.id)),
            children: node.children.iter().map(|child_node| {
                let reason = node.goal.children.iter()
                    .find(|cl| cl.id == child_node.goal.id)
                    .map(|cl| cl.reason.as_str())
                    .unwrap_or("");
                node_to_entry(child_node, reason)
            }).collect(),
            related: node.goal.related.iter().map(|r| Related {
                id: r.id.clone(),
                reason: r.reason.clone(),
            }).collect(),
        }
    }

    let tree = build_tree(goals);
    let entries: Vec<Entry> = tree.iter().map(|node| node_to_entry(node, "")).collect();
    serde_json::to_string(&entries)
        .expect("compact index serialization failed — this is a bug in the Entry schema")
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

    fn goal_toml_with_summary(id: &str, summary: &str, description: &str) -> String {
        format!(
            "id = \"{}\"\nsummary = \"{}\"\ndescription = \"\"\"\n{}\n\"\"\"\nparent_id = \"\"\nchildren = []\n",
            id, summary, description
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
            summary: "".into(),
            description: "do x".into(),
            parent_id: "".into(),
            children: vec![],
            related: vec![],
            tier: None,
            kind: None,
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
            Goal { id: "a".into(), summary: "".into(), description: "".into(), parent_id: "".into(), children: vec![], related: vec![], kind: None, tier: None, source_path: None },
            Goal { id: "b".into(), summary: "".into(), description: "".into(), parent_id: "".into(), children: vec![], related: vec![], kind: None, tier: None, source_path: None },
        ];
        let tree = build_tree(&goals);
        assert_eq!(tree.len(), 2);
        assert_eq!(tree[0].depth, 0);
        assert_eq!(tree[1].depth, 0);
    }

    #[test]
    fn test_spec_build_tree_parent_one_child() {
        let goals = vec![
            Goal { id: "parent".into(), summary: "".into(), description: "".into(), parent_id: "".into(), children: vec![], related: vec![], kind: None, tier: None, source_path: None },
            Goal { id: "child".into(), summary: "".into(), description: "".into(), parent_id: "parent".into(), children: vec![], related: vec![], kind: None, tier: None, source_path: None },
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
            Goal { id: "g".into(), summary: "".into(), description: "".into(), parent_id: "".into(), children: vec![], related: vec![], kind: None, tier: None, source_path: None },
            Goal { id: "p".into(), summary: "".into(), description: "".into(), parent_id: "g".into(), children: vec![], related: vec![], kind: None, tier: None, source_path: None },
            Goal { id: "c".into(), summary: "".into(), description: "".into(), parent_id: "p".into(), children: vec![], related: vec![], kind: None, tier: None, source_path: None },
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

        let result = load_all_goals(&fs, std::slice::from_ref(&tinker)).unwrap();
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
            summary: "".into(),
            description: "d".into(),
            parent_id: "".into(),
            children: vec![],
            related: vec![],
            tier: None,
            kind: None,
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
            Goal { id: "p".into(), summary: "".into(), description: "".into(), parent_id: "".into(), children: vec![ChildLink { id: "wrong".into(), reason: "".into() }], related: vec![], kind: None, tier: None, source_path: None },
            Goal { id: "c".into(), summary: "".into(), description: "".into(), parent_id: "p".into(), children: vec![], related: vec![], kind: None, tier: None, source_path: None },
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
        let summary = if g.related.is_empty() {
            String::new()
        } else {
            let links = g.related.iter()
                .map(|r| format!("{}: \"{}\"", r.id, r.reason))
                .collect::<Vec<_>>()
                .join(", ");
            format!(" [related: {}]", links)
        };
        assert!(summary.contains("b: \"cross-cuts b\""), "related link b must appear in summary");
        assert!(summary.contains("c: \"depends on c\""), "related link c must appear in summary");
    }

    // spec: compact-goal-context — a goal TOML without a `summary` field must
    // load with an empty summary string (not an error).
    #[test]
    fn test_spec_summary_field_absent_loads_as_empty() {
        let fs = MockFs::new();
        let tinker = PathBuf::from("/proj/.tinker");
        fs.add_file(&tinker.join("goals/g.toml"), &goal_toml("g", "desc"));

        let result = load_all_goals(&fs, &[tinker]).unwrap();
        assert_eq!(result.errors.len(), 0);
        let g = result.goals.iter().find(|g| g.id == "g").unwrap();
        assert!(g.summary.is_empty(), "missing summary must load as empty string");
    }

    // spec: compact-goal-context — a goal TOML with a `summary` field round-trips
    // through load; the field is omitted from serialization when empty.
    #[test]
    fn test_spec_summary_field_roundtrip() {
        let fs = MockFs::new();
        let tinker = PathBuf::from("/proj/.tinker");
        fs.add_file(
            &tinker.join("goals/g.toml"),
            &goal_toml_with_summary("g", "governs: foo; triggers: bar", "desc"),
        );

        let result = load_all_goals(&fs, &[tinker]).unwrap();
        assert_eq!(result.errors.len(), 0);
        let g = result.goals.iter().find(|g| g.id == "g").unwrap();
        assert_eq!(g.summary, "governs: foo; triggers: bar");
    }

    // spec: compact-goal-context — build_full_text_index renders goals with their
    // full description text in tree order; descriptions appear, summaries do not.
    #[test]
    fn test_spec_full_text_index_includes_description() {
        let goals = vec![
            Goal {
                id: "root".into(),
                summary: "governs: root domain".into(),
                description: "full description of root goal".into(),
                parent_id: "".into(),
                children: vec![],
                related: vec![],
                tier: None,
                kind: None,
                source_path: Some(PathBuf::from("/proj/.tinker/goals/root.toml")),
            },
            Goal {
                id: "child".into(),
                summary: "governs: child domain".into(),
                description: "full description of child goal".into(),
                parent_id: "root".into(),
                children: vec![],
                related: vec![],
                tier: None,
                kind: None,
                source_path: Some(PathBuf::from("/proj/.tinker/goals/child.toml")),
            },
        ];
        let index = build_full_text_index(&goals);
        // Full descriptions appear
        assert!(index.contains("full description of root goal"), "root description must appear");
        assert!(index.contains("full description of child goal"), "child description must appear");
        // Goal ids appear as headings
        assert!(index.contains("### root"), "root must appear as heading");
        // Child has deeper heading level
        assert!(index.contains("#### child"), "child must appear at depth+1 heading");
        // Path appears
        assert!(index.contains("root.toml"), "goal path must appear");
    }

    // spec: compact-goal-context — build_compact_index produces a JSON array
    // with nested children and related links; only summary (not description)
    // is included per entry.
    #[test]
    fn test_spec_compact_index_uses_summary_not_description() {
        let goals = vec![
            Goal {
                id: "root".into(),
                summary: "governs: root domain".into(),
                description: "full description text that must not appear".into(),
                parent_id: "".into(),
                children: vec![],
                related: vec![RelatedLink { id: "other".into(), reason: "cross-cuts".into() }],
                tier: None,
                kind: None,
                source_path: Some(PathBuf::from("/proj/.tinker/goals/root.toml")),
            },
            Goal {
                id: "child".into(),
                summary: "governs: child domain".into(),
                description: "child description that must not appear".into(),
                parent_id: "root".into(),
                children: vec![],
                related: vec![],
                tier: None,
                kind: None,
                source_path: Some(PathBuf::from("/proj/.tinker/goals/child.toml")),
            },
        ];
        let index = build_compact_index(&goals);
        // Summary fields are present
        assert!(index.contains("governs: root domain"), "root summary must be in index");
        assert!(index.contains("governs: child domain"), "child summary must be in index");
        // Full descriptions are absent
        assert!(!index.contains("full description text"), "description must not appear in index");
        assert!(!index.contains("child description"), "child description must not appear in index");
        // Children are nested
        assert!(index.contains("\"children\""), "children must appear in index");
        // Related links are present
        assert!(index.contains("\"related\""), "related must appear in index");
        assert!(index.contains("cross-cuts"), "related reason must appear in index");
    }

    // spec: compact-goal-context — each index entry must include `parent_id`
    // so an agent can navigate upward without loading the parent's full file.
    // Root goals have an empty parent_id; children carry their parent's id.
    #[test]
    fn test_spec_compact_index_includes_parent_id() {
        let goals = vec![
            Goal {
                id: "root".into(),
                summary: "governs: root".into(),
                description: "".into(),
                parent_id: "".into(),
                children: vec![],
                related: vec![],
                tier: None,
                kind: None,
                source_path: Some(PathBuf::from("/proj/.tinker/goals/root.toml")),
            },
            Goal {
                id: "child".into(),
                summary: "governs: child".into(),
                description: "".into(),
                parent_id: "root".into(),
                children: vec![],
                related: vec![],
                tier: None,
                kind: None,
                source_path: Some(PathBuf::from("/proj/.tinker/goals/child.toml")),
            },
        ];
        let index = build_compact_index(&goals);
        let parsed: serde_json::Value = serde_json::from_str(&index).unwrap();
        // Root entry has empty parent_id
        assert_eq!(parsed[0]["parent_id"], "", "root entry must have empty parent_id");
        // Child entry carries parent id
        let child = &parsed[0]["children"][0];
        assert_eq!(child["parent_id"], "root", "child entry must carry its parent's id");
    }

    // spec: goal-storage transition note — old `children = ["id"]` plain-string
    // arrays must load without error as ChildLink { id, reason: "" }.
    #[test]
    fn test_spec_children_old_string_format_loads_as_child_link() {
        let fs = MockFs::new();
        let tinker = PathBuf::from("/proj/.tinker");
        let toml_content = r#"id = "g"
description = "desc"
parent_id = ""
children = ["sub-a", "sub-b"]
"#;
        fs.add_file(&tinker.join("goals/g.toml"), toml_content);

        let result = load_all_goals(&fs, &[tinker]).unwrap();
        assert_eq!(result.errors.len(), 0, "old children format must not cause a parse error");
        let g = result.goals.iter().find(|g| g.id == "g").unwrap();
        assert_eq!(g.children.len(), 2);
        assert_eq!(g.children[0].id, "sub-a");
        assert_eq!(g.children[0].reason, "", "string-format entry must have empty reason");
        assert_eq!(g.children[1].id, "sub-b");
        assert_eq!(g.children[1].reason, "");
    }

    // spec: goal-storage — new `[[children]] { id, reason }` array-of-tables
    // format loads with id and reason preserved.
    #[test]
    fn test_spec_children_new_table_format_loads_correctly() {
        let fs = MockFs::new();
        let tinker = PathBuf::from("/proj/.tinker");
        let toml_content = r#"id = "g"
description = "desc"
parent_id = ""

[[children]]
id = "sub-a"
reason = "handles the alpha subproblem"

[[children]]
id = "sub-b"
reason = "handles the beta subproblem"
"#;
        fs.add_file(&tinker.join("goals/g.toml"), toml_content);

        let result = load_all_goals(&fs, &[tinker]).unwrap();
        assert_eq!(result.errors.len(), 0);
        let g = result.goals.iter().find(|g| g.id == "g").unwrap();
        assert_eq!(g.children.len(), 2);
        assert_eq!(g.children[0].id, "sub-a");
        assert_eq!(g.children[0].reason, "handles the alpha subproblem");
        assert_eq!(g.children[1].id, "sub-b");
        assert_eq!(g.children[1].reason, "handles the beta subproblem");
    }

    // spec: compact-goal-context — each nested child entry in the compact index
    // must carry the `reason` from the parent's [[children]] link for that child id.
    // This makes the navigation decision local to the index: the agent sees both
    // what the child governs (its summary) and why the parent has it (the reason).
    #[test]
    fn test_spec_compact_index_child_entries_carry_reason() {
        let goals = vec![
            Goal {
                id: "parent".into(),
                summary: "governs: parent domain".into(),
                description: "".into(),
                parent_id: "".into(),
                children: vec![ChildLink { id: "child".into(), reason: "handles the subproblem".into() }],
                related: vec![],
                tier: None,
                kind: None,
                source_path: Some(PathBuf::from("/proj/.tinker/goals/parent.toml")),
            },
            Goal {
                id: "child".into(),
                summary: "governs: child domain".into(),
                description: "".into(),
                parent_id: "parent".into(),
                children: vec![],
                related: vec![],
                tier: None,
                kind: None,
                source_path: Some(PathBuf::from("/proj/.tinker/goals/child.toml")),
            },
        ];
        let index = build_compact_index(&goals);
        let parsed: serde_json::Value = serde_json::from_str(&index).unwrap();
        let child_entry = &parsed[0]["children"][0];
        assert_eq!(child_entry["id"], "child", "child entry id must be correct");
        assert_eq!(
            child_entry["reason"], "handles the subproblem",
            "child entry must carry the reason from the parent's [[children]] link"
        );
        // Root entry must not have a reason field (empty string is skipped)
        assert!(
            parsed[0].get("reason").is_none() || parsed[0]["reason"] == "",
            "root entry must not carry a reason"
        );
    }

    // spec: goal-storage — a goal TOML without a `children` field loads with
    // an empty children vec (field is optional, same as `related`).
    #[test]
    fn test_spec_children_field_absent_loads_as_empty() {
        let fs = MockFs::new();
        let tinker = PathBuf::from("/proj/.tinker");
        let toml_content = "id = \"g\"\ndescription = \"desc\"\nparent_id = \"\"\n";
        fs.add_file(&tinker.join("goals/g.toml"), toml_content);

        let result = load_all_goals(&fs, &[tinker]).unwrap();
        assert_eq!(result.errors.len(), 0);
        let g = result.goals.iter().find(|g| g.id == "g").unwrap();
        assert!(g.children.is_empty(), "missing children field must load as empty vec");
    }

    // spec: goal-agents — a goal TOML without a `tier` field loads with tier = None
    // (optional field; effective tier resolved at session start, kind-aware).
    #[test]
    fn test_spec_tier_field_absent_loads_as_none() {
        let fs = MockFs::new();
        let tinker = PathBuf::from("/proj/.tinker");
        fs.add_file(&tinker.join("goals/g.toml"), &goal_toml("g", "desc"));

        let result = load_all_goals(&fs, &[tinker]).unwrap();
        assert_eq!(result.errors.len(), 0);
        let g = result.goals.iter().find(|g| g.id == "g").unwrap();
        assert!(g.tier.is_none(), "missing tier field must load as None");
    }

    // spec: goal-agents — a goal TOML with `tier = "high"` round-trips through load.
    // Tier field is omitted from serialization when None.
    #[test]
    fn test_spec_tier_field_roundtrip() {
        let fs = MockFs::new();
        let tinker = PathBuf::from("/proj/.tinker");
        let toml_content = "id = \"g\"\ndescription = \"desc\"\nparent_id = \"\"\ntier = \"high\"\n";
        fs.add_file(&tinker.join("goals/g.toml"), toml_content);

        let result = load_all_goals(&fs, &[tinker]).unwrap();
        assert_eq!(result.errors.len(), 0);
        let g = result.goals.iter().find(|g| g.id == "g").unwrap();
        assert_eq!(g.tier.as_deref(), Some("high"), "tier = \"high\" must load correctly");
    }

    // spec: goal-agents — serialized Goal omits the tier field when None.
    #[test]
    fn test_spec_tier_field_omitted_when_none() {
        let g = Goal {
            id: "x".into(),
            summary: "".into(),
            description: "d".into(),
            parent_id: "".into(),
            children: vec![],
            related: vec![],
            tier: None,
            kind: None,
            source_path: None,
        };
        let serialized = toml::to_string_pretty(&g).unwrap();
        assert!(
            !serialized.contains("tier"),
            "Goal must not write a tier field when None; got:\n{}",
            serialized,
        );
    }

    // spec: goal-structure-standard — a goal TOML without a `kind` field loads
    // with kind = None (optional; unclassified/legacy goals are tolerated).
    #[test]
    fn test_spec_kind_field_absent_loads_as_none() {
        let fs = MockFs::new();
        let tinker = PathBuf::from("/proj/.tinker");
        fs.add_file(&tinker.join("goals/g.toml"), &goal_toml("g", "desc"));

        let result = load_all_goals(&fs, &[tinker]).unwrap();
        assert_eq!(result.errors.len(), 0);
        let g = result.goals.iter().find(|g| g.id == "g").unwrap();
        assert!(g.kind.is_none(), "missing kind field must load as None");
    }

    // spec: goal-structure-standard — `kind = "behavior"` round-trips through
    // load; the field is omitted from serialization when None.
    #[test]
    fn test_spec_kind_field_roundtrip_and_omitted_when_none() {
        let fs = MockFs::new();
        let tinker = PathBuf::from("/proj/.tinker");
        let toml_content = "id = \"g\"\nkind = \"behavior\"\ndescription = \"desc\"\nparent_id = \"\"\n";
        fs.add_file(&tinker.join("goals/g.toml"), toml_content);

        let result = load_all_goals(&fs, &[tinker]).unwrap();
        assert_eq!(result.errors.len(), 0);
        let g = result.goals.iter().find(|g| g.id == "g").unwrap();
        assert_eq!(g.kind.as_deref(), Some("behavior"), "kind = \"behavior\" must load");

        let none_kind = Goal {
            id: "x".into(), kind: None, summary: "".into(), description: "d".into(),
            parent_id: "".into(), children: vec![], related: vec![], tier: None, source_path: None,
        };
        assert!(
            !toml::to_string_pretty(&none_kind).unwrap().contains("kind"),
            "Goal must not write a kind field when None",
        );
    }

    // spec: goal-structure-standard — the compact index surfaces each goal's
    // `kind` so the orchestrator can find behavior goals and evaluate their
    // triggers. Absent kind is omitted from the entry.
    #[test]
    fn test_spec_compact_index_includes_kind() {
        let goals = vec![
            Goal {
                id: "tui".into(), kind: Some("feature".into()),
                summary: "This goal owns the terminal UI.".into(),
                description: "".into(), parent_id: "".into(), children: vec![],
                related: vec![], tier: None,
                source_path: Some(PathBuf::from("/proj/.tinker/goals/tui.toml")),
            },
            Goal {
                id: "coding-standards".into(), kind: Some("behavior".into()),
                summary: "This goal runs when code changes.".into(),
                description: "".into(), parent_id: "".into(), children: vec![],
                related: vec![], tier: None,
                source_path: Some(PathBuf::from("/proj/.tinker/goals/coding-standards.toml")),
            },
        ];
        let index = build_compact_index(&goals);
        assert!(index.contains("\"kind\":\"feature\""), "feature kind must be in index; got:\n{}", index);
        assert!(index.contains("\"kind\":\"behavior\""), "behavior kind must be in index; got:\n{}", index);
    }
}
