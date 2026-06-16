use crate::cap::Filesystem;
use anyhow::Result;
use serde::{Deserialize, Serialize};
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

/// A single cross-tier goal-id collision discovered at load time. When the
/// same goal id is found at multiple `.tinker` discovery tiers, only the
/// first occurrence wins (cwd-most takes precedence); this struct records
/// every contributing tier and path so the runtime can warn the user that
/// a hidden override took place.
///
/// The first entry in `contributors` is the one that won; subsequent entries
/// are duplicates that were ignored. This ordering matches `load_all_goals`'s
/// first-occurrence-wins iteration over `tinker_dirs` (nearest first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalCollision {
    pub goal_id: String,
    /// Each entry is `(tier_label, path)` describing one of the contributing
    /// copies. Tier labels are `"project-local"` (tinker_dirs[0]),
    /// `"ancestor global"` (subsequent entries), `"packaged"` (loaded
    /// from a tinker_dir's `packaged-goals/` subdir — the tinker-shipped
    /// default vendored at the project), or `"binary-relative-packaged"`
    /// (loaded from `<binary>/../../.tinker/goals/packaged-goals/` — the
    /// tinker-shipped default that travels with the binary install).
    pub contributors: Vec<(String, PathBuf)>,
}

pub struct LoadResult {
    pub goals: Vec<Goal>,
    pub errors: Vec<(PathBuf, String)>,
    /// Goal ids found at multiple discovery tiers. Empty when every goal
    /// appears at exactly one tier. See `GoalCollision` for tier labels and
    /// ordering.
    pub collisions: Vec<GoalCollision>,
}

/// Loads goals from all given `.tinker` dirs and merges them. On duplicate goal IDs,
/// the first occurrence wins (so cwd-most takes precedence over ancestors).
/// Goals are pure spec - session_id lives in the per-project sessions.toml
/// (see `load_session_overrides`) and is looked up at goal-run time.
///
/// Cross-tier goal-id collisions are reported in `LoadResult.collisions`.
/// The winning copy's tier is `tinker_dirs[0]` (project-local); later
/// duplicates carry their tier label and path. The runtime emits a system
/// message per collision so silent overrides become visible.
///
/// Discovery is three-pass:
///   1. Regular goals from each `tinker_dir/goals/*.toml` (project-local at
///      the cwd-most tinker dir, and globally-installed at ancestor tinker
///      dirs). Cwd-most wins on duplicates within this pass.
///   2. Packaged goals from each `tinker_dir/goals/packaged-goals/*.toml`,
///      which are tinker-shipped defaults vendored at each tinker_dir.
///      These land after the regular pass, so the regular-pass goals
///      always win on duplicate IDs.
///   3. Binary-relative packaged goals from
///      `<binary>/../../.tinker/goals/packaged-goals/`, resolved from
///      `std::env::current_exe()`. These are tinker-shipped defaults that
///      travel with the binary install — the install location is separate
///      from the tinker_dir-relative path in dev mode (a dev binary at
///      `<project>/target/debug/tinker` resolves to
///      `<project>/target/.tinker/goals/packaged-goals/`, not the
///      project's own `.tinker/goals/packaged-goals/`). They land last,
///      so all earlier passes win on duplicate IDs. Skipped silently
///      when `current_exe()` fails to resolve.
///
/// Classification (project-local vs. packaged) is done at render time by
/// `App::is_packaged` using the source_path — the directory-convention
/// signal that `goal-placement` writes and `packaged-goals-style` reads.
pub fn load_all_goals(fs: &dyn Filesystem, tinker_dirs: &[PathBuf]) -> Result<LoadResult> {
    // Resolve the running binary's path once at the top. Used by the
    // binary-relative packaged pass. If resolution fails (sandboxed
    // environments, exotic filesystems) the pass is silently skipped;
    // packaged defaults still come from the tinker_dir passes.
    let exe_path = std::env::current_exe().ok();
    load_all_goals_with_exe(fs, tinker_dirs, exe_path.as_deref())
}

