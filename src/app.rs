use crate::goal::{Goal, GoalCollision};
use crate::repl_buffer::ReplBuffer;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

/// An item in the goal list — either a permanent goal loaded from disk or an
/// ephemeral fresh sub-session spawned by a dispatcher goal.
#[derive(Clone, Debug)]
pub enum GoalListItem {
    Goal(Goal),
    /// A fresh sub-session: its full session ID (e.g. `"coding-standards~1"`)
    /// and an optional correlation label from the dispatcher (the part after `|`).
    Ephemeral(String, Option<String>),
}

impl GoalListItem {
    /// The display / session ID for this item.
    pub fn id(&self) -> &str {
        match self {
            Self::Goal(g) => &g.id,
            Self::Ephemeral(id, _) => id,
        }
    }

}

/// Strip all `~{counter}` suffixes from a fresh sub-session ID, returning the
/// root permanent goal ID. For ordinary session IDs (no `~`) the value is
/// returned unchanged.
/// Examples: `"fresh-agents~1"` → `"fresh-agents"`, `"jog~1~3"` → `"jog"`,
/// `"tend"` → `"tend"`.
/// Use this only where root normalisation is wanted (e.g. pane label, running
/// marker on the permanent goal row). For immediate-parent lookup use
/// `session_parent_id`.
pub fn session_base_id(id: &str) -> &str {
    id.split_once('~').map_or(id, |(base, _)| base)
}

/// Return the immediate parent ID of a fresh sub-session ID.
/// Strips only the trailing `~{counter}` segment, so `"jog~1~3"` → `"jog~1"`,
/// `"jog~1"` → `"jog"`, `"jog"` → `"jog"`.
pub fn session_parent_id(id: &str) -> &str {
    match id.rfind('~') {
        Some(pos) => &id[..pos],
        None => id,
    }
}

/// Per-goal token-usage statistics for the TUI's cost-driver indicators.
/// Tracks running totals across all API calls for a goal; the three
/// rendered metrics are derived lazily from these totals.
///
/// `session_count` is incremented on each completed dispatch (each
/// `SessionEvent::Done`). Input/output totals accumulate from
/// `SessionEvent::TokenUsage` events. The averages are computed at
/// render time so they always reflect the latest state.
#[derive(Debug, Clone, Default)]
pub struct GoalTokenStats {
    /// Number of completed dispatches for this goal.
    pub session_count: u64,
    /// Cumulative prompt (input) tokens across all API calls.
    pub total_prompt_tokens: u64,
    /// Cumulative completion (output) tokens across all API calls.
    pub total_completion_tokens: u64,
    /// Cumulative cached tokens across all API calls (where reported).
    pub total_cached_tokens: u64,
}

impl GoalTokenStats {
    /// Average prompt tokens per session. Returns 0 when no sessions have
    /// completed — the metric is hidden in the row when count is 0.
    pub fn avg_prompt_per_session(&self) -> u64 {
        self.total_prompt_tokens.checked_div(self.session_count).unwrap_or(0)
    }

    /// Average completion tokens per session.
    pub fn avg_completion_per_session(&self) -> u64 {
        self.total_completion_tokens.checked_div(self.session_count).unwrap_or(0)
    }

    /// True when any token data has been recorded for this goal.
    pub fn has_data(&self) -> bool {
        self.session_count > 0
    }
}

#[derive(Debug, Clone, PartialEq, Hash)]
pub enum Role {
    User(String),
    System,
    Agent(String),
}

#[derive(Debug, Clone)]
pub struct Message {
    pub role: Role,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Focus {
    Repl,
    Tree,
}

/// Which field has keyboard focus inside the options dialog.
#[derive(Debug, Clone, PartialEq)]
pub enum ModalField {
    Reason,
    Tier,
}

/// State for the options dialog opened via Enter in the goal tree. The dialog
/// collects a trigger reason and lets the user inspect / change the goal's
/// tier. The modal owns keyboard focus while `App.modal` is `Some`; submitting
/// fires the goal with the typed reason (and writes a changed tier to disk),
/// escape cancels. Closing the modal doesn't touch `App.focus`.
#[derive(Debug, Clone)]
pub struct ModalState {
    pub goal_id: String,
    /// Trigger-reason text (field 1).
    pub input: String,
    /// Tier value shown in the dialog: "mid" | "high" | "low".
    /// "mid" is used as the display label for the absent (None) case.
    pub tier: String,
    /// Tier when the dialog was opened — used to detect changes.
    pub initial_tier: String,
    /// Which field currently has the cursor.
    pub focused_field: ModalField,
}


/// Per-pane scroll state. `y = None` follows the tail (snaps to bottom).
/// `y = Some(n)` is an absolute line offset from the top of the wrapped
/// content. `last_total` and `last_height` are written by the renderer each
/// frame so the event handler can derive max-scroll and re-engage follow
/// when the user scrolls back to the bottom.
#[derive(Debug, Clone)]
pub struct ScrollState {
    pub y: Option<usize>,
    pub last_total: usize,
    pub last_height: u16,
    /// A cheap key representing the content + width that `last_total` was
    /// computed for. When this differs from the current key the cached total
    /// is stale and `line_count` must be re-run.
    pub cache_key: u64,
}

impl ScrollState {
    pub fn new() -> Self {
        Self { y: None, last_total: 0, last_height: 0, cache_key: 0 }
    }

    fn max_y(&self) -> usize {
        self.last_total.saturating_sub(self.last_height as usize)
    }

    pub fn scroll_up(&mut self, step: usize) {
        let current = self.y.unwrap_or(self.max_y());
        self.y = Some(current.saturating_sub(step));
    }

    pub fn scroll_down(&mut self, step: usize) {
        let max_y = self.max_y();
        match self.y {
            None => {}
            Some(y) => {
                let new_y = y.saturating_add(step);
                if new_y >= max_y {
                    self.y = None;
                } else {
                    self.y = Some(new_y);
                }
            }
        }
    }

    pub fn effective_y(&self) -> usize {
        match self.y {
            None => self.max_y(),
            Some(y) => y.min(self.max_y()),
        }
    }

    pub fn record_render(&mut self, total: usize, height: u16, cache_key: u64) {
        self.last_total = total;
        self.last_height = height;
        self.cache_key = cache_key;
    }

    pub fn is_cache_valid(&self, cache_key: u64) -> bool {
        self.cache_key == cache_key
    }

