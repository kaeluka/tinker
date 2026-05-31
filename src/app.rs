use crate::goal::Goal;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::PathBuf;

/// A goal agent session waiting to run.
#[derive(Debug, Clone)]
pub struct GoalQueueEntry {
    pub goal_id: String,
    /// Formatted message to deliver to the session when it starts.
    pub message: String,
    /// Short trigger reason for TUI display (may equal message for keyboard triggers).
    pub display_reason: String,
}

#[derive(Debug, Clone, PartialEq)]
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
    /// Sending the init prompt to tend.
    Initializing,
    /// Ready; sessions may be running but there is no serial queue.
    Idle,
}

pub struct App {
    pub messages: Vec<Message>,
    pub input: String,
    pub goals: Vec<Goal>,
    pub tinker_dirs: Vec<PathBuf>,
    pub parse_errors: Vec<(PathBuf, String)>,
    pub selected_goal: usize,
    /// Goal sessions currently running, mapped to the reason they were triggered.
    pub running_sessions: HashMap<String, Option<String>>,
    /// Goal agents waiting for the current session to finish (serial execution order).
    pub goal_queue: VecDeque<GoalQueueEntry>,
    pub goal_logs: HashMap<String, String>,
    pub user_has_interacted: bool,
    pub phase: Phase,
    /// How many times we've asked tend to fix a parse error in this
    /// edit cycle. Reset on a fresh user message or a clean Done.
    pub correction_attempts: u8,
    pub current_session_text: HashMap<String, String>,
    pub focus: Focus,
    pub should_quit: bool,
    pub repl_scroll: ScrollState,
    pub log_scroll: ScrollState,
    pub goal_text_scroll: ScrollState,
    pub goal_list_scroll: ScrollState,
    pub modal: Option<ModalState>,
    /// Which session currently receives the user's REPL input (goal-id string).
    pub active_session: String,
    /// Tracks the index in `messages` of the current in-progress agent turn
    /// for each session, so incoming chunks can append to that slot in-place.
    pub agent_msg_idx: HashMap<String, usize>,
}

impl App {
    pub fn new() -> Self {
        Self {
            messages: vec![],
            input: String::new(),
            goals: vec![],
            tinker_dirs: vec![],
            parse_errors: vec![],
            selected_goal: 0,
            running_sessions: HashMap::new(),
            goal_queue: VecDeque::new(),
            goal_logs: HashMap::new(),
            user_has_interacted: false,
            phase: Phase::Initializing,
            correction_attempts: 0,
            current_session_text: HashMap::new(),
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
            active_session: "tend".to_string(),
            agent_msg_idx: HashMap::new(),
        }
    }

    pub fn push_user_message(&mut self, text: &str, session_id: &str) {
        self.messages.push(Message { role: Role::User(session_id.to_string()), text: text.to_string() });
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

    /// Append agent text to the REPL message list. The first chunk for a new
    /// turn creates a new `Role::Agent` message; subsequent chunks extend it
    /// in-place so the REPL doesn't accumulate thousands of tiny entries.
    pub fn append_agent_message(&mut self, goal_id: &str, text: &str) {
        if let Some(&idx) = self.agent_msg_idx.get(goal_id) {
            if let Some(msg) = self.messages.get_mut(idx) {
                msg.text.push_str(text);
                return;
            }
        }
        let idx = self.messages.len();
        self.messages.push(Message { role: Role::Agent(goal_id.to_string()), text: text.to_string() });
        self.agent_msg_idx.insert(goal_id.to_string(), idx);
    }

    /// Call when a session turn ends so the next turn opens a fresh message.
    pub fn finalize_agent_message(&mut self, goal_id: &str) {
        self.agent_msg_idx.remove(goal_id);
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
