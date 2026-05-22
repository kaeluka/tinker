use crate::goal::Goal;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub enum Role {
    User,
    Assistant,
    System,
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

/// State for the reason-prompt modal that pops up when the user triggers a
/// goal via Enter in the goal tree (or `/run` with no arguments). The modal
/// owns keyboard focus while `App.modal` is `Some`; submitting fires the goal
/// with the typed reason, escape cancels. Closing the modal doesn't touch
/// `App.focus`, so the previous pane focus is preserved automatically.
#[derive(Debug, Clone)]
pub struct ModalState {
    pub goal_id: String,
    pub input: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LoopMode {
    Auto,
    Manual,
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
}

impl ScrollState {
    pub fn new() -> Self {
        Self { y: None, last_total: 0, last_height: 0 }
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

    pub fn record_render(&mut self, total: usize, height: u16) {
        self.last_total = total;
        self.last_height = height;
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
    /// Sending the init prompt to the orchestrator.
    Initializing,
    /// Nothing running; waiting for user input or new goals.
    Idle,
    /// A goal session is actively running.
    RunningGoal(String),
    /// Manual mode: next goal chosen, waiting for user to approve.
    AwaitingConfirm(String),
    /// Batch finished — asking the orchestrator to summarize what was done.
    SummarizingBatch,
}

pub struct App {
    pub messages: Vec<Message>,
    pub current_assistant_text: String,
    pub input: String,
    pub goals: Vec<Goal>,
    /// All `.tinker` directories discovered at startup (cwd + ancestors).
    pub tinker_dirs: Vec<PathBuf>,
    /// Currently-failing TOML files: (path, short error). Used to deduplicate
    /// system messages so each new failure is announced once.
    pub parse_errors: Vec<(PathBuf, String)>,
    pub selected_goal: usize,
    pub active_goal_id: Option<String>,
    pub goal_logs: HashMap<String, String>,
    pub orchestrator_session_id: Option<String>,
    /// How many orchestrator LLM tasks are currently running or queued.
    pub orchestrator_tasks: usize,
    /// Trigger reason for the currently-running goal session.
    /// Cleared when the session finishes or is blocked.
    pub active_goal_reason: Option<String>,
    /// Set to true after the user sends their first REPL message.
    /// Once true, the cold-start scheduling filter is disabled.
    pub user_has_interacted: bool,
    pub phase: Phase,
    pub loop_mode: LoopMode,
    /// Goals queued to run after the current one finishes (from a multi-goal
    /// scheduling response). Drained before triggering a new schedule.
    /// Each entry is `(Goal, optional reason)` — the reason is the per-trigger
    /// "what to do right now" hint emitted by the orchestrator's `/run` line
    /// or by the scheduler's `yes|<reason>` reply.
    pub goal_queue: VecDeque<(Goal, Option<String>)>,
    /// True if any goal session has fired since the last batch summary.
    /// Used to decide whether to ask for a batch summary when scheduling returns `none`.
    pub batch_had_goals: bool,
    /// (goal_id, summary) entries accumulated for the current batch.
    /// Forwarded to the orchestrator in the batch summary request, then cleared.
    pub batch_summaries: Vec<(String, String)>,
    /// How many times we've asked the orchestrator to fix a parse error in this
    /// edit cycle. Reset on a fresh user message or a clean Done.
    pub correction_attempts: u8,
    pub focus: Focus,
    pub should_quit: bool,
    pub repl_scroll: ScrollState,
    pub log_scroll: ScrollState,
    pub goal_text_scroll: ScrollState,
    pub goal_list_scroll: ScrollState,
    /// When `Some`, the reason-prompt modal is open; all keys route to it
    /// until submit/cancel. The previous `focus` is preserved unchanged.
    pub modal: Option<ModalState>,
}

impl App {
    pub fn new() -> Self {
        Self {
            messages: vec![],
            current_assistant_text: String::new(),
            input: String::new(),
            goals: vec![],
            tinker_dirs: vec![],
            parse_errors: vec![],
            selected_goal: 0,
            active_goal_id: None,
            active_goal_reason: None,
            goal_logs: HashMap::new(),
            orchestrator_session_id: None,
            orchestrator_tasks: 0,
            user_has_interacted: false,
            phase: Phase::Initializing,
            loop_mode: LoopMode::Auto,
            goal_queue: VecDeque::new(),
            batch_had_goals: false,
            batch_summaries: vec![],
            correction_attempts: 0,
            focus: Focus::Repl,
            should_quit: false,
            repl_scroll: ScrollState::new(),
            log_scroll: ScrollState::new(),
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
        }
    }

    pub fn push_user_message(&mut self, text: &str) {
        self.messages.push(Message { role: Role::User, text: text.to_string() });
    }

    pub fn append_assistant_chunk(&mut self, chunk: &str) {
        self.current_assistant_text.push_str(chunk);
    }

    pub fn finalize_assistant_message(&mut self) {
        if !self.current_assistant_text.is_empty() {
            let text = std::mem::take(&mut self.current_assistant_text);
            self.messages.push(Message { role: Role::Assistant, text });
        }
    }

    pub fn push_system_message(&mut self, text: &str) {
        self.messages.push(Message { role: Role::System, text: text.to_string() });
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

    pub fn append_goal_log(&mut self, goal_id: &str, text: &str) {
        self.goal_logs.entry(goal_id.to_string()).or_default().push_str(text);
    }

    pub fn selected_goal(&self) -> Option<Goal> {
        self.flat_goals().into_iter().nth(self.selected_goal)
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

    pub fn select_next_goal(&mut self) {
        let n = self.flat_goals().len();
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

}