    /// Anchor at the top of the content. Use for panes that should default
    /// to "read from the beginning" (e.g. a goal description), as opposed to
    /// chat-style panes that follow the tail.
    pub fn reset_to_top(&mut self) {
        self.y = Some(0);
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Phase {
    /// Sending the init prompt to tend.
    Initializing,
    /// Ready; sessions may be running but there is no serial queue.
    Idle,
}

pub struct App {
    pub messages: Vec<Message>,
    pub input: String,
    /// Character-index cursor position within `input` (0 = before first char,
    /// `input.chars().count()` = after last char). Characters are inserted at
    /// this position; backspace deletes the char before it. Maintained by the
    /// `input_*` helper methods to stay in sync with `input`.
    pub input_cursor: usize,
    pub goals: Vec<Goal>,
    pub tinker_dirs: Vec<PathBuf>,
    pub parse_errors: Vec<(PathBuf, String)>,
    pub selected_goal: usize,
    /// Goal sessions currently running, mapped to the reason they were triggered.
    pub running_sessions: HashMap<String, Option<String>>,
    pub user_has_interacted: bool,
    pub phase: Phase,
    /// How many times we've asked tend to fix a parse error in this
    /// edit cycle. Reset on a fresh user message or a clean Done.
    pub correction_attempts: u8,
    pub focus: Focus,
    pub should_quit: bool,
    pub repl_scroll: ScrollState,
    /// Retained REPL line cache that survives across frames. Invalidated when
    /// the active session changes or the pane width changes.
    pub repl_buffer: ReplBuffer,
    pub goal_text_scroll: ScrollState,
    pub goal_list_scroll: ScrollState,
    pub modal: Option<ModalState>,
    /// Which session currently receives the user's REPL input (goal-id string).
    pub active_session: String,
    /// Tracks the index in `messages` of the current in-progress agent turn
    /// for each session, so incoming chunks can append to that slot in-place.
    pub agent_msg_idx: HashMap<String, usize>,
    /// Session IDs that belong to ephemeral fresh sub-sessions. These are
    /// removed from `session_senders` by the run loop once they complete.
    pub ephemeral_sessions: HashSet<String>,
    /// Ordered list of ephemeral session IDs for goal-list display. Maintained
    /// in insertion order so sub-sessions appear in the order they were spawned.
    /// Entries stay until `retire_completed_ephemeral_sessions` removes them.
    pub ephemeral_sessions_ordered: Vec<String>,
    /// Correlation labels for ephemeral sessions, keyed by session ID.
    /// Populated when the `spawn_session` tool dispatcher creates a new
    /// sub-session; cleaned up when the session is retired. The TUI
    /// goal-list renders this label instead of the raw session ID when
    /// present.
    pub ephemeral_labels: HashMap<String, Option<String>>,
    /// Monotone counter used to assign unique IDs to fresh sub-sessions.
    pub fresh_session_counter: u64,
    /// Last-seen set of cross-tier goal-id collisions from the loader. The
    /// watcher diffs the load result against this on each cycle to drive
    /// startup-diagnostic system messages — see `update_goal_id_collisions`.
    pub goal_id_collisions: Vec<GoalCollision>,
    /// Index into the WERKELN_VERBS list; advanced every ~2 s while sessions run.
    pub werkeln_verb_idx: usize,
    /// When the verb index was last advanced.
    pub werkeln_last_advance: Instant,
    /// Per-goal token-usage statistics for the TUI cost-driver indicators.
    /// Keyed by goal id (permanent goal id or ephemeral session id).
    pub token_usage: HashMap<String, GoalTokenStats>,
}

impl App {
    pub fn new() -> Self {
        Self {
            messages: vec![],
            input: String::new(),
            input_cursor: 0,
            goals: vec![],
            tinker_dirs: vec![],
            parse_errors: vec![],
            selected_goal: 0,
            running_sessions: HashMap::new(),
            user_has_interacted: false,
            phase: Phase::Initializing,
            correction_attempts: 0,
            focus: Focus::Repl,
            should_quit: false,
            repl_scroll: ScrollState::new(),
            repl_buffer: ReplBuffer::new(),
            goal_text_scroll: {
                let mut s = ScrollState::new();
                s.reset_to_top();
                s
            },
            goal_list_scroll: {
                let mut s = ScrollState::new();
                s.reset_to_top();
                s
            },
            modal: None,
            active_session: "tend".to_string(),
            agent_msg_idx: HashMap::new(),
            ephemeral_sessions: HashSet::new(),
            ephemeral_sessions_ordered: Vec::new(),
            ephemeral_labels: HashMap::new(),
            fresh_session_counter: 0,
            goal_id_collisions: Vec::new(),
            werkeln_verb_idx: 0,
            werkeln_last_advance: Instant::now(),
            token_usage: HashMap::new(),
        }
    }

    pub fn push_user_message(&mut self, text: &str, session_id: &str) {
        self.messages.push(Message { role: Role::User(session_id.to_string()), text: text.to_string() });
    }

    pub fn push_system_message(&mut self, text: &str) {
        self.messages.push(Message { role: Role::System, text: text.to_string() });
    }

    // ------------------------------------------------------------------
    // REPL input cursor editing
    // ------------------------------------------------------------------
    //
    // The REPL input is a single-line buffer with a movable cursor. All
    // mutations go through these methods so `input_cursor` stays a valid
    // char index into `input` (0..=chars().count()). The cursor is
    // char-based (not byte-based) so multi-byte characters work correctly.

    /// Clamps `input_cursor` into the valid range. Called at the start of
    /// every editing operation so direct field writes (e.g. in tests) don't
    /// produce an out-of-bounds index.
    fn clamp_cursor(&mut self) {
        let max = self.input.chars().count();
        if self.input_cursor > max {
            self.input_cursor = max;
        }
    }

    /// Insert a character at the cursor and advance the cursor past it.
    pub fn input_insert(&mut self, c: char) {
        self.clamp_cursor();
        let pos = self.input_cursor;
        let mut new_input = String::with_capacity(self.input.len() + c.len_utf8());
        new_input.extend(self.input.chars().take(pos));
        new_input.push(c);
        new_input.extend(self.input.chars().skip(pos));
        self.input = new_input;
        self.input_cursor = pos + 1;
    }

    /// Delete the character before the cursor (Backspace). Does nothing when
    /// the cursor is at position 0.
    pub fn input_backspace(&mut self) {
        self.clamp_cursor();
        if self.input_cursor == 0 {
            return;
        }
        let pos = self.input_cursor;
        let mut new_input = String::with_capacity(self.input.len());
        new_input.extend(self.input.chars().take(pos - 1));
        new_input.extend(self.input.chars().skip(pos));
        self.input = new_input;
        self.input_cursor = pos - 1;
    }

    /// Delete the character after the cursor (forward Delete). Does nothing
    /// when the cursor is at the end.
    pub fn input_delete(&mut self) {
        self.clamp_cursor();
        let pos = self.input_cursor;
        let mut new_input = String::with_capacity(self.input.len());
        new_input.extend(self.input.chars().take(pos));
        new_input.extend(self.input.chars().skip(pos + 1));
        self.input = new_input;
        // cursor position unchanged
    }

    /// Move the cursor one character to the left. Clamped at 0.
    pub fn input_cursor_left(&mut self) {
        self.clamp_cursor();
        self.input_cursor = self.input_cursor.saturating_sub(1);
    }

    /// Move the cursor one character to the right. Clamped at end.
    pub fn input_cursor_right(&mut self) {
        self.clamp_cursor();
        let max = self.input.chars().count();
        if self.input_cursor < max {
            self.input_cursor += 1;
        }
    }

    /// Move the cursor to the beginning of the input.
    pub fn input_cursor_home(&mut self) {
        self.input_cursor = 0;
    }

    /// Move the cursor to the end of the input.
    pub fn input_cursor_end(&mut self) {
        self.input_cursor = self.input.chars().count();
    }

    /// Move the cursor backward by one word. A word boundary is the
    /// transition from whitespace to non-whitespace; trailing whitespace is
    /// skipped first, then the contiguous run of non-whitespace characters.
    pub fn input_cursor_word_left(&mut self) {
        self.clamp_cursor();
        let chars: Vec<char> = self.input.chars().collect();
        let mut pos = self.input_cursor.min(chars.len());
        while pos > 0 && chars[pos - 1].is_whitespace() {
            pos -= 1;
        }
        while pos > 0 && !chars[pos - 1].is_whitespace() {
            pos -= 1;
        }
        self.input_cursor = pos;
    }

    /// Move the cursor forward by one word. Skips the current run of
    /// non-whitespace, then the following whitespace.
    pub fn input_cursor_word_right(&mut self) {
        self.clamp_cursor();
        let chars: Vec<char> = self.input.chars().collect();
        let mut pos = self.input_cursor.min(chars.len());
        while pos < chars.len() && !chars[pos].is_whitespace() {
            pos += 1;
        }
        while pos < chars.len() && chars[pos].is_whitespace() {
            pos += 1;
        }
        self.input_cursor = pos;
    }

    /// Delete from the cursor to the beginning of the line (Ctrl-U).
    pub fn input_kill_to_start(&mut self) {
        self.clamp_cursor();
        let pos = self.input_cursor;
        let rest: String = self.input.chars().skip(pos).collect();
        self.input = rest;
        self.input_cursor = 0;
    }

    /// Delete from the cursor to the end of the line (Ctrl-K).
    pub fn input_kill_to_end(&mut self) {
        self.clamp_cursor();
        let pos = self.input_cursor;
        let kept: String = self.input.chars().take(pos).collect();
        self.input = kept;
        // cursor stays (now at end)
    }

    /// Delete the word before the cursor (Ctrl-W). Skips trailing whitespace,
    /// then deletes the contiguous run of non-whitespace.
    pub fn input_delete_word(&mut self) {
        self.clamp_cursor();
        let chars: Vec<char> = self.input.chars().collect();
        let old_pos = self.input_cursor.min(chars.len());
        let mut new_pos = old_pos;
        while new_pos > 0 && chars[new_pos - 1].is_whitespace() {
            new_pos -= 1;
        }
        while new_pos > 0 && !chars[new_pos - 1].is_whitespace() {
            new_pos -= 1;
        }
        let mut new_input = String::with_capacity(self.input.len());
        new_input.extend(chars[..new_pos].iter());
        new_input.extend(chars[old_pos..].iter());
        self.input = new_input;
        self.input_cursor = new_pos;
    }

    /// Clear the input and reset the cursor to 0.
    pub fn clear_input(&mut self) {
        self.input.clear();
        self.input_cursor = 0;
    }

    /// Diffs `new` against the current parse_errors. Emits a system message
    /// for any newly-failing file (not already in `parse_errors`). Then stores
    /// `new` as the current set.
    pub fn update_parse_errors(&mut self, new: Vec<(PathBuf, String)>) {
        for (path, err) in &new {
            let already = self.parse_errors.iter().any(|(p, e)| p == path && e == err);
            if !already {
                let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("?");
                self.push_system_message(&format!("Failed to parse goal `{}`: {}", name, err));
            }
        }
        self.parse_errors = new;
    }

    /// Diffs `new` against the current `goal_id_collisions` and emits a
    /// system message for any collision not already reported. Each new
    /// collision produces a single message listing the goal id and every
    /// contributing tier and path. Returns the list of new collisions
    /// (cloned from `new`) so callers can drive parallel log-event
    /// emission — see the `GoalCollision` log event captured by
    /// `tend-introspection`. Then stores `new` as the current set.
    ///
    /// The diff is structural (full path set comparison), not id-only: if a
    /// user edits one of the contributing paths — renaming or removing the
    /// duplicate — the new state is detected as a fresh collision to report.
    /// This mirrors `update_parse_errors` and keeps the watcher in sync with
    /// disk state.
    ///
    /// System messages look like:
    ///
    ///   Goal id `shared` found at multiple tiers: \
    ///   [project-local] /proj/.tinker/goals/shared.toml, \
    ///   [ancestor global] /home/.tinker/goals/shared.toml
    pub fn update_goal_id_collisions(&mut self, new: Vec<GoalCollision>) -> Vec<GoalCollision> {
        let mut added = Vec::new();
        for collision in &new {
            let already = self.goal_id_collisions.iter().any(|existing| {
                existing.goal_id == collision.goal_id
                    && existing.contributors == collision.contributors
            });
            if !already {
                added.push(collision.clone());
                let contributors = collision
                    .contributors
                    .iter()
                    .map(|(tier, path)| format!("[{}] {}", tier, path.display()))
                    .collect::<Vec<_>>()
                    .join(", ");
                self.push_system_message(&format!(
                    "Goal id `{}` found at multiple tiers: {}",
                    collision.goal_id, contributors,
                ));
            }
        }
        self.goal_id_collisions = new;
        added
    }

    /// Append agent text to the REPL message list. The first chunk for a new
    /// turn creates a new `Role::Agent` message; subsequent chunks extend it
    /// in-place so the REPL doesn't accumulate thousands of tiny entries.
    pub fn append_agent_message(&mut self, goal_id: &str, text: &str) {
        if let Some(&idx) = self.agent_msg_idx.get(goal_id)
            && let Some(msg) = self.messages.get_mut(idx) {
                msg.text.push_str(text);
                return;
            }
        let idx = self.messages.len();
        self.messages.push(Message { role: Role::Agent(goal_id.to_string()), text: text.to_string() });
        self.agent_msg_idx.insert(goal_id.to_string(), idx);
    }

    /// Call when a session turn ends so the next turn opens a fresh message.
    pub fn finalize_agent_message(&mut self, goal_id: &str) {
        self.agent_msg_idx.remove(goal_id);
    }

    /// Retire completed ephemeral sessions: returns the session IDs whose
    /// channels should be dropped from `session_senders`. A session is
    /// considered complete when it has been removed from `running_sessions`.
    /// The returned IDs are also removed from `ephemeral_sessions` and
    /// `ephemeral_sessions_ordered`.
    pub fn retire_completed_ephemeral_sessions(&mut self) -> Vec<String> {
        let completed: Vec<String> = self.ephemeral_sessions.iter()
            .filter(|id| !self.running_sessions.contains_key(*id))
            .cloned()
            .collect();
        for id in &completed {
            self.ephemeral_sessions.remove(id);
            self.ephemeral_labels.remove(id);
        }
        self.ephemeral_sessions_ordered.retain(|id| self.ephemeral_sessions.contains(id));
        // Clamp selection in case ephemerals that were selected got retired.
        let n = self.flat_items().len();
        if n == 0 {
            self.selected_goal = 0;
        } else if self.selected_goal >= n {
            self.selected_goal = n - 1;
        }
        completed
    }

    /// The combined flat list of permanent goals and ephemeral sub-sessions,
    /// in depth-first tree order with each ephemeral nested immediately after
    /// its immediate parent (which may itself be an ephemeral at any depth).
    ///
    /// Example ordering for goals=[A, B] and ephemerals=[A~1, A~1~2, A~2]:
    ///   A, A~1, A~1~2, A~2, B
    pub fn flat_items(&self) -> Vec<GoalListItem> {
        let goals = self.flat_goals();
        let mut items: Vec<GoalListItem> = goals.into_iter().map(GoalListItem::Goal).collect();

        // Walk items forward; for each item insert its immediate ephemeral
        // children right after it. Newly inserted ephemerals are visited in
        // subsequent iterations so their own children are inserted too.
        let mut i = 0;
        while i < items.len() {
            let parent_id = items[i].id().to_string();
            let children: Vec<String> = self.ephemeral_sessions_ordered.iter()
                .filter(|eph_id| session_parent_id(eph_id.as_str()) == parent_id.as_str())
                .cloned()
                .collect();
            for (j, child_id) in children.into_iter().enumerate() {
                let label = self.ephemeral_labels.get(&child_id).cloned().flatten();
                items.insert(i + 1 + j, GoalListItem::Ephemeral(child_id, label));
            }
            i += 1;
        }

        items
    }

    /// The session ID of the currently-selected item (goal or ephemeral).
    pub fn selected_item_id(&self) -> Option<String> {
        self.flat_items().into_iter().nth(self.selected_goal).map(|i| i.id().to_string())
    }

    /// The currently-selected permanent goal, if the selection points at one.
    /// Returns `None` when an ephemeral sub-session is selected.
    pub fn selected_goal(&self) -> Option<Goal> {
        match self.flat_items().into_iter().nth(self.selected_goal)? {
            GoalListItem::Goal(g) => Some(g),
            GoalListItem::Ephemeral(_, _) => None,
        }
    }

    pub fn flat_goals(&self) -> Vec<Goal> {
        use crate::goal::{build_tree, GoalNode};
        fn flatten(nodes: &[GoalNode], out: &mut Vec<Goal>) {
            for node in nodes {
                out.push(node.goal.clone());
                flatten(&node.children, out);
            }
        }
        let tree = build_tree(&self.goals);
        let mut out = vec![];
        flatten(&tree, &mut out);
        out
    }

    /// True when the goal's `source_path` sits inside a `packaged-goals/`
    /// subdirectory — i.e., it is a packaged goal shipped with tinker, not a
    /// goal the user authored for this project. Classification follows the
    /// on-disk `packaged-goals/` convention, not the `.tinker` ancestor walk:
    /// a goal loaded from `~/.tinker/goals/x.toml` is project-local (it's
    /// not in a `packaged-goals/` subdir), while a goal loaded from
    /// `~/.tinker/goals/packaged-goals/x.toml` is packaged. The ancestor
    /// walk is a *discovery* mechanism that finds more goal files to load;
    /// the `packaged-goals/` directory is the *classification* signal. They
    /// are orthogonal. Goals with no `source_path` (the test-only path)
    /// default to project-local.
    ///
    /// This consumes the on-disk distinction that `goal-placement` writes
    /// and produces the render-time predicate the de-emphasis rule needs.
    pub fn is_packaged(&self, g: &Goal) -> bool {
        g.source_path.as_ref()
            .map(|p| p.components().any(|c| c.as_os_str() == "packaged-goals"))
            .unwrap_or(false)
    }

    pub fn select_next_goal(&mut self) {
        let n = self.flat_items().len();
        if n > 0 {
            self.selected_goal = (self.selected_goal + 1).min(n - 1);
            self.goal_text_scroll.reset_to_top();
            self.ensure_goal_visible();
        }
    }

    pub fn select_prev_goal(&mut self) {
        if self.selected_goal > 0 {
            self.selected_goal -= 1;
            self.goal_text_scroll.reset_to_top();
            self.ensure_goal_visible();
        }
    }

    fn ensure_goal_visible(&mut self) {
        let visible = self.goal_list_scroll.last_height.max(1) as usize;
        let offset = self.goal_list_scroll.effective_y();
        if self.selected_goal >= offset + visible {
            self.goal_list_scroll.y = Some(self.selected_goal.saturating_sub(visible).saturating_add(1));
        } else if self.selected_goal < offset {
            self.goal_list_scroll.y = Some(self.selected_goal);
        }
    }

    /// Aggregated token-usage statistics for a permanent goal, summing the
    /// goal's own API calls with all its ephemeral sub-sessions' calls. A
    /// parent goal that delegates all work to sub-sessions shows the full
    /// cost of delegation, not zero — the cost of delegated work shows on
    /// the parent.
    ///
    /// Aggregation matches ephemeral sessions by `session_base_id`: any key
    /// in `token_usage` whose root permanent goal ID matches `goal_id`
    /// contributes. This includes the permanent goal's own entry (if any)
    /// and all ephemeral descendants at any nesting depth
    /// (`tend`, `tend~1`, `tend~1~3`, …). Retired sub-sessions whose token
    /// stats persist in the map after their session entry is retired are
    /// included — the cost persists on the parent.
    ///
    /// The averages (`avg_prompt_per_session`, `avg_completion_per_session`)
    /// are computed from the aggregated totals divided by the aggregated
    /// session count, so the per-session average includes the delegated
    /// work's sessions in the denominator.
    pub fn aggregated_token_stats(&self, goal_id: &str) -> GoalTokenStats {
        let mut agg = GoalTokenStats::default();
        for (key, stats) in &self.token_usage {
            if session_base_id(key) == goal_id {
                agg.session_count += stats.session_count;
                agg.total_prompt_tokens += stats.total_prompt_tokens;
                agg.total_completion_tokens += stats.total_completion_tokens;
                agg.total_cached_tokens += stats.total_cached_tokens;
            }
        }
        agg
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    // --- session_base_id ---

    #[test]
    fn test_spec_session_base_id_plain_id_is_unchanged() {
        assert_eq!(session_base_id("tend"), "tend");
    }

    #[test]
    fn test_spec_session_base_id_single_tilde_returns_root() {
        assert_eq!(session_base_id("fresh-agents~1"), "fresh-agents");
    }

    #[test]
    fn test_spec_session_base_id_multiple_tildes_returns_root() {
        assert_eq!(session_base_id("jog~1~3"), "jog");
    }

    // --- session_parent_id ---

    #[test]
    fn test_spec_session_parent_id_plain_id_is_unchanged() {
        assert_eq!(session_parent_id("tend"), "tend");
    }

    #[test]
    fn test_spec_session_parent_id_single_tilde_returns_root() {
        assert_eq!(session_parent_id("jog~1"), "jog");
    }

    #[test]
    fn test_spec_session_parent_id_double_tilde_returns_intermediate() {
        assert_eq!(session_parent_id("jog~1~3"), "jog~1");
    }

    #[test]
    fn test_spec_session_parent_id_triple_tilde_returns_immediate_parent() {
        assert_eq!(session_parent_id("a~1~2~5"), "a~1~2");
    }

    // --- flat_items nesting ---

    fn make_app_with_ephemerals(ephemerals: Vec<&str>) -> App {
        let mut app = App::new();
        for e in ephemerals {
            app.ephemeral_sessions.insert(e.to_string());
            app.ephemeral_sessions_ordered.push(e.to_string());
        }
        app
    }

    fn item_ids(items: &[GoalListItem]) -> Vec<&str> {
        items.iter().map(|i| i.id()).collect()
    }

    // --- is_packaged ---

    fn make_goal_with_path(id: &str, path: Option<PathBuf>) -> crate::goal::Goal {
        crate::goal::Goal {
            id: id.to_string(),
            summary: String::new(),
            description: String::new(),
            source_path: path,
            kind: None,
            tier: None,
            parent_id: String::new(),
            children: vec![],
            related: vec![],
        }
    }

    // spec: packaged-goals-style — a goal whose `source_path` sits inside a
    // `packaged-goals/` subdirectory is packaged. Classification follows the
    // directory convention regardless of which `.tinker` directory the
    // subdirectory lives under.
    #[test]
    fn test_spec_is_packaged_source_in_project_local_packaged_goals_dir_returns_true() {
        let mut app = App::new();
        app.tinker_dirs = vec![PathBuf::from("/proj/.tinker")];
        let g = make_goal_with_path(
            "local-packaged",
            Some(PathBuf::from("/proj/.tinker/goals/packaged-goals/x.toml")),
        );
        assert!(app.is_packaged(&g),
            "goal under project-local .tinker/goals/packaged-goals/ must be packaged");
    }

    // spec: packaged-goals-style — a goal loaded from an ancestor `.tinker/goals/`
    // is project-local under the new rule, NOT packaged. The ancestor walk is a
    // discovery mechanism, not a classification signal; the `packaged-goals/`
    // subdirectory is the only classification signal.
    #[test]
    fn test_spec_is_packaged_source_in_ancestor_tinker_dir_outside_packaged_goals_returns_false() {
        let mut app = App::new();
        app.tinker_dirs = vec![
            PathBuf::from("/proj/.tinker"),
            PathBuf::from("/home/.tinker"),
        ];
        let g = make_goal_with_path(
            "ancestor-shared",
            Some(PathBuf::from("/home/.tinker/goals/shared.toml")),
        );
        assert!(!app.is_packaged(&g),
            "ancestor-loaded goal outside packaged-goals/ must not be packaged");
    }

    // spec: packaged-goals-style — a goal loaded from an ancestor
    // `.tinker/goals/packaged-goals/` is packaged. Covers the
    // "shipped with tinker via the home install symlink" case where
    // the discovery ancestor walk finds a packaged goal in `~/.tinker/`.
    #[test]
    fn test_spec_is_packaged_source_in_ancestor_packaged_goals_dir_returns_true() {
        let mut app = App::new();
        app.tinker_dirs = vec![
            PathBuf::from("/proj/.tinker"),
            PathBuf::from("/home/.tinker"),
        ];
        let g = make_goal_with_path(
            "ancestor-packaged",
            Some(PathBuf::from("/home/.tinker/goals/packaged-goals/x.toml")),
        );
        assert!(app.is_packaged(&g),
            "ancestor-loaded goal under packaged-goals/ must be packaged");
    }

    // spec: packaged-goals-style — a goal under the primary `.tinker/goals/`
    // (project-local) is not packaged. The directory must contain a
    // `packaged-goals/` subdirectory for the goal to classify as packaged.
    #[test]
    fn test_spec_is_packaged_source_in_project_local_goals_dir_returns_false() {
        let mut app = App::new();
        app.tinker_dirs = vec![PathBuf::from("/proj/.tinker")];
        let g = make_goal_with_path(
            "local",
            Some(PathBuf::from("/proj/.tinker/goals/local.toml")),
        );
        assert!(!app.is_packaged(&g),
            "goal under project-local .tinker/goals/ must not be packaged");
    }

    // spec: packaged-goals-style — a goal with no `source_path` (test-only
    // construction) must default to project-local, not packaged.
    #[test]
    fn test_spec_is_packaged_no_source_path_returns_false() {
        let mut app = App::new();
        app.tinker_dirs = vec![PathBuf::from("/proj/.tinker")];
        let g = make_goal_with_path("x", None);
        assert!(!app.is_packaged(&g),
            "goal without source_path must not be packaged");
    }

    // spec: packaged-goals-style — the `packaged-goals/` match is exact.
    // A directory whose name *contains* "packaged-goals" (e.g. "my-packaged-goals"
    // or "packaged-goals-backup") must not trigger the packaged classification.
    // Guards against accidental substring matching.
    #[test]
    fn test_spec_is_packaged_substring_directory_names_do_not_match() {
        let mut app = App::new();
        app.tinker_dirs = vec![PathBuf::from("/proj/.tinker")];
        let g = make_goal_with_path(
            "x",
            Some(PathBuf::from("/proj/.tinker/goals/my-packaged-goals/x.toml")),
        );
        assert!(!app.is_packaged(&g),
            "a directory whose name contains but does not equal 'packaged-goals' must not classify the goal as packaged");
    }

    #[test]
    fn test_spec_flat_items_depth1_ephemeral_appears_after_root() {
        // jog~1 should appear after jog (no permanent goals in this minimal app,
        // so we only check that the ordering respects parent-before-child).
        // With no permanent goals, flat_goals() is empty; the ephemeral hangs
        // with no permanent parent and so is not inserted by the algorithm — this
        // exercises the degenerate case without panicking.
        let app = make_app_with_ephemerals(vec!["jog~1"]);
        let items = app.flat_items();
        // No permanent goals → no ephemerals surfaced (parent not in list).
        assert_eq!(items.len(), 0);
    }

    fn make_goal(id: &str) -> crate::goal::Goal {
        crate::goal::Goal {
            id: id.to_string(),
            summary: String::new(),
            description: String::new(),
            source_path: None,
            kind: None,
            tier: None,
            parent_id: String::new(),
            children: vec![],
            related: vec![],
        }
    }

    #[test]
    fn test_spec_flat_items_nested_ephemerals_depth_first_order() {
        // Simulate: permanent goal "jog", ephemerals jog~1, jog~1~2, jog~2.
        // Expected order: jog (Goal), jog~1, jog~1~2, jog~2.
        let mut app = App::new();
        app.goals = vec![make_goal("jog")];
        for e in ["jog~1", "jog~1~2", "jog~2"] {
            app.ephemeral_sessions.insert(e.to_string());
            app.ephemeral_sessions_ordered.push(e.to_string());
        }
        let items = app.flat_items();
        assert_eq!(item_ids(&items), vec!["jog", "jog~1", "jog~1~2", "jog~2"]);
    }

    #[test]
    fn test_spec_flat_items_three_level_nesting() {
        // jog~1~2~5 should appear after jog~1~2.
        let mut app = App::new();
        app.goals = vec![make_goal("jog")];
        for e in ["jog~1", "jog~1~2", "jog~1~2~5"] {
            app.ephemeral_sessions.insert(e.to_string());
            app.ephemeral_sessions_ordered.push(e.to_string());
        }
        let items = app.flat_items();
        assert_eq!(item_ids(&items), vec!["jog", "jog~1", "jog~1~2", "jog~1~2~5"]);
    }

    // --- update_goal_id_collisions ---

    use crate::goal::GoalCollision;

    fn collision(
        id: &str,
        contributors: Vec<(&str, &str)>,
    ) -> GoalCollision {
        GoalCollision {
            goal_id: id.to_string(),
            contributors: contributors
                .into_iter()
                .map(|(tier, path)| (tier.to_string(), PathBuf::from(path)))
                .collect(),
        }
    }

    fn system_messages(app: &App) -> Vec<String> {
        app.messages
            .iter()
            .filter(|m| matches!(m.role, Role::System))
            .map(|m| m.text.clone())
            .collect()
    }

    // spec: goal-agents startup diagnostic — on the first call, every collision
    // in `new` produces a system message; `goal_id_collisions` is set to `new`.
    #[test]
    fn test_spec_update_goal_id_collisions_first_call_emits_each_collision() {
        let mut app = App::new();
        let new = vec![
            collision(
                "shared",
                vec![
                    ("project-local", "/proj/.tinker/goals/shared.toml"),
                    ("ancestor global", "/home/.tinker/goals/shared.toml"),
                ],
            ),
            collision(
                "tooling",
                vec![
                    ("project-local", "/proj/.tinker/goals/tooling.toml"),
                    ("ancestor global", "/home/.tinker/goals/tooling.toml"),
                ],
            ),
        ];
        app.update_goal_id_collisions(new.clone());

        let msgs = system_messages(&app);
        assert_eq!(msgs.len(), 2, "one system message per collision");
        assert!(
            msgs[0].contains("`shared`"),
            "first message must name the shared collision: {}",
            msgs[0]
        );
        assert!(
            msgs[0].contains("[project-local]"),
            "first message must list the project-local tier: {}",
            msgs[0]
        );
        assert!(
            msgs[0].contains("[ancestor global]"),
            "first message must list the ancestor tier: {}",
            msgs[0]
        );
        assert!(
            msgs[1].contains("`tooling`"),
            "second message must name the tooling collision: {}",
            msgs[1]
        );
        assert_eq!(app.goal_id_collisions, new);
    }

    // spec: goal-agents startup diagnostic — when the watcher re-runs with the
    // same set of collisions, no new system messages are emitted. Avoids
    // re-spamming the user on every 2-second reload cycle.
    #[test]
    fn test_spec_update_goal_id_collisions_idempotent_when_unchanged() {
        let mut app = App::new();
        let new = vec![collision(
            "shared",
            vec![
                ("project-local", "/proj/.tinker/goals/shared.toml"),
                ("ancestor global", "/home/.tinker/goals/shared.toml"),
            ],
        )];
        app.update_goal_id_collisions(new.clone());
        let after_first = system_messages(&app).len();
        assert_eq!(after_first, 1);

        // Same set — no new messages.
        app.update_goal_id_collisions(new.clone());
        assert_eq!(
            system_messages(&app).len(),
            after_first,
            "identical collision set must not emit new system messages"
        );
    }

    // spec: goal-agents startup diagnostic — when a contributing path changes
    // (e.g. the user renames the ancestor file), the new state is detected as
    // a fresh collision to report. The diff is structural, not id-only.
    #[test]
    fn test_spec_update_goal_id_collisions_detects_changed_path() {
        let mut app = App::new();
        app.update_goal_id_collisions(vec![collision(
            "shared",
            vec![
                ("project-local", "/proj/.tinker/goals/shared.toml"),
                ("ancestor global", "/home/.tinker/goals/shared-old.toml"),
            ],
        )]);
        assert_eq!(system_messages(&app).len(), 1);

        // Path changed — emit a new system message for the new state.
        app.update_goal_id_collisions(vec![collision(
            "shared",
            vec![
                ("project-local", "/proj/.tinker/goals/shared.toml"),
                ("ancestor global", "/home/.tinker/goals/shared-new.toml"),
            ],
        )]);
        let msgs = system_messages(&app);
        assert_eq!(msgs.len(), 2, "path change must emit a new message");
        assert!(
            msgs[1].contains("shared-new.toml"),
            "new message must reference the new path: {}",
            msgs[1]
        );
    }

    // spec: goal-agents startup diagnostic — when a collision clears (the
    // duplicate file is removed), the stored set updates to empty and no
    // spurious message is emitted.
    #[test]
    fn test_spec_update_goal_id_collisions_clears_when_resolved() {
        let mut app = App::new();
        app.update_goal_id_collisions(vec![collision(
            "shared",
            vec![
                ("project-local", "/proj/.tinker/goals/shared.toml"),
                ("ancestor global", "/home/.tinker/goals/shared.toml"),
            ],
        )]);
        assert_eq!(system_messages(&app).len(), 1);

        // Duplicate removed — empty collision set updates state without
        // emitting a new message.
        app.update_goal_id_collisions(vec![]);
        assert!(
            app.goal_id_collisions.is_empty(),
            "stored collisions must clear when loader reports none"
        );
        assert_eq!(
            system_messages(&app).len(),
            1,
            "clearing collisions must not emit a system message"
        );
    }

    // spec: goal-agents startup diagnostic — an empty input vec must not emit
    // any system message, even on the first call. Common case for the typical
    // single-project install.
    #[test]
    fn test_spec_update_goal_id_collisions_empty_first_call_is_silent() {
        let mut app = App::new();
        app.update_goal_id_collisions(vec![]);
        assert!(system_messages(&app).is_empty());
        assert!(app.goal_id_collisions.is_empty());
    }

    // --- GoalTokenStats ---

    // spec: token-usage metrics — session count flags overtriggering, input per
    // session flags prompt size, output per session flags verbosity. The three
    // metrics isolate one cost driver each. GoalTokenStats tracks running totals
    // and computes the averages lazily.

    #[test]
    fn test_spec_goal_token_stats_avg_per_session_divides_totals_by_count() {
        let stats = GoalTokenStats {
            session_count: 3,
            total_prompt_tokens: 15_000,
            total_completion_tokens: 3_000,
            total_cached_tokens: 0,
        };
        assert_eq!(stats.avg_prompt_per_session(), 5_000);
        assert_eq!(stats.avg_completion_per_session(), 1_000);
    }

    #[test]
    fn test_spec_goal_token_stats_avg_returns_zero_when_no_sessions() {
        let stats = GoalTokenStats::default();
        assert_eq!(stats.avg_prompt_per_session(), 0);
        assert_eq!(stats.avg_completion_per_session(), 0);
        assert!(!stats.has_data());
    }

    #[test]
    fn test_spec_goal_token_stats_has_data_true_after_session_count_increment() {
        let stats = GoalTokenStats {
            session_count: 1,
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            total_cached_tokens: 0,
        };
        assert!(stats.has_data());
    }

    // spec: token-usage metrics — the TUI's App tracks per-goal token stats
    // in a HashMap keyed by goal id. The three cost-driver indicators render
    // from these totals.

    #[test]
    fn test_spec_app_token_usage_starts_empty() {
        let app = App::new();
        assert!(app.token_usage.is_empty());
    }

    #[test]
    fn test_spec_app_token_usage_accumulates_across_multiple_events() {
        let mut app = App::new();
        // Simulate two API calls in the first dispatch.
        let stats = app.token_usage.entry("tend".into()).or_default();
        stats.total_prompt_tokens += 5_000;
        stats.total_completion_tokens += 1_000;
        stats.total_prompt_tokens += 3_000;
        stats.total_completion_tokens += 500;
        stats.session_count += 1;

        let stats = app.token_usage.get("tend").unwrap();
        assert_eq!(stats.session_count, 1);
        assert_eq!(stats.total_prompt_tokens, 8_000);
        assert_eq!(stats.total_completion_tokens, 1_500);
        assert_eq!(stats.avg_prompt_per_session(), 8_000);
        assert_eq!(stats.avg_completion_per_session(), 1_500);
    }

    #[test]
    fn test_spec_app_token_usage_cached_tokens_accumulate() {
        let mut app = App::new();
        let stats = app.token_usage.entry("tend".into()).or_default();
        stats.total_prompt_tokens += 10_000;
        stats.total_cached_tokens += 3_000;
        stats.session_count += 1;

        let stats = app.token_usage.get("tend").unwrap();
        assert_eq!(stats.total_cached_tokens, 3_000);
    }

    // ── aggregated_token_stats ─────────────────────────────────────────────

    // spec (tui — token-usage metrics): the three metrics shown for a
    // permanent goal aggregate the goal's own API calls with all its
    // ephemeral sub-sessions' calls, so the cost of delegated work shows
    // on the parent. A parent that delegates everything to sub-sessions
    // must show non-zero cost.

    #[test]
    fn test_spec_aggregated_token_stats_includes_parent_own_calls() {
        let mut app = App::new();
        app.token_usage.insert("tend".into(), GoalTokenStats {
            session_count: 2,
            total_prompt_tokens: 10_000,
            total_completion_tokens: 2_000,
            total_cached_tokens: 0,
        });
        let agg = app.aggregated_token_stats("tend");
        assert_eq!(agg.session_count, 2);
        assert_eq!(agg.total_prompt_tokens, 10_000);
        assert_eq!(agg.total_completion_tokens, 2_000);
        assert!(agg.has_data());
    }

    // spec (tui — token-usage metrics): when the parent has no own calls
    // but sub-sessions have produced tokens, the aggregated stats must
    // reflect the sub-sessions' cost.
    #[test]
    fn test_spec_aggregated_token_stats_rolls_up_ephemeral_children() {
        let mut app = App::new();
        // No parent entry — all work delegated.
        app.token_usage.insert("tend~1".into(), GoalTokenStats {
            session_count: 1,
            total_prompt_tokens: 5_000,
            total_completion_tokens: 800,
            total_cached_tokens: 0,
        });
        app.token_usage.insert("tend~2".into(), GoalTokenStats {
            session_count: 1,
            total_prompt_tokens: 3_000,
            total_completion_tokens: 400,
            total_cached_tokens: 0,
        });
        let agg = app.aggregated_token_stats("tend");
        assert_eq!(agg.session_count, 2);
        assert_eq!(agg.total_prompt_tokens, 8_000);
        assert_eq!(agg.total_completion_tokens, 1_200);
        assert_eq!(agg.avg_prompt_per_session(), 4_000);
        assert_eq!(agg.avg_completion_per_session(), 600);
    }

    // spec (tui — token-usage metrics): deeply nested sub-sessions
    // (tend~1~3) roll up to the same root as direct children (tend~1).
    #[test]
    fn test_spec_aggregated_token_stats_rolls_up_deeply_nested() {
        let mut app = App::new();
        app.token_usage.insert("tend~1".into(), GoalTokenStats {
            session_count: 1,
            total_prompt_tokens: 2_000,
            total_completion_tokens: 300,
            total_cached_tokens: 0,
        });
        app.token_usage.insert("tend~1~3".into(), GoalTokenStats {
            session_count: 1,
            total_prompt_tokens: 1_000,
            total_completion_tokens: 100,
            total_cached_tokens: 0,
        });
        app.token_usage.insert("tend~2".into(), GoalTokenStats {
            session_count: 1,
            total_prompt_tokens: 4_000,
            total_completion_tokens: 500,
            total_cached_tokens: 0,
        });
        let agg = app.aggregated_token_stats("tend");
        assert_eq!(agg.session_count, 3);
        assert_eq!(agg.total_prompt_tokens, 7_000);
        assert_eq!(agg.total_completion_tokens, 900);
    }

    // spec (tui — token-usage metrics): aggregated stats include the
    // parent's own calls PLUS its children's — the two are summed, not
    // one replacing the other.
    #[test]
    fn test_spec_aggregated_token_stats_sums_parent_and_children() {
        let mut app = App::new();
        app.token_usage.insert("tend".into(), GoalTokenStats {
            session_count: 1,
            total_prompt_tokens: 6_000,
            total_completion_tokens: 1_000,
            total_cached_tokens: 200,
        });
        app.token_usage.insert("tend~1".into(), GoalTokenStats {
            session_count: 1,
            total_prompt_tokens: 4_000,
            total_completion_tokens: 500,
            total_cached_tokens: 100,
        });
        let agg = app.aggregated_token_stats("tend");
        assert_eq!(agg.session_count, 2);
        assert_eq!(agg.total_prompt_tokens, 10_000);
        assert_eq!(agg.total_completion_tokens, 1_500);
        assert_eq!(agg.total_cached_tokens, 300);
    }

    // spec (tui — token-usage metrics): goals that happen to share a name
    // prefix but are different goals (e.g. "tend" vs "tendril") must not
    // contaminate each other's aggregates. session_base_id splits on the
    // first '~', so "tendril~1" → "tendril", not "tend".
    #[test]
    fn test_spec_aggregated_token_stats_does_not_match_prefix_neighbors() {
        let mut app = App::new();
        app.token_usage.insert("tend".into(), GoalTokenStats {
            session_count: 1,
            total_prompt_tokens: 1_000,
            total_completion_tokens: 100,
            total_cached_tokens: 0,
        });
        app.token_usage.insert("tendril~1".into(), GoalTokenStats {
            session_count: 1,
            total_prompt_tokens: 99_999,
            total_completion_tokens: 99_999,
            total_cached_tokens: 0,
        });
        let agg = app.aggregated_token_stats("tend");
        assert_eq!(agg.session_count, 1);
        assert_eq!(agg.total_prompt_tokens, 1_000);
        assert_eq!(agg.total_completion_tokens, 100);
    }

    // spec (tui — token-usage metrics): a goal with no token data at all
    // (neither own nor children) returns empty stats with has_data() == false.
    #[test]
    fn test_spec_aggregated_token_stats_empty_when_no_data() {
        let app = App::new();
        let agg = app.aggregated_token_stats("nobody");
        assert!(!agg.has_data());
        assert_eq!(agg.session_count, 0);
    }

    // ==================================================================
    // REPL input cursor editing
    // ==================================================================

    // --- input_insert ---

    /// Spec (tui — input cursor): inserting characters at the cursor builds
    /// the expected string when typing left-to-right (cursor at end).
    #[test]
    fn test_spec_input_insert_appends_at_end_when_cursor_at_end() {
        let mut app = App::new();
        app.input_insert('h');
        app.input_insert('i');
        assert_eq!(app.input, "hi");
        assert_eq!(app.input_cursor, 2);
    }

    /// Spec (tui — input cursor): inserting a character in the middle of the
    /// text places it at the cursor position and advances the cursor.
    #[test]
    fn test_spec_input_insert_mid_string() {
        let mut app = App::new();
        app.input = "helo".into();
        app.input_cursor = 3; // after "hel"
        app.input_insert('l');
        assert_eq!(app.input, "hello");
        assert_eq!(app.input_cursor, 4);
    }

    // --- input_backspace ---

    /// Spec (tui — input cursor): backspace at the end removes the last char.
    #[test]
    fn test_spec_input_backspace_at_end() {
        let mut app = App::new();
        app.input = "hello".into();
        app.input_cursor = 5;
        app.input_backspace();
        assert_eq!(app.input, "hell");
        assert_eq!(app.input_cursor, 4);
    }

    /// Spec (tui — input cursor): backspace in the middle removes the char
    /// before the cursor and decrements the cursor.
    #[test]
    fn test_spec_input_backspace_mid_string() {
        let mut app = App::new();
        app.input = "hello".into();
        app.input_cursor = 3; // after "hel"
        app.input_backspace();
        assert_eq!(app.input, "helo");
        assert_eq!(app.input_cursor, 2);
    }

    /// Spec (tui — input cursor): backspace at position 0 is a no-op.
    #[test]
    fn test_spec_input_backspace_at_start_is_noop() {
        let mut app = App::new();
        app.input = "hello".into();
        app.input_cursor = 0;
        app.input_backspace();
        assert_eq!(app.input, "hello");
        assert_eq!(app.input_cursor, 0);
    }

    // --- input_delete (forward delete) ---

    /// Spec (tui — input cursor): forward delete removes the char after the
    /// cursor without moving the cursor.
    #[test]
    fn test_spec_input_delete_mid_string() {
        let mut app = App::new();
        app.input = "hello".into();
        app.input_cursor = 1; // after "h"
        app.input_delete();
        assert_eq!(app.input, "hllo");
        assert_eq!(app.input_cursor, 1);
    }

    /// Spec (tui — input cursor): forward delete at end is a no-op.
    #[test]
    fn test_spec_input_delete_at_end_is_noop() {
        let mut app = App::new();
        app.input = "hello".into();
        app.input_cursor = 5;
        app.input_delete();
        assert_eq!(app.input, "hello");
        assert_eq!(app.input_cursor, 5);
    }

    // --- cursor movement ---

    /// Spec (tui — input cursor): left arrow decrements the cursor, clamped at 0.
    #[test]
    fn test_spec_input_cursor_left() {
        let mut app = App::new();
        app.input = "abc".into();
        app.input_cursor = 3;
        app.input_cursor_left();
        assert_eq!(app.input_cursor, 2);
        app.input_cursor_left();
        assert_eq!(app.input_cursor, 1);
        app.input_cursor_left();
        assert_eq!(app.input_cursor, 0);
        app.input_cursor_left(); // clamp
        assert_eq!(app.input_cursor, 0);
    }

    /// Spec (tui — input cursor): right arrow increments the cursor, clamped
    /// at the string length.
    #[test]
    fn test_spec_input_cursor_right() {
        let mut app = App::new();
        app.input = "abc".into();
        app.input_cursor = 0;
        app.input_cursor_right();
        assert_eq!(app.input_cursor, 1);
        app.input_cursor_right();
        assert_eq!(app.input_cursor, 2);
        app.input_cursor_right();
        assert_eq!(app.input_cursor, 3);
        app.input_cursor_right(); // clamp
        assert_eq!(app.input_cursor, 3);
    }

    /// Spec (tui — input cursor): Home moves cursor to 0.
    #[test]
    fn test_spec_input_cursor_home() {
        let mut app = App::new();
        app.input = "abc".into();
        app.input_cursor = 2;
        app.input_cursor_home();
        assert_eq!(app.input_cursor, 0);
    }

    /// Spec (tui — input cursor): End moves cursor to string length.
    #[test]
    fn test_spec_input_cursor_end() {
        let mut app = App::new();
        app.input = "abc".into();
        app.input_cursor = 0;
        app.input_cursor_end();
        assert_eq!(app.input_cursor, 3);
    }

    // --- word navigation ---

    /// Spec (tui — input cursor): word-left skips trailing whitespace then
    /// the contiguous word characters.
    #[test]
    fn test_spec_input_cursor_word_left_skips_whitespace_then_word() {
        let mut app = App::new();
        app.input = "hello world".into();
        app.input_cursor = 11; // end
        app.input_cursor_word_left();
        assert_eq!(app.input_cursor, 6, "should land at start of 'world'");
    }

    /// Spec (tui — input cursor): word-left from mid-word lands at the start
    /// of the current word.
    #[test]
    fn test_spec_input_cursor_word_left_mid_word() {
        let mut app = App::new();
        app.input = "hello world".into();
        app.input_cursor = 8; // inside "world"
        app.input_cursor_word_left();
        assert_eq!(app.input_cursor, 6, "should land at start of 'world'");
    }

    /// Spec (tui — input cursor): word-right skips the current word then the
    /// following whitespace.
    #[test]
    fn test_spec_input_cursor_word_right() {
        let mut app = App::new();
        app.input = "hello world".into();
        app.input_cursor = 0;
        app.input_cursor_word_right();
        assert_eq!(app.input_cursor, 6, "should land at start of 'world'");
    }

    /// Spec (tui — input cursor): word-right from inside a word lands at the
    /// start of the next word.
    #[test]
    fn test_spec_input_cursor_word_right_mid_word() {
        let mut app = App::new();
        app.input = "hello world".into();
        app.input_cursor = 2; // inside "hello"
        app.input_cursor_word_right();
        assert_eq!(app.input_cursor, 6);
    }

    // --- kill operations ---

    /// Spec (tui — input cursor): Ctrl-U (kill to start) deletes from cursor
    /// to beginning and resets cursor to 0.
    #[test]
    fn test_spec_input_kill_to_start() {
        let mut app = App::new();
        app.input = "hello world".into();
        app.input_cursor = 5;
        app.input_kill_to_start();
        assert_eq!(app.input, " world");
        assert_eq!(app.input_cursor, 0);
    }

    /// Spec (tui — input cursor): Ctrl-K (kill to end) deletes from cursor
    /// to end; cursor stays at new end.
    #[test]
    fn test_spec_input_kill_to_end() {
        let mut app = App::new();
        app.input = "hello world".into();
        app.input_cursor = 5;
        app.input_kill_to_end();
        assert_eq!(app.input, "hello");
        assert_eq!(app.input_cursor, 5);
    }

    // --- delete word (Ctrl-W) ---

    /// Spec (tui — input cursor): Ctrl-W deletes the word before the cursor.
    #[test]
    fn test_spec_input_delete_word() {
        let mut app = App::new();
        app.input = "hello world".into();
        app.input_cursor = 11; // end
        app.input_delete_word();
        assert_eq!(app.input, "hello ");
        assert_eq!(app.input_cursor, 6);
    }

    /// Spec (tui — input cursor): Ctrl-W skips trailing whitespace before
    /// deleting the word.
    #[test]
    fn test_spec_input_delete_word_skips_trailing_whitespace() {
        let mut app = App::new();
        app.input = "hello   ".into();
        app.input_cursor = 8;
        app.input_delete_word();
        assert_eq!(app.input, "");
        assert_eq!(app.input_cursor, 0);
    }

    // --- clear_input ---

    /// Spec (tui — input cursor): clear_input resets both the text and cursor.
    #[test]
    fn test_spec_clear_input_resets_cursor() {
        let mut app = App::new();
        app.input = "hello".into();
        app.input_cursor = 3;
        app.clear_input();
        assert!(app.input.is_empty());
        assert_eq!(app.input_cursor, 0);
    }

    // --- multi-byte safety ---

    /// Spec (tui — input cursor): multi-byte characters (emoji) must be
    /// inserted, navigated, and deleted correctly by char index. Marked
    /// spec (not security): char-based indexing correctness is a TUI
    /// correctness property, not a threat in security.md (whose threats
    /// T1–T5 cover TOML isolation, session leaks, ancestor merge, tool
    /// boundaries, and terminal corruption).
    #[test]
    fn test_spec_input_cursor_multibyte_insert_and_navigate() {
        let mut app = App::new();
        app.input_insert('a');
        app.input_insert('😀');
        app.input_insert('b');
        // input = "a😀b", cursor = 3
        assert_eq!(app.input, "a😀b");
        assert_eq!(app.input_cursor, 3);
        app.input_cursor_left(); // 2
        app.input_cursor_left(); // 1
        assert_eq!(app.input_cursor, 1); // before 😀
        app.input_backspace(); // delete 'a'
        assert_eq!(app.input, "😀b");
        assert_eq!(app.input_cursor, 0);
    }
}