/// Inner implementation of `load_all_goals` that accepts the binary path
/// explicitly. Split out so tests can drive the binary-relative pass with
/// a mock path instead of relying on the test binary's actual location.
/// The public `load_all_goals` resolves `current_exe()` and delegates here.
fn load_all_goals_with_exe(
    fs: &dyn Filesystem,
    tinker_dirs: &[PathBuf],
    exe_path: Option<&Path>,
) -> Result<LoadResult> {
    let mut all = vec![];
    let mut all_errors: Vec<(PathBuf, String)> = vec![];
    // Map from goal id to the (tier, path) of its first occurrence; later
    // duplicates are recorded in `collision_paths` rather than overriding.
    let mut first_seen: std::collections::HashMap<String, (String, PathBuf)> =
        std::collections::HashMap::new();
    let mut collision_paths: std::collections::HashMap<String, Vec<(String, PathBuf)>> =
        std::collections::HashMap::new();

    // Pass 1: regular goals at the top level of each `.tinker/goals/` dir.
    // Tier label is dir-based (project-local at tinker_dirs[0], ancestor
    // global beyond). Cwd-most wins on duplicates.
    for dir in tinker_dirs {
        let tier = tier_label(dir, tinker_dirs).to_string();
        let r = load_goals(fs, dir)?;
        all_errors.extend(r.errors);
        for goal in r.goals {
            record_loaded_goal(&mut all, &mut first_seen, &mut collision_paths, goal, &tier);
        }
    }

    // Pass 2: packaged goals at the `packaged-goals/` subdir of each
    // tinker_dir. These are tinker-shipped defaults vendored at each
    // tinker_dir. Tier label is fixed ("packaged") because the on-disk
    // convention is the classification signal, not the tinker_dir
    // hierarchy. Pass-1 wins on duplicates.
    for dir in tinker_dirs {
        let r = load_packaged_goals(fs, dir)?;
        all_errors.extend(r.errors);
        for goal in r.goals {
            record_loaded_goal(
                &mut all,
                &mut first_seen,
                &mut collision_paths,
                goal,
                PACKAGED_TIER,
            );
        }
    }

    // Pass 3: binary-relative packaged goals. The spec convention is
    // `<binary>/../../.tinker/goals/packaged-goals/` — tinker-shipped
    // defaults that travel with the install. Tier label is
    // `"binary-relative-packaged"`, distinct from Pass 2's `"packaged"`
    // so the diagnostic can tell apart a vendored project-local packaged
    // copy from a binary-shipped default. Pass-1 and Pass-2 win on
    // duplicates (first-occurrence-wins in the merge state). Skipped
    // silently when `exe_path` is `None` (binary path unresolved).
    if let Some(exe_path) = exe_path {
        let r = load_binary_relative_packaged_goals(fs, exe_path)?;
        all_errors.extend(r.errors);
        for goal in r.goals {
            record_loaded_goal(
                &mut all,
                &mut first_seen,
                &mut collision_paths,
                goal,
                BINARY_RELATIVE_PACKAGED_TIER,
            );
        }
    }

    // Merge the first-occurrence entry and the duplicate entries into a
    // single ordered `contributors` list per collided goal id, then sort
    // the collision list by id for deterministic output.
    let mut collisions: Vec<GoalCollision> = collision_paths
        .into_iter()
        .filter_map(|(goal_id, mut extras)| {
            let (first_tier, first_path) = first_seen.get(&goal_id)?.clone();
            let mut contributors = vec![(first_tier, first_path)];
            contributors.append(&mut extras);
            Some(GoalCollision { goal_id, contributors })
        })
        .collect();
    collisions.sort_by(|a, b| a.goal_id.cmp(&b.goal_id));

    Ok(LoadResult {
        goals: all,
        errors: all_errors,
        collisions,
    })
}

/// Tier label for goals loaded from a tinker_dir's `packaged-goals/` subdir.
/// Fixed string — packaged-ness is a directory-convention signal, not a
/// tinker_dir-hierarchy signal. See `App::is_packaged` and
/// `packaged-goals-style` for the rendering rules that consume it.
pub const PACKAGED_TIER: &str = "packaged";

/// Tier label for goals loaded from the binary-relative
/// `<binary>/../../.tinker/goals/packaged-goals/` directory. Distinct
/// from `PACKAGED_TIER` so the collision diagnostic can tell apart a
/// vendored project-local packaged copy (Pass 2) from a binary-shipped
/// default (Pass 3) when both are present in dev mode.
pub const BINARY_RELATIVE_PACKAGED_TIER: &str = "binary-relative-packaged";

/// Records a freshly loaded `goal` into the merge state. Shared between
/// the regular and packaged passes of `load_all_goals` so the collision
/// accounting stays in one place.
///
/// Collision policy: a goal id seen at a *different* tier than its first
/// occurrence is a cross-tier collision that must be reported. A goal id
/// seen at the *same* tier (intra-tier duplicate, e.g. two TOML files with
/// the same id in one `.tinker/goals/`) is silently deduped to the first
/// occurrence — matching the existing `load_goals` first-occurrence-wins
/// behavior, and outside the scope of the cross-tier diagnostic.
fn record_loaded_goal(
    all: &mut Vec<Goal>,
    first_seen: &mut std::collections::HashMap<String, (String, PathBuf)>,
    collision_paths: &mut std::collections::HashMap<String, Vec<(String, PathBuf)>>,
    goal: Goal,
    tier: &str,
) {
    let path = goal.source_path.clone().unwrap_or_default();
    if let Some((first_tier, _first_path)) = first_seen.get(&goal.id).cloned() {
        if first_tier != tier {
            // Cross-tier collision: different tier, hidden override.
            // Record the contributing copy for the diagnostic; do not
            // override the winning copy.
            collision_paths
                .entry(goal.id.clone())
                .or_default()
                .push((tier.to_string(), path));
        }
        // Same-tier duplicate: silent first-wins, no collision report.
        // The first occurrence is already in `all`; this duplicate is
        // discarded, matching the original `load_goals` first-encountered
        // semantics.
    } else {
        first_seen.insert(goal.id.clone(), (tier.to_string(), path));
        all.push(goal);
    }
}

