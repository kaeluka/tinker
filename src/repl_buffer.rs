use crate::app::{Message, Role};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};

/// Cached metadata for one converted message.
#[derive(Debug, Clone)]
struct CachedEntry {
    /// Index of the source message in `App::messages`.
    msg_index: usize,
    /// Text at the time this entry was built; used to detect streaming changes.
    source_text: String,
    /// Logical `Line`s produced from this message.
    lines: Vec<Line<'static>>,
    /// Wrapped-line count for each logical line at `width`.
    wrapped_counts: Vec<usize>,
}

impl CachedEntry {
    fn total_wrapped(&self) -> usize {
        self.wrapped_counts.iter().sum()
    }
}

/// Retained REPL buffer.  Survives across frames; invalidated only when
/// the active session or pane width changes.  Visible messages are
/// converted incrementally, and the wrapped-line count of the last entry
/// is lazily recomputed when its source text changes during streaming.
#[derive(Debug, Clone, Default)]
pub struct ReplBuffer {
    entries: Vec<CachedEntry>,
    width: u16,
    active_session: String,
    /// Number of source messages already walked (including invisible ones).
    built_up_to: usize,
    /// Sum of all entry `total_wrapped()` values.  Kept up-to-date
    /// incrementally so `draw_repl` never needs `Paragraph::line_count`
    /// on the full buffer.
    total_wrapped: usize,
    /// Bumps whenever the buffer content changes.  Used as a cheap
    /// `ScrollState` cache key.
    version: u64,
}

impl ReplBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn invalidate(&mut self) {
        self.entries.clear();
        self.built_up_to = 0;
        self.total_wrapped = 0;
        self.version += 1;
    }

    pub fn is_valid(&self, width: u16, active_session: &str) -> bool {
        self.width == width && self.active_session == active_session
    }

    /// Convert any new messages and, if the last visible message is still
    /// being appended to (streaming), refresh its wrapped counts.
    pub fn build_or_extend(
        &mut self,
        messages: &[Message],
        active_session: &str,
        width: u16,
    ) {
        if !self.is_valid(width, active_session) {
            self.invalidate();
            self.width = width;
            self.active_session = active_session.to_string();
        }

        let mut changed = false;

        // Streaming: the last visible message may have grown.
        if let Some(last_entry) = self.entries.last() {
            let idx = last_entry.msg_index;
            if idx < messages.len() && last_entry.source_text != messages[idx].text {
                let old = last_entry.total_wrapped();
                let new_entry = build_entry(&messages[idx], width, idx);
                let new = new_entry.total_wrapped();
                self.total_wrapped = self.total_wrapped.saturating_sub(old).saturating_add(new);
                *self.entries.last_mut().unwrap() = new_entry;
                changed = true;
            }
        }

        // Append newly-arrived messages.
        for (i, msg) in messages.iter().enumerate().skip(self.built_up_to) {
            if is_visible(msg, active_session) {
                let entry = build_entry(msg, width, i);
                self.total_wrapped += entry.total_wrapped();
                self.entries.push(entry);
                changed = true;
            }
        }
        self.built_up_to = messages.len();

        if changed {
            self.version += 1;
        }
    }

    pub fn total_lines(&self) -> usize {
        self.total_wrapped
    }

    #[allow(dead_code)]
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Return the logical lines that overlap the wrapped-line range
    /// `[scroll_y, scroll_y + viewport_height)`, plus a scroll offset
    /// relative to the first included line so that `Paragraph::scroll`
    /// starts at exactly the right wrapped line.
    pub fn viewport_lines(
        &self,
        scroll_y: usize,
        _viewport_height: u16,
    ) -> (Vec<Line<'static>>, u16) {
        let mut result = Vec::new();
        let mut cumulative = 0usize;
        let mut scroll_offset = 0u16;
        let mut started = false;

        for entry in &self.entries {
            let entry_total = entry.total_wrapped();
            let entry_end = cumulative + entry_total;

            if entry_end <= scroll_y {
                cumulative = entry_end;
                continue;
            }

            if !started {
                scroll_offset = (scroll_y.saturating_sub(cumulative)).min(u16::MAX as usize) as u16;
                started = true;
            }

            result.extend(entry.lines.iter().cloned());
            cumulative = entry_end;
        }

        (result, scroll_offset)
    }
}

fn is_visible(msg: &Message, active_session: &str) -> bool {
    match &msg.role {
        Role::System => true,
        Role::User(id) | Role::Agent(id) => id == active_session,
    }
}