/// Labels a tinker dir with its discovery tier relative to the others in
/// the same load. `tinker_dirs[0]` is the primary (project-local) tier;
/// every later entry is an ancestor. Packaged goals are labeled separately
/// via `PACKAGED_TIER` because the packaged-tier signal is the
/// `packaged-goals/` subdirectory, not the tinker_dir hierarchy.
fn tier_label(dir: &Path, tinker_dirs: &[PathBuf]) -> &'static str {
    if tinker_dirs.first() == Some(&dir.to_path_buf()) {
        "project-local"
    } else {
        "ancestor global"
    }
}

pub fn load_goals(fs: &dyn Filesystem, tinker_dir: &Path) -> Result<LoadResult> {
    let goals_dir = tinker_dir.join("goals");
    if !fs.is_dir(&goals_dir) {
        return Ok(LoadResult {
            goals: vec![],
            errors: vec![],
            collisions: vec![],
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
    Ok(LoadResult { goals, errors, collisions: vec![] })
}

/// Loads packaged goals from `<tinker_dir>/goals/packaged-goals/*.toml`. The
/// `packaged-goals/` subdirectory is the on-disk signal that a goal is
/// tinker-shipped content (universal methodology) rather than user-authored
/// project-local content. Returns an empty `LoadResult` when the
/// subdirectory does not exist; reports per-file parse errors via
/// `LoadResult.errors` for any malformed TOML in the subdirectory, leaving
/// well-formed siblings loadable.
///
/// This is the second-pass loader for `load_all_goals`; it is not intended
/// to be called on its own. Use `load_all_goals` to get the merged result
/// with the proper project-local > ancestor-installed > packaged precedence.
pub fn load_packaged_goals(fs: &dyn Filesystem, tinker_dir: &Path) -> Result<LoadResult> {
    let packaged_dir = tinker_dir.join("goals").join("packaged-goals");
    if !fs.is_dir(&packaged_dir) {
        return Ok(LoadResult {
            goals: vec![],
            errors: vec![],
            collisions: vec![],
        });
    }

    let mut goals = vec![];
    let mut errors: Vec<(PathBuf, String)> = vec![];
    for path in fs.list_files_with_ext(&packaged_dir, "toml")? {
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
    Ok(LoadResult { goals, errors, collisions: vec![] })
}

/// Loads packaged goals from the binary-relative
/// `<binary>/../../.tinker/goals/packaged-goals/` directory. This is the
/// third-pass loader for `load_all_goals`, after the regular and the
/// tinker_dir-relative packaged passes. The path convention is the spec'd
/// one: parent of the binary's parent dir, then `.tinker/goals/packaged-goals/`.
///
/// In installed mode (e.g., binary at `/usr/bin/tinker` or
/// `~/.cargo/bin/tinker`) the resolved path is
/// `/usr/.tinker/goals/packaged-goals/` or
/// `~/.cargo/.tinker/goals/packaged-goals/` — a user-level install
/// location for binary-shipped defaults. The directory is expected to
/// be populated by the install script (e.g., a Cargo `include_dir!` or
/// a post-install step that copies from the source repo's
/// `packaged-goals/`).
///
/// In dev mode the binary at `<project>/target/debug/tinker` resolves to
/// `<project>/target/.tinker/goals/packaged-goals/` — a *different*
/// directory from the project's own `.tinker/goals/packaged-goals/`
/// that `load_packaged_goals` walks. The two passes don't overlap in
/// dev mode (the binary-relative dir typically doesn't exist until
/// after install). When both passes do find the same goal id (a
/// deliberate dev-setup case or a user-customized install), the
/// first-occurrence-wins merge in `load_all_goals` makes the
/// duplication harmless: whichever pass runs first claims the id, the
/// other sees it as a collision with `BINARY_RELATIVE_PACKAGED_TIER`
/// as the contributor label.
///
/// Returns an empty `LoadResult` when the resolved directory does not
/// exist; reports per-file parse errors via `LoadResult.errors` for any
/// malformed TOML, leaving well-formed siblings loadable.
pub fn load_binary_relative_packaged_goals(
    fs: &dyn Filesystem,
    exe_path: &Path,
) -> Result<LoadResult> {
    let packaged_dir = binary_relative_packaged_dir(exe_path);
    if !fs.is_dir(&packaged_dir) {
        return Ok(LoadResult {
            goals: vec![],
            errors: vec![],
            collisions: vec![],
        });
    }

    let mut goals = vec![];
    let mut errors: Vec<(PathBuf, String)> = vec![];
    for path in fs.list_files_with_ext(&packaged_dir, "toml")? {
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
    Ok(LoadResult { goals, errors, collisions: vec![] })
}

/// Resolves the binary-relative packaged-goals directory from the tinker
/// binary's path. The convention is
/// `<binary>/../../.tinker/goals/packaged-goals/` — i.e., two directory
/// pops: the binary name, then the binary's parent dir (typically
/// `bin/` or `target/debug/`). If the path doesn't have enough
/// components to pop twice, the function returns the partial path
/// (possibly empty) — the caller checks `fs.is_dir` and returns an
/// empty result if the directory is absent. No panic on short paths.
///
/// Examples:
/// - `/usr/bin/tinker`                       → `/usr/.tinker/goals/packaged-goals/`
/// - `/home/me/proj/target/debug/tinker`     → `/home/me/proj/target/.tinker/goals/packaged-goals/`
/// - `/home/me/.cargo/bin/tinker`            → `/home/me/.cargo/.tinker/goals/packaged-goals/`
/// - `tinker` (no parent dir)                → `""` (after one pop, then the second pop fails; caller sees an empty path and skips)
/// - `/tinker`                               → `/.tinker/goals/packaged-goals/` (one pop succeeds, one fails; caller rejects the system path)
fn binary_relative_packaged_dir(exe_path: &Path) -> PathBuf {
    let mut path = exe_path.to_path_buf();
    // Pop the binary name to get the binary's directory.
    if !path.pop() {
        return path;
    }
    // Pop the parent directory (typically `bin/` or `target/debug/`).
    if !path.pop() {
        return path;
    }
    path.push(".tinker");
    path.push("goals");
    path.push("packaged-goals");
    path
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

    // spec: goal-agents startup diagnostic — "At session-registry population,
    // emit a system message for each goal id found in multiple discovery
    // tiers, listing the contributing tiers and paths." `load_all_goals` must
    // surface the collision data; this test verifies (a) the winner is
    // project-local, (b) both contributors are recorded with their tier
    // labels, and (c) the loaded goal itself is the winning copy.
    #[test]
    fn test_spec_load_all_goals_reports_cross_tier_collision() {
        let fs = MockFs::new();
        let proj = PathBuf::from("/proj/.tinker");
        let home = PathBuf::from("/home/.tinker");
        fs.add_file(&proj.join("goals/shared.toml"),
            &goal_toml("shared", "from cwd"),
        );
        fs.add_file(&home.join("goals/shared.toml"),
            &goal_toml("shared", "from ancestor"),
        );

        let dirs = vec![proj.clone(), home.clone()];
        let result = load_all_goals(&fs, &dirs).unwrap();
        assert_eq!(result.goals.len(), 1);
        // Winner is the project-local copy.
        assert_eq!(result.goals[0].description.trim(), "from cwd");
        // Collision is reported with both contributing tiers in the order
        // they were encountered (winner first, then each ignored duplicate).
        assert_eq!(result.collisions.len(), 1);
        let c = &result.collisions[0];
        assert_eq!(c.goal_id, "shared");
        assert_eq!(c.contributors.len(), 2);
        assert_eq!(c.contributors[0].0, "project-local");
        assert_eq!(c.contributors[0].1, PathBuf::from("/proj/.tinker/goals/shared.toml"));
        assert_eq!(c.contributors[1].0, "ancestor global");
        assert_eq!(c.contributors[1].1, PathBuf::from("/home/.tinker/goals/shared.toml"));
    }

    // spec: goal-agents startup diagnostic — collisions are reported in a
    // stable order (sorted by goal id) so a watcher re-load emits the same
    // messages in the same order each cycle.
    #[test]
    fn test_spec_load_all_goals_collisions_sorted_by_goal_id() {
        let fs = MockFs::new();
        let proj = PathBuf::from("/proj/.tinker");
        let home = PathBuf::from("/home/.tinker");
        // Two collisions; insertion order in the file system is zeta-then-alpha.
        fs.add_file(&proj.join("goals/zeta.toml"), &goal_toml("zeta", "from cwd"));
        fs.add_file(&home.join("goals/zeta.toml"), &goal_toml("zeta", "from ancestor"));
        fs.add_file(&proj.join("goals/alpha.toml"), &goal_toml("alpha", "from cwd"));
        fs.add_file(&home.join("goals/alpha.toml"), &goal_toml("alpha", "from ancestor"));

        let dirs = vec![proj, home];
        let result = load_all_goals(&fs, &dirs).unwrap();
        assert_eq!(result.collisions.len(), 2);
        // Sorted by goal id, regardless of file-system order.
        assert_eq!(result.collisions[0].goal_id, "alpha");
        assert_eq!(result.collisions[1].goal_id, "zeta");
    }

    // spec: goal-agents startup diagnostic — a goal id that appears at
    // exactly one tier is not a collision. `collisions` is empty in the
    // common case of disjoint goal sets across tiers.
    #[test]
    fn test_spec_load_all_goals_no_collisions_when_disjoint() {
        let fs = MockFs::new();
        let proj = PathBuf::from("/proj/.tinker");
        let home = PathBuf::from("/home/.tinker");
        fs.add_file(&proj.join("goals/local.toml"), &goal_toml("local", "p"));
        fs.add_file(&home.join("goals/ancestor.toml"), &goal_toml("ancestor", "h"));

        let dirs = vec![proj, home];
        let result = load_all_goals(&fs, &dirs).unwrap();
        assert_eq!(result.goals.len(), 2);
        assert!(result.collisions.is_empty(),
            "non-overlapping goal sets must not produce collisions");
    }

    // spec: goal-agents startup diagnostic — the same goal id appearing in
    // the same tier twice (e.g. duplicate files within one dir) is a
    // first-occurrence-wins situation, but it is NOT a cross-tier collision
    // because both copies are in the same tier. The `collisions` vec is
    // therefore empty: the diagnostic covers cross-tier overlap only.
    #[test]
    fn test_spec_load_all_goals_no_collision_for_intra_tier_duplicates() {
        let fs = MockFs::new();
        let proj = PathBuf::from("/proj/.tinker");
        // Two toml files in the same dir with the same id — the first
        // encountered during `list_files_with_ext` wins, but no cross-tier
        // collision should be reported.
        fs.add_file(&proj.join("goals/a-dup1.toml"), &goal_toml("a", "first"));
        fs.add_file(&proj.join("goals/a-dup2.toml"), &goal_toml("a", "second"));

        let dirs = vec![proj];
        let result = load_all_goals(&fs, &dirs).unwrap();
        assert_eq!(result.goals.len(), 1);
        assert!(result.collisions.is_empty(),
            "intra-tier duplicates must not be reported as cross-tier collisions");
    }

    // spec: goal-agents startup diagnostic — `tier_label` must label
    // `tinker_dirs[0]` as `"project-local"` and every other entry as
    // `"ancestor global"`. This is the single source of truth for tier
    // naming that the system message relies on.
    #[test]
    fn test_spec_tier_label_distinguishes_primary_from_ancestor() {
        let proj = PathBuf::from("/proj/.tinker");
        let home = PathBuf::from("/home/.tinker");
        let dirs = vec![proj.clone(), home.clone()];
        assert_eq!(tier_label(&proj, &dirs), "project-local");
        assert_eq!(tier_label(&home, &dirs), "ancestor global");
    }

    // spec: goal-storage — `load_packaged_goals` finds every `.toml` file in
    // `<tinker_dir>/goals/packaged-goals/`, populates `source_path`, and
    // surfaces parse errors via `LoadResult.errors` without blocking
    // well-formed siblings. The packaged subdirectory is the on-disk
    // signal for tinker-shipped (universal) content.
    #[test]
    fn test_spec_load_packaged_goals_finds_packaged_subdir() {
        let fs = MockFs::new();
        let tinker = PathBuf::from("/proj/.tinker");
        fs.add_file(
            &tinker.join("goals/packaged-goals/tend.toml"),
            &goal_toml("tend", "the shipped tend"),
        );
        fs.add_file(
            &tinker.join("goals/packaged-goals/rummage.toml"),
            &goal_toml("rummage", "the shipped rummage"),
        );

        let result = load_packaged_goals(&fs, &tinker).unwrap();
        assert_eq!(result.errors.len(), 0);
        assert_eq!(result.goals.len(), 2);
        let ids: Vec<&str> = result.goals.iter().map(|g| g.id.as_str()).collect();
        assert!(ids.contains(&"tend"));
        assert!(ids.contains(&"rummage"));
        // Every loaded goal has a `packaged-goals/` source_path so
        // `App::is_packaged` can classify it as packaged.
        for g in &result.goals {
            let p = g.source_path.as_ref().expect("source_path populated");
            assert!(
                p.components().any(|c| c.as_os_str() == "packaged-goals"),
                "packaged-goals/ subdir must be in source_path; got {p:?}"
            );
        }
    }

    // spec: goal-storage — when the `packaged-goals/` subdir does not exist
    // the loader returns an empty result without error. The function must
    // not require the subdir's presence — it is opt-in for tinker_dirs that
    // ship default content.
    #[test]
    fn test_spec_load_packaged_goals_returns_empty_when_subdir_absent() {
        let fs = MockFs::new();
        let tinker = PathBuf::from("/proj/.tinker");
        // No `packaged-goals/` subdir at all.
        let result = load_packaged_goals(&fs, &tinker).unwrap();
        assert_eq!(result.errors.len(), 0);
        assert!(result.goals.is_empty());
    }

    // spec: goal-storage — a malformed goal TOML in `packaged-goals/` does
    // not block loading of well-formed siblings; the error is surfaced
    // via `LoadResult.errors`. Mirrors the security T1 invariant for
    // regular goals but scoped to the packaged subdir.
    #[test]
    fn test_spec_load_packaged_goals_malformed_does_not_block_siblings() {
        let fs = MockFs::new();
        let tinker = PathBuf::from("/proj/.tinker");
        fs.add_file(
            &tinker.join("goals/packaged-goals/ok.toml"),
            &goal_toml("ok", "fine"),
        );
        fs.add_file(
            &tinker.join("goals/packaged-goals/broken.toml"),
            "id = \"broken\"\ndescription = \"\"\"\nunterminated triple-quoted block\n",
        );

        let result = load_packaged_goals(&fs, &tinker).unwrap();
        assert!(result.goals.iter().any(|g| g.id == "ok"));
        assert!(!result.goals.iter().any(|g| g.id == "broken"));
        assert!(
            result.errors.iter().any(|(p, _)| p.ends_with("broken.toml")),
            "malformed packaged file must surface in errors"
        );
    }

    // spec: goal-storage — `load_all_goals` applies the three-tier
    // precedence: project-local > ancestor-installed > packaged. A goal id
    // present in all three tiers must land as the project-local copy, with
    // the ancestor and packaged copies recorded as collision contributors.
    #[test]
    fn test_spec_load_all_goals_three_tier_precedence() {
        let fs = MockFs::new();
        let proj = PathBuf::from("/proj/.tinker");
        let home = PathBuf::from("/home/.tinker");
        // All three tiers have a "shared" goal with different descriptions.
        fs.add_file(&proj.join("goals/shared.toml"),  &goal_toml("shared", "from proj"));
        fs.add_file(&home.join("goals/shared.toml"),  &goal_toml("shared", "from home"));
        fs.add_file(
            &proj.join("goals/packaged-goals/shared.toml"),
            &goal_toml("shared", "from packaged"),
        );

        let dirs = vec![proj.clone(), home.clone()];
        let result = load_all_goals(&fs, &dirs).unwrap();
        // Winner is the project-local copy.
        assert_eq!(result.goals.len(), 1);
        assert_eq!(result.goals[0].description.trim(), "from proj");
        // All three tiers recorded as collision contributors, in
        // load-order (project-local first, then ancestor, then packaged).
        assert_eq!(result.collisions.len(), 1);
        let c = &result.collisions[0];
        assert_eq!(c.goal_id, "shared");
        assert_eq!(c.contributors.len(), 3);
        assert_eq!(c.contributors[0].0, "project-local");
        assert_eq!(c.contributors[0].1, PathBuf::from("/proj/.tinker/goals/shared.toml"));
        assert_eq!(c.contributors[1].0, "ancestor global");
        assert_eq!(c.contributors[1].1, PathBuf::from("/home/.tinker/goals/shared.toml"));
        assert_eq!(c.contributors[2].0, "packaged");
        assert_eq!(
            c.contributors[2].1,
            PathBuf::from("/proj/.tinker/goals/packaged-goals/shared.toml")
        );
    }

    // spec: goal-storage — when a goal id is present only in the packaged
    // subdir of the project's own tinker_dir, `load_all_goals` still loads
    // it (the packaged pass is a "fill in the default" pass when no
    // project-local or ancestor copy exists).
    #[test]
    fn test_spec_load_all_goals_packaged_default_fills_in_when_no_override() {
        let fs = MockFs::new();
        let proj = PathBuf::from("/proj/.tinker");
        // No project-local or ancestor copy — only the packaged default.
        fs.add_file(
            &proj.join("goals/packaged-goals/feature-craft.toml"),
            &goal_toml("feature-craft", "shipped default"),
        );

        let dirs = vec![proj];
        let result = load_all_goals(&fs, &dirs).unwrap();
        assert_eq!(result.goals.len(), 1);
        assert_eq!(result.goals[0].id, "feature-craft");
        assert_eq!(result.goals[0].description.trim(), "shipped default");
        assert!(result.collisions.is_empty(),
            "no collision when only the packaged copy exists");
    }

    // spec: goal-storage — a packaged goal loaded by `load_all_goals` has
    // a source_path inside `packaged-goals/`, so `App::is_packaged`
    // classifies it correctly via the directory-convention signal.
    // The render-time classification is not exercised here, but the
    // source_path contract is.
    #[test]
    fn test_spec_load_all_goals_packaged_goal_has_packaged_source_path() {
        let fs = MockFs::new();
        let proj = PathBuf::from("/proj/.tinker");
        fs.add_file(
            &proj.join("goals/packaged-goals/tend.toml"),
            &goal_toml("tend", "shipped"),
        );
        // A non-packaged goal in the same tinker_dir — covers the case
        // where both regular and packaged live side by side.
        fs.add_file(&proj.join("goals/local.toml"), &goal_toml("local", "p"));

        let dirs = vec![proj];
        let result = load_all_goals(&fs, &dirs).unwrap();
        let tend = result.goals.iter().find(|g| g.id == "tend").unwrap();
        let tend_path = tend.source_path.as_ref().unwrap();
        assert!(
            tend_path.components().any(|c| c.as_os_str() == "packaged-goals"),
            "tend's source_path must be inside packaged-goals/"
        );
        let local = result.goals.iter().find(|g| g.id == "local").unwrap();
        let local_path = local.source_path.as_ref().unwrap();
        assert!(
            !local_path.components().any(|c| c.as_os_str() == "packaged-goals"),
            "local's source_path must NOT be inside packaged-goals/"
        );
    }

    // spec: goal-storage — `binary_relative_packaged_dir` resolves the
    // spec'd convention from any binary path. Pop the binary name, pop
    // the binary's parent dir, append `.tinker/goals/packaged-goals/`.
    #[test]
    fn test_spec_binary_relative_packaged_dir_resolves_convention() {
        let exe = PathBuf::from("/usr/bin/tinker");
        let dir = binary_relative_packaged_dir(&exe);
        assert_eq!(dir, PathBuf::from("/usr/.tinker/goals/packaged-goals"));
    }

    // spec: goal-storage — in dev mode the binary at
    // `<project>/target/debug/tinker` resolves to
    // `<project>/target/.tinker/goals/packaged-goals/` — a *different*
    // directory from the project's own `.tinker/goals/packaged-goals/`
    // (which `load_packaged_goals` walks). The two passes don't
    // overlap in dev mode; the binary-relative dir is populated by
    // the install script, not by `cargo build`.
    #[test]
    fn test_spec_binary_relative_packaged_dir_dev_mode_path() {
        let exe = PathBuf::from("/Users/me/proj/target/debug/tinker");
        let dir = binary_relative_packaged_dir(&exe);
        assert_eq!(dir, PathBuf::from("/Users/me/proj/target/.tinker/goals/packaged-goals"));
    }

    // spec: goal-storage — `binary_relative_packaged_dir` is tolerant of
    // short paths. A single-component path like `tinker` (no parent
    // dir) results in an empty path after one pop; the second pop
    // fails, the function returns the empty path. The caller checks
    // `fs.is_dir("")` which is always false, so the pass is silently
    // skipped. No panic, no bogus directory.
    #[test]
    fn test_spec_binary_relative_packaged_dir_short_path_is_safe() {
        let exe = PathBuf::from("tinker");
        let dir = binary_relative_packaged_dir(&exe);
        // After popping "tinker", the path is empty. The second pop
        // fails and the function returns the empty path.
        assert_eq!(dir, PathBuf::new());
        // Verify the function can be called and is_dir("") is false
        // (the pass would be silently skipped).
        // We don't construct a real Filesystem here — the empty path
        // is the testable property.
    }

    // spec: goal-storage — `load_binary_relative_packaged_goals` finds
    // every `.toml` file in the binary-relative packaged-goals directory
    // and populates `source_path` for the render-time `is_packaged` check.
    #[test]
    fn test_spec_load_binary_relative_packaged_goals_finds_convention_dir() {
        let fs = MockFs::new();
        let exe = PathBuf::from("/usr/bin/tinker");
        let dir = PathBuf::from("/usr/.tinker/goals/packaged-goals");
        fs.add_file(&dir.join("foo.toml"), &goal_toml("foo", "shipped default"));
        fs.add_file(&dir.join("bar.toml"), &goal_toml("bar", "shipped default"));

        let result = load_binary_relative_packaged_goals(&fs, &exe).unwrap();
        assert_eq!(result.errors.len(), 0);
        let ids: Vec<&str> = result.goals.iter().map(|g| g.id.as_str()).collect();
        assert!(ids.contains(&"foo"));
        assert!(ids.contains(&"bar"));
        // Every loaded goal has a `packaged-goals/` source_path so
        // `App::is_packaged` classifies it correctly.
        for g in &result.goals {
            let p = g.source_path.as_ref().expect("source_path populated");
            assert!(
                p.components().any(|c| c.as_os_str() == "packaged-goals"),
                "binary-relative packaged goal's source_path must be inside packaged-goals/; got {p:?}"
            );
        }
    }

    // spec: goal-storage — returns an empty result when the binary-relative
    // directory does not exist (e.g., a binary run with no install layout).
    #[test]
    fn test_spec_load_binary_relative_packaged_goals_returns_empty_when_dir_absent() {
        let fs = MockFs::new();
        let exe = PathBuf::from("/usr/bin/tinker");
        // /usr/.tinker/goals/packaged-goals doesn't exist
        let result = load_binary_relative_packaged_goals(&fs, &exe).unwrap();
        assert_eq!(result.errors.len(), 0);
        assert!(result.goals.is_empty());
    }

    // spec: goal-storage — a malformed goal TOML in the binary-relative
    // directory does not block loading of well-formed siblings. Mirrors
    // the security T1 invariant for the other loader passes.
    #[test]
    fn test_spec_load_binary_relative_packaged_goals_malformed_does_not_block_siblings() {
        let fs = MockFs::new();
        let exe = PathBuf::from("/usr/bin/tinker");
        let dir = PathBuf::from("/usr/.tinker/goals/packaged-goals");
        fs.add_file(&dir.join("ok.toml"), &goal_toml("ok", "fine"));
        fs.add_file(
            &dir.join("broken.toml"),
            "id = \"broken\"\ndescription = \"\"\"\nunterminated triple-quoted block\n",
        );

        let result = load_binary_relative_packaged_goals(&fs, &exe).unwrap();
        assert!(result.goals.iter().any(|g| g.id == "ok"));
        assert!(!result.goals.iter().any(|g| g.id == "broken"));
        assert!(
            result.errors.iter().any(|(p, _)| p.ends_with("broken.toml")),
            "malformed binary-relative packaged file must surface in errors"
        );
    }

    // spec: goal-storage — `load_all_goals_with_exe` runs the binary-relative
    // pass as the third (lowest-precedence) tier. A goal id present only
    // in the binary-relative path lands as a `binary-relative-packaged` goal.
    #[test]
    fn test_spec_load_all_goals_with_exe_includes_binary_relative_pass() {
        let fs = MockFs::new();
        let proj = PathBuf::from("/proj/.tinker");
        // No regular or tinker_dir-packaged copy; only the binary-relative copy.
        // We mock a binary at a custom path that resolves to a unique dir,
        // so the test doesn't depend on the project's actual `.tinker/goals/packaged-goals/`.
        let exe = PathBuf::from("/opt/tinker/bin/tinker");
        let br_dir = PathBuf::from("/opt/tinker/.tinker/goals/packaged-goals");
        fs.add_file(
            &br_dir.join("binary-only.toml"),
            &goal_toml("binary-only", "shipped"),
        );

        let dirs = vec![proj];
        let result = load_all_goals_with_exe(&fs, &dirs, Some(&exe)).unwrap();
        assert_eq!(result.goals.len(), 1);
        assert_eq!(result.goals[0].id, "binary-only");
        // source_path is inside packaged-goals/ so `is_packaged` classifies it.
        let p = result.goals[0].source_path.as_ref().unwrap();
        assert!(
            p.components().any(|c| c.as_os_str() == "packaged-goals"),
            "binary-only's source_path must be inside packaged-goals/"
        );
    }

    // spec: goal-storage — when `exe_path` is `None` the binary-relative
    // pass is silently skipped. The first two passes still run; the result
    // is the same as the no-binary-relative case.
    #[test]
    fn test_spec_load_all_goals_with_exe_none_skips_binary_relative_pass() {
        let fs = MockFs::new();
        let proj = PathBuf::from("/proj/.tinker");
        // The project has a regular goal but no packaged copy.
        fs.add_file(&proj.join("goals/local.toml"), &goal_toml("local", "p"));

        let dirs = vec![proj];
        let result = load_all_goals_with_exe(&fs, &dirs, None).unwrap();
        assert_eq!(result.goals.len(), 1);
        assert_eq!(result.goals[0].id, "local");
    }

    // spec: goal-storage — when a goal id is present in both the
    // tinker_dir-packaged pass (Pass 2) and the binary-relative pass
    // (Pass 3), the merge state produces a cross-tier collision with
    // Pass 2 as the winner (it runs first) and Pass 3 as the
    // duplicate contributor. Demonstrates that the two passes are
    // tier-distinct even when they happen to find the same id.
    //
    // Setup: a binary at `/install/bin/tinker` (resolves to
    // `/install/.tinker/goals/packaged-goals/`) and a project's own
    // `.tinker/goals/packaged-goals/` at a *different* physical
    // location (`/proj/.tinker/goals/packaged-goals/`). Both contain
    // the same goal id `dup` with different descriptions.
    #[test]
    fn test_spec_load_all_goals_with_exe_binary_relative_loses_to_tinker_dir_packaged() {
        let fs = MockFs::new();
        let proj = PathBuf::from("/proj/.tinker");
        // The tinker_dir-packaged dir (Pass 2 walks this).
        let proj_packaged = proj.join("goals/packaged-goals");
        // A binary at a separate install location, resolving to a
        // different physical dir than the project's packaged-goals.
        let exe = PathBuf::from("/install/bin/tinker");
        let br_packaged = PathBuf::from("/install/.tinker/goals/packaged-goals");

        fs.add_file(
            &proj_packaged.join("dup.toml"),
            &goal_toml("dup", "tinker_dir-packaged copy"),
        );
        fs.add_file(
            &br_packaged.join("dup.toml"),
            &goal_toml("dup", "binary-relative-packaged copy"),
        );

        let dirs = vec![proj];
        let result = load_all_goals_with_exe(&fs, &dirs, Some(&exe)).unwrap();
        // Winner: the tinker_dir-packaged copy (Pass 2 runs first).
        assert_eq!(result.goals.len(), 1);
        assert_eq!(result.goals[0].description.trim(), "tinker_dir-packaged copy");
        // Collision is reported with both contributing tiers, in
        // load-order (winner first, then the ignored duplicate).
        assert_eq!(result.collisions.len(), 1);
        let c = &result.collisions[0];
        assert_eq!(c.goal_id, "dup");
        assert_eq!(c.contributors.len(), 2);
        assert_eq!(c.contributors[0].0, PACKAGED_TIER,
            "winner is tinker_dir-packaged (Pass 2 runs before Pass 3)");
        assert_eq!(c.contributors[1].0, BINARY_RELATIVE_PACKAGED_TIER,
            "binary-relative-packaged is the duplicate contributor");
    }

    // spec: goal-storage — `BINARY_RELATIVE_PACKAGED_TIER` is the string
    // `"binary-relative-packaged"`. Pinned to catch accidental rename
    // regressions, since the collision diagnostic and event capture
    // downstream depend on this exact label.
    #[test]
    fn test_spec_binary_relative_packaged_tier_label_is_stable() {
        assert_eq!(BINARY_RELATIVE_PACKAGED_TIER, "binary-relative-packaged");
    }
}