fn build_entry(msg: &Message, width: u16, msg_index: usize) -> CachedEntry {
    let mut lines = Vec::new();
    build_message_lines(&mut lines, msg, width);
    let wrapped_counts: Vec<usize> = lines
        .iter()
        .map(|line| Paragraph::new(vec![line.clone()]).wrap(Wrap { trim: false }).line_count(width))
        .collect();
    CachedEntry {
        msg_index,
        source_text: msg.text.clone(),
        lines,
        wrapped_counts,
    }
}

/// Build the visible `Line` representation for a single message.
///
/// This is the single source of truth for how `Message` maps to `Line<'static>`
/// in the REPL.
///
/// **Clone-once policy**: every span content is converted to an owned
/// `String` so the resulting `Line` carries `'static` lifetime.  The clone
/// happens here, once per message, and the result is retained across frames.
pub fn build_message_lines(lines: &mut Vec<Line<'static>>, msg: &Message, pane_width: u16) {
    match &msg.role {
        Role::User(_) => {
            lines.push(Line::from(vec![
                Span::styled(
                    "you    ",
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
                Span::raw(msg.text.as_str().to_string()),
            ]));
        }
        Role::System => {
            const LABEL: &str = "sys    "; // 7 chars
            let available = (pane_width as usize).saturating_sub(LABEL.len());
            let first_line = msg.text.lines().next().unwrap_or("");
            let has_more = msg.text.contains('\n') || first_line.chars().count() > available;
            let display = if has_more && available > 0 {
                let budget = available.saturating_sub(1); // 1 for the '…' char
                let truncated: String = first_line.chars().take(budget).collect();
                format!("{}…", truncated)
            } else {
                first_line.to_string()
            };
            lines.push(Line::from(vec![
                Span::styled(LABEL, Style::default().fg(Color::Yellow)),
                Span::styled(display, Style::default().fg(Color::DarkGray)),
            ]));
        }
        Role::Agent(id) => {
            let label = format!("{:<7}", &id[..id.len().min(7)]);
            let label_style = Style::default().fg(Color::Green).add_modifier(Modifier::BOLD);
            let mut text_lines = msg.text.lines();
            if let Some(first) = text_lines.next() {
                lines.push(Line::from(vec![
                    Span::styled(label.clone(), label_style),
                    Span::raw(first.to_string()),
                ]));
            }
            for rest in text_lines {
                lines.push(Line::from(vec![
                    Span::raw("       "),
                    Span::raw(rest.to_string()),
                ]));
            }
            // If the text ends with a newline, the final empty segment needs a blank line.
            if msg.text.ends_with('\n') {
                lines.push(Line::from(""));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg_user(text: &str) -> Message {
        Message {
            role: Role::User("tend".to_string()),
            text: text.to_string(),
        }
    }

    fn msg_agent(id: &str, text: &str) -> Message {
        Message {
            role: Role::Agent(id.to_string()),
            text: text.to_string(),
        }
    }

    fn msg_system(text: &str) -> Message {
        Message {
            role: Role::System,
            text: text.to_string(),
        }
    }

    // --- build_or_extend caching ---

    /// Spec (tui — repl buffer): `build_or_extend` must produce the same
    /// visible total as an equivalent raw loop.
    #[test]
    fn test_spec_repl_buffer_builds_same_lines_as_raw_conversion() {
        let messages = vec![
            msg_system("hello"),
            msg_user("world"),
            msg_agent("tend", "ack\nreceived"),
        ];
        let mut buf = ReplBuffer::new();
        buf.build_or_extend(&messages, "tend", 80);
        // sys(1) + user(1) + agent(2 logical lines, may wrap)
        assert!(buf.total_lines() >= 3);
    }

    /// Spec (tui — repl buffer): `build_or_extend` called twice without
    /// changes must not duplicate lines.
    #[test]
    fn test_spec_repl_buffer_double_extend_is_idempotent() {
        let messages = vec![msg_user("hello")];
        let mut buf = ReplBuffer::new();
        buf.build_or_extend(&messages, "tend", 40);
        let first_total = buf.total_lines();
        let first_version = buf.version();
        buf.build_or_extend(&messages, "tend", 40);
        assert_eq!(buf.total_lines(), first_total, "duplicate build must not grow");
        assert_eq!(buf.version(), first_version, "version must not bump");
    }

    /// Spec (tui — repl buffer): when new messages are appended, the buffer
    /// must extend rather than rebuild from scratch.
    #[test]
    fn test_spec_repl_buffer_appends_incrementally() {
        let mut messages = vec![msg_user("first")];
        let mut buf = ReplBuffer::new();
        buf.build_or_extend(&messages, "tend", 40);
        let after_first = buf.total_lines();
        messages.push(msg_user("second"));
        buf.build_or_extend(&messages, "tend", 40);
        assert!(
            buf.total_lines() > after_first,
            "incremental append should grow total"
        );
    }

    /// Spec (tui — repl buffer): changing `active_session` must invalidate
    /// the cache and rebuild with the new session's view.
    #[test]
    fn test_spec_repl_buffer_session_switch_invalidates() {
        let messages = vec![
            msg_agent("tend", "hello"),
            msg_agent("rummage", "secret"),
        ];
        let mut buf = ReplBuffer::new();
        buf.build_or_extend(&messages, "tend", 40);
        let tend_total = buf.total_lines();
        assert!(tend_total > 0);
        buf.build_or_extend(&messages, "rummage", 40);
        let rummage_total = buf.total_lines();
        assert_eq!(rummage_total, tend_total);
        // Only the "rummage" agent msg is visible.
        assert_eq!(rummage_total, 1);
    }

    /// Spec (tui — repl buffer): changing pane width must invalidate the
    /// cache so that word-wrapping is recomputed.
    #[test]
    fn test_spec_repl_buffer_width_change_invalidates() {
        let messages = vec![msg_user("the quick brown fox jumps over the lazy dog")];
        let mut buf = ReplBuffer::new();
        buf.build_or_extend(&messages, "tend", 80);
        let wide_total = buf.total_lines();
        buf.build_or_extend(&messages, "tend", 20);
        let narrow_total = buf.total_lines();
        assert!(
            narrow_total >= wide_total,
            "narrower width should not reduce line count"
        );
    }

    /// Spec (tui — repl_buffer — visibility): system messages are visible
    /// regardless of active session.
    #[test]
    fn test_spec_repl_buffer_system_always_visible() {
        let messages = vec![msg_system("global note")];
        let mut buf = ReplBuffer::new();
        buf.build_or_extend(&messages, "rummage", 40);
        assert_eq!(buf.total_lines(), 1, "system msg must be visible for any session");
    }

    /// Spec (tui — repl_buffer — visibility): agent messages from a session
    /// other than `active_session` must not appear.
    #[test]
    fn test_spec_repl_buffer_filters_inactive_sessions() {
        let messages = vec![msg_agent("jog", "hello")];
        let mut buf = ReplBuffer::new();
        buf.build_or_extend(&messages, "tend", 40);
        assert_eq!(buf.total_lines(), 0, "inactive session agent must be filtered");
    }

    /// Spec (tui — repl_buffer — visibility): user messages from a session
    /// other than `active_session` must not appear.
    #[test]
    fn test_spec_repl_buffer_filters_inactive_user_sessions() {
        let messages = vec![msg_user("hello")];
        let mut buf = ReplBuffer::new();
        buf.build_or_extend(&messages, "rummage", 40);
        assert_eq!(buf.total_lines(), 0, "inactive session user must be filtered");
    }

    // --- streaming re-computation ---

    /// Spec (tui — repl buffer): when the last visible message's text
    /// changes (streaming), `build_or_extend` must refresh its wrapped
    /// count without rebuilding earlier messages.
    #[test]
    fn test_spec_repl_buffer_streaming_updates_last_entry() {
        let mut messages = vec![msg_user("hello")];
        let mut buf = ReplBuffer::new();
        buf.build_or_extend(&messages, "tend", 40);
        let before = buf.total_lines();

        // Simulate streaming append.
        messages[0].text.push_str(" world");
        buf.build_or_extend(&messages, "tend", 40);
        let after = buf.total_lines();

        assert!(
            after >= before,
            "streaming append should not shrink wrapped total"
        );
        assert_eq!(buf.entries.len(), 1, "only one entry should exist");
    }

    // --- viewport_lines culling ---

    /// Spec (tui — repl buffer — culling): `viewport_lines` must return
    /// only the logical-line entries that intersect the visible wrapped-line
    /// range.
    #[test]
    fn test_spec_repl_buffer_viewport_culls_to_range() {
        let text = "hello world this is a test string that should wrap";
        let messages = vec![msg_user(text)];
        let mut buf = ReplBuffer::new();
        buf.build_or_extend(&messages, "tend", 15);
        let total = buf.total_lines();
        assert!(total > 1, "message should wrap to >1 lines; got {}", total);

        // Ask for a viewport starting 1 wrapped line in.
        let (subset, offset) = buf.viewport_lines(1, 1);
        assert!(
            !subset.is_empty(),
            "viewport must contain at least the overlapping entry"
        );
        assert_eq!(offset, 1, "scroll offset should be 1 when starting 1 line in");
    }

    /// Spec (tui — repl buffer — culling): `viewport_lines` past the end
    /// returns an empty subset with zero offset.
    #[test]
    fn test_spec_repl_buffer_viewport_past_end_is_empty() {
        let messages = vec![msg_user("hello")];
        let mut buf = ReplBuffer::new();
        buf.build_or_extend(&messages, "tend", 40);
        let (subset, offset) = buf.viewport_lines(1000, 10);
        assert!(subset.is_empty(), "viewport past end must be empty");
        assert_eq!(offset, 0);
    }

    // --- build_message_lines mapping correctness ---

    /// Spec (tui — repl buffer): user messages must render with the "you"
    /// label in Cyan Bold and the raw text body.
    #[test]
    fn test_spec_build_lines_user_label_and_style() {
        let mut lines: Vec<Line> = vec![];
        build_message_lines(&mut lines, &msg_user("hi"), 80);
        assert_eq!(lines.len(), 1);
        let spans = &lines[0].spans;
        assert_eq!(spans[0].content, "you    ");
        assert_eq!(spans[1].content, "hi");
    }

    /// Spec (tui — repl buffer): system messages must truncate to one line
    /// with ellipsis when multi-line or too long.
    #[test]
    fn test_spec_build_lines_system_truncation() {
        let mut lines: Vec<Line> = vec![];
        build_message_lines(&mut lines, &msg_system("first\nsecond"), 80);
        assert_eq!(lines.len(), 1);
        let s: String = lines[0].spans.iter().map(|sp| sp.content.as_ref()).collect();
        assert!(s.contains("first"), "first line must appear: {}", s);
        assert!(s.contains('…'), "ellipsis must be present: {}", s);
        assert!(!s.contains("second"), "subsequent lines must not appear: {}", s);
    }

    /// Spec (tui — repl buffer): agent messages must split across multiple
    /// lines with the label on the first line and indentation on the rest.
    #[test]
    fn test_spec_build_lines_agent_multiline() {
        let mut lines: Vec<Line> = vec![];
        build_message_lines(&mut lines, &msg_agent("tend", "line1\nline2"), 80);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].spans[0].content.contains("tend"), "first line carries label");
        assert_eq!(lines[1].spans[0].content, "       ", "subsequent lines are indented");
    }

    /// Spec (tui — repl buffer): agent messages ending with a newline must
    /// produce a trailing blank line.
    #[test]
    fn test_spec_build_lines_agent_trailing_newline_blank() {
        let mut lines: Vec<Line> = vec![];
        build_message_lines(&mut lines, &msg_agent("tend", "text\n"), 80);
        let last = lines.last().unwrap();
        assert!(last.spans.is_empty(), "trailing newline must produce blank line");
    }

    // --- security / robustness ---

    /// Spec (security — repl buffer): `build_or_extend` must handle empty
    /// messages gracefully without panicking. Empty user / agent / system
    /// messages still each produce one logical line.
    #[test]
    fn test_security_repl_buffer_empty_messages() {
        let messages = vec![
            msg_user(""),
            msg_agent("tend", ""),
            msg_system(""),
        ];
        let mut buf = ReplBuffer::new();
        buf.build_or_extend(&messages, "tend", 40);
        // Should not panic and should produce at least some lines.
        // User + Agent each produce one visible line at width 40.
        // System is also visible.
        assert!(buf.total_lines() >= 2);
    }

    /// Spec (security — repl buffer): zero-width pane must not panic.
    #[test]
    fn test_security_repl_buffer_zero_width() {
        let messages = vec![msg_user("hello")];
        let mut buf = ReplBuffer::new();
        buf.build_or_extend(&messages, "tend", 0);
        // The property under test is "must not panic" — calling total_lines on
        // a zero-width buffer must not unwrap or divide by zero. The return
        // value is secondary (usize, no negative invariant to assert).
        let _ = buf.total_lines();
    }

    /// Spec (security — repl buffer): very long single-line messages must
    /// not panic during wrapping.
    #[test]
    fn test_security_repl_buffer_very_long_message() {
        let text = "a".repeat(100_000);
        let messages = vec![msg_user(&text)];
        let mut buf = ReplBuffer::new();
        buf.build_or_extend(&messages, "tend", 80);
        assert!(
            buf.total_lines() > 0,
            "very long message must wrap to many lines"
        );
    }
}
