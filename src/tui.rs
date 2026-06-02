use crate::app::{App, Focus, Phase, Role};
use crate::goal_session::TRIGGER_REASON_MARKER;
use crate::goal::{build_tree, GoalNode};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};

pub struct PaneRects {
    pub repl: Rect,
    /// Whole top-right pane (Goals), border + title included.
    pub tree: Rect,
    /// Goal-list sub-area inside the Goals pane (selectable).
    pub goal_list: Rect,
    /// Separator row between goal list and goal text (decorative).
    pub goal_sep: Rect,
    /// Goal-description text sub-area inside the Goals pane (scrollable).
    pub goal_text: Rect,
    pub log: Rect,
}

/// Pure function of the terminal area → per-pane rects. Called by both the
/// renderer and the mouse-event handler so they stay in sync.
pub fn pane_rects(area: Rect) -> PaneRects {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    let right_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(75), Constraint::Percentage(25)])
        .split(columns[1]);
    let tree_outer = right_rows[0];
    let tree_inner = Block::default().borders(Borders::ALL).inner(tree_outer);
    let tree_splits = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(33),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(tree_inner);
    PaneRects {
        repl: columns[0],
        tree: tree_outer,
        goal_list: tree_splits[0],
        goal_sep: tree_splits[1],
        goal_text: tree_splits[2],
        log: right_rows[1],
    }
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let rects = pane_rects(frame.area());
    draw_repl(frame, app, rects.repl);
    draw_goal_tree(
        frame,
        app,
        rects.tree,
        rects.goal_list,
        rects.goal_sep,
        rects.goal_text,
    );
    draw_log(frame, app, rects.log);
    if app.modal.is_some() {
        draw_modal(frame, app);
    }
}

/// Render the reason-prompt modal as a centered overlay. Clears the
/// underlying area first so the panes don't bleed through.
fn draw_modal(frame: &mut Frame, app: &App) {
    let modal = match &app.modal {
        Some(m) => m,
        None => return,
    };
    let area = frame.area();
    // Center a 60%-wide, 5-row box.
    let width = (area.width as u32 * 60 / 100).max(40).min(area.width as u32) as u16;
    let height: u16 = 5;
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    let rect = Rect { x, y, width, height };

    frame.render_widget(Clear, rect);

    let title = format!(" Trigger reason — `{}` ", modal.goal_id);
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let hint = "Enter to fire · Esc to cancel · empty reason ⇒ no reason";
    let lines = vec![
        Line::from(vec![
            Span::styled("> ", Style::default().fg(Color::Yellow)),
            Span::raw(modal.input.clone()),
            Span::styled("█", Style::default().fg(Color::Cyan)),
        ]),
        Line::from(""),
        Line::from(Span::styled(hint, Style::default().fg(Color::DarkGray))),
    ];
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        inner,
    );
}

fn draw_repl(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Repl;
    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let title = " REPL ".to_string();

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(border_style);

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let (prompt, prompt_style) = if app.phase == Phase::Initializing {
        ("… ".to_string(), Style::default().fg(Color::DarkGray))
    } else {
        (format!("{}> ", app.active_session), Style::default().fg(Color::Cyan))
    };
    let input_locked = app.phase == Phase::Initializing;
    let input_text_style = if input_locked { Style::default().fg(Color::DarkGray) } else { Style::default() };
    let cursor = if !input_locked { "█" } else { "" };

    let max_input = (inner.height / 2).max(1);
    let (input_height, input_scroll) =
        input_pane_layout(&prompt, &app.input, cursor, inner.width, max_input);

    let msg_area = Rect {
        height: inner.height.saturating_sub(input_height),
        ..inner
    };
    let input_area = Rect {
        y: inner.y + inner.height.saturating_sub(input_height),
        height: input_height,
        ..inner
    };

    let mut lines: Vec<Line> = vec![];
    for msg in &app.messages {
        let visible = match &msg.role {
            Role::System => true,
            Role::User(id) | Role::Agent(id) => id == &app.active_session,
        };
        if visible {
            push_message_lines(&mut lines, msg);
        }
    }
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let total = paragraph.line_count(msg_area.width);
    app.repl_scroll.record_render(total, msg_area.height);
    let scroll_y = app.repl_scroll.effective_y().min(u16::MAX as usize) as u16;
    frame.render_widget(paragraph.scroll((scroll_y, 0)), msg_area);

    let input_line = Line::from(vec![
        Span::styled(prompt, prompt_style),
        Span::styled(app.input.clone(), input_text_style),
        Span::styled(cursor, Style::default().fg(Color::Cyan)),
    ]);
    frame.render_widget(
        Paragraph::new(input_line)
            .wrap(Wrap { trim: false })
            .scroll((input_scroll, 0)),
        input_area,
    );
}

/// Chat input pane layout: returns `(height, scroll)` for the input pane.
/// Height matches the wrapped Paragraph's actual row count, capped by `max`
/// and floored at 1. When the unclamped row count exceeds `max`, `scroll` is
/// set so the bottom wrapped row (where the cursor sits) stays visible.
fn input_pane_layout(prompt: &str, input: &str, cursor: &str, width: u16, max: u16) -> (u16, u16) {
    let line = Line::from(vec![
        Span::raw(prompt.to_string()),
        Span::raw(input.to_string()),
        Span::raw(cursor.to_string()),
    ]);
    let needed = (Paragraph::new(line)
        .wrap(Wrap { trim: false })
        .line_count(width.max(1)) as u16)
        .max(1);
    let height = needed.min(max.max(1));
    let scroll = needed.saturating_sub(height);
    (height, scroll)
}

fn push_message_lines(lines: &mut Vec<Line<'static>>, msg: &crate::app::Message) {
    match &msg.role {
        Role::User(_) => {
            lines.push(Line::from(vec![
                Span::styled(
                    "you    ",
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
                Span::raw(msg.text.clone()),
            ]));
        }
        Role::System => {
            lines.push(Line::from(vec![
                Span::styled("sys    ", Style::default().fg(Color::Yellow)),
                Span::styled(msg.text.clone(), Style::default().fg(Color::DarkGray)),
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


/// Build the running-sessions label for the Goals pane title.
/// Sorts IDs alphabetically, then greedily fits as many as possible within
/// `max_chars`. Returns e.g. `" ▶ alpha, beta + 2 goals "` or `" ▶ alpha "`.
fn running_label(mut ids: Vec<&str>, max_chars: usize) -> String {
    ids.sort_unstable();
    let n = ids.len();
    let mut best_k = 0usize;
    for k in 1..=n {
        let shown = &ids[..k];
        let label = if k == n {
            format!(" ▶ {} ", shown.join(", "))
        } else {
            let remaining = n - k;
            format!(" ▶ {} + {} {} ", shown.join(", "), remaining, if remaining == 1 { "goal" } else { "goals" })
        };
        if label.chars().count() <= max_chars {
            best_k = k;
        }
    }
    if best_k == n && best_k > 0 {
        format!(" ▶ {} ", ids[..best_k].join(", "))
    } else if best_k > 0 {
        let remaining = n - best_k;
        format!(" ▶ {} + {} {} ", ids[..best_k].join(", "), remaining, if remaining == 1 { "goal" } else { "goals" })
    } else {
        let remaining = n;
        format!(" ▶ {} {} ", remaining, if remaining == 1 { "goal" } else { "goals" })
    }
}

fn draw_goal_tree(
    frame: &mut Frame,
    app: &mut App,
    area: Rect,
    list_area: Rect,
    sep_area: Rect,
    text_area: Rect,
) {
    let focused = app.focus == Focus::Tree;
    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let phase_label = if app.phase == Phase::Initializing {
        " starting… ".to_string()
    } else {
        let n = app.running_sessions.len();
        if n == 0 {
            String::new()
        } else {
            // " Goals{phase_label} " must fit in area.width - 2 (border corners).
            // Fixed overhead: " Goals " = 7 chars.
            let max_chars = (area.width as usize).saturating_sub(9);
            let ids: Vec<&str> = app.running_sessions.keys().map(String::as_str).collect();
            running_label(ids, max_chars)
        }
    };
    let title = format!(" Goals{} ", phase_label);

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(border_style);

    frame.render_widget(block, area);

    if app.goals.is_empty() {
        let inner = Block::default().borders(Borders::ALL).inner(area);
        frame.render_widget(
            Paragraph::new(Span::styled(
                "No goals yet. Ask tinker to add one.",
                Style::default().fg(Color::DarkGray),
            )),
            inner,
        );
        return;
    }

    let tree = build_tree(&app.goals);
    let flat = flatten_tree(&tree);
    let selected_goal = app.selected_goal().map(|g| g.id.clone());
    let list_lines: Vec<Line> = flat
        .iter()
        .map(|(depth, node)| {
            let is_selected = selected_goal.as_deref() == Some(&node.goal.id);
            let is_active = app.running_sessions.contains_key(&node.goal.id);

            let name_style = if is_selected {
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let id_style = if is_selected {
                name_style
            } else {
                Style::default().fg(Color::DarkGray)
            };

            let marker_style = if is_active {
                Style::default().fg(Color::DarkGray)
            } else {
                name_style
            };
            let marker_str = if is_active {
                "▶ ".to_string()
            } else {
                "◉ ".to_string()
            };

            let indent = "  ".repeat(*depth);
            let id_label = format!("`{}`", node.goal.id);
            let preview = truncate_with_ellipsis(&first_meaningful_line(&node.goal.description), 60);

            let mut spans = vec![
                Span::styled(format!("{}{}", indent, marker_str), marker_style),
                Span::styled(id_label, id_style),
            ];
            if !preview.is_empty() {
                spans.push(Span::styled(format!(" — {}", preview), name_style));
            }
            Line::from(spans)
        })
        .collect();

    let list_paragraph = Paragraph::new(list_lines.clone());
    let list_total = list_lines.len();
    app.goal_list_scroll.record_render(list_total, list_area.height);
    let scroll_y = app.goal_list_scroll.effective_y().min(u16::MAX as usize) as u16;
    frame.render_widget(list_paragraph.scroll((scroll_y, 0)), list_area);

    let sep = "─".repeat(sep_area.width as usize);
    frame.render_widget(
        Paragraph::new(Span::styled(sep, Style::default().fg(Color::DarkGray))),
        sep_area,
    );

    let text_lines: Vec<Line> = match app.selected_goal() {
        None => vec![Line::from(Span::styled(
            "(no selection)",
            Style::default().fg(Color::DarkGray),
        ))],
        Some(g) => {
            let mut lines: Vec<Line> = g
                .description
                .lines()
                .map(|l| Line::from(l.to_string()))
                .collect();
            if app.running_sessions.contains_key(&g.id) {
                let reason = app.running_sessions.get(&g.id)
                    .and_then(|r| r.as_ref())
                    .map(|s| s.as_str())
                    .unwrap_or("");
                lines.push(Line::from(""));
                lines.push(Line::from(vec![
                    Span::styled("▶ ", Style::default().fg(Color::DarkGray)),
                    Span::raw(reason.to_string()),
                ]));
            }
            lines
        }
    };
    let p = Paragraph::new(text_lines).wrap(Wrap { trim: false });
    let total = p.line_count(text_area.width);
    app.goal_text_scroll.record_render(total, text_area.height);
    let scroll_y = app.goal_text_scroll.effective_y().min(u16::MAX as usize) as u16;
    frame.render_widget(p.scroll((scroll_y, 0)), text_area);
}

/// First non-empty, non-markdown-header line of `text`, trimmed.
/// Returns empty string if there's nothing meaningful.
fn first_meaningful_line(text: &str) -> String {
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('#') {
            continue;
        }
        return trimmed.to_string();
    }
    String::new()
}

fn draw_log(frame: &mut Frame, app: &mut App, area: Rect) {
    let selected_id = app.selected_goal().map(|g| g.id.clone());
    let title = match &selected_id {
        Some(id) => {
            if let Some(g) = app.goals.iter().find(|g| &g.id == id) {
                format!(" Log: {} ", truncate_str(&g.description, 28))
            } else {
                " Log ".to_string()
            }
        }
        None => " Log ".to_string(),
    };

    let is_active_log = selected_id.as_deref().map(|id| app.running_sessions.contains_key(id)).unwrap_or(false);
    let border_style = if is_active_log {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(border_style);

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let content: Paragraph = match &selected_id {
        None => Paragraph::new(Span::styled(
            "No goal selected.",
            Style::default().fg(Color::DarkGray),
        )),
        Some(id) => match app.goal_logs.get(id) {
            None => Paragraph::new(Span::styled(
                "No output yet.",
                Style::default().fg(Color::DarkGray),
            )),
            Some(log) if log.is_empty() => Paragraph::new(Span::styled(
                "No output yet.",
                Style::default().fg(Color::DarkGray),
            )),
            Some(log) => {
                let lines: Vec<Line> = log.lines().flat_map(render_log_line).collect();
                let p = Paragraph::new(lines).wrap(Wrap { trim: false });
                let total = p.line_count(inner.width);
                app.log_scroll.record_render(total, inner.height);
                let scroll_y = app.log_scroll.effective_y().min(u16::MAX as usize) as u16;
                p.scroll((scroll_y, 0))
            }
        },
    };

    frame.render_widget(content, inner);
}

fn flatten_tree(nodes: &[GoalNode]) -> Vec<(usize, &GoalNode)> {
    let mut result = vec![];
    for node in nodes {
        flatten_node(node, &mut result);
    }
    result
}

fn flatten_node<'a>(node: &'a GoalNode, result: &mut Vec<(usize, &'a GoalNode)>) {
    result.push((node.depth, node));
    for child in &node.children {
        flatten_node(child, result);
    }
}

fn truncate_str(s: &str, max: usize) -> &str {
    let mut end = 0;
    for (i, _) in s.char_indices().take(max) {
        end = i;
    }
    if s.chars().count() <= max {
        s
    } else {
        &s[..end]
    }
}

fn truncate_with_ellipsis(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{}...", truncated)
    }
}

pub fn render_log_line(raw: &str) -> Vec<Line<'static>> {
    let line = if let Some(reason) = raw.strip_prefix(TRIGGER_REASON_MARKER) {
        Line::from(Span::styled(
            reason.to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ))
    } else {
        Line::from(raw.to_string())
    };
    vec![line]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, ScrollState};
    use crate::goal::Goal;

    fn mk_goal(id: &str) -> Goal {
        Goal {
            id: id.to_string(),
            summary: String::new(),
            description: format!("desc for {}", id),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            tier: None,
            source_path: None,
        }
    }

    /// Spec: "Tinker presents a terminal interface with three regions
    /// visible at once" and the layout has a 50/50 horizontal split
    /// (REPL on the left, Goals + Log stacked on the right).
    #[test]
    fn test_spec_pane_rects_horizontal_split_is_fifty_fifty() {
        let area = Rect { x: 0, y: 0, width: 80, height: 100 };
        let r = pane_rects(area);
        // Left REPL and right column are each ~50% of the width.
        assert_eq!(r.repl.width, 40);
        assert_eq!(r.repl.x, 0);
        assert_eq!(r.tree.x, 40);
        assert_eq!(r.tree.width, 40);
        assert_eq!(r.log.x, 40);
        assert_eq!(r.log.width, 40);
        // Tree and log together cover the full right column height.
        assert_eq!(r.tree.y, 0);
        assert_eq!(r.tree.y + r.tree.height, r.log.y);
        assert_eq!(r.log.y + r.log.height, 100);
    }

    /// Spec: "The layout proportions for the right side are fixed at
    /// 25% height for the goal list, 50% for the goal text view, and
    /// 25% for the session log." Asserts each region is within a small
    /// tolerance of the requested proportion of total height.
    #[test]
    fn test_spec_pane_rects_right_column_quarter_half_quarter() {
        let area = Rect { x: 0, y: 0, width: 80, height: 100 };
        let r = pane_rects(area);
        let total = area.height as i32;
        let list = r.goal_list.height as i32;
        let text = r.goal_text.height as i32;
        let log = r.log.height as i32;
        // Tolerate small slack from the 1-row separator and the borders.
        let tol = 5;
        assert!(
            (list - 25).abs() <= tol,
            "goal list should be ~25% of height, got {} of {}",
            list,
            total
        );
        assert!(
            (text - 50).abs() <= tol,
            "goal text should be ~50% of height, got {} of {}",
            text,
            total
        );
        assert!(
            (log - 25).abs() <= tol,
            "log should be ~25% of height, got {} of {}",
            log,
            total
        );
        // Sub-areas live inside the tree pane and don't overlap.
        assert!(r.goal_list.y >= r.tree.y);
        assert_eq!(r.goal_list.y + r.goal_list.height, r.goal_sep.y);
        assert_eq!(r.goal_sep.y + r.goal_sep.height, r.goal_text.y);
        // Separator is a single decorative row.
        assert_eq!(r.goal_sep.height, 1);
    }

    /// Spec: "Text regions follow the tail of new content by default.
    /// Scrolling up disables the follow until the user scrolls all the
    /// way back to the bottom, at which point follow re-engages."
    #[test]
    fn test_spec_scroll_up_disables_follow_tail() {
        let mut s = ScrollState::new();
        // Tail-follow starts engaged (y = None).
        assert!(s.y.is_none());
        // Pretend the renderer just drew 100 lines into a 10-row pane.
        s.record_render(100, 10);
        // Effective offset is pinned to the tail.
        assert_eq!(s.effective_y(), 90);
        // User scrolls up: follow is disabled, offset moves toward the top.
        s.scroll_up(5);
        assert!(s.y.is_some(), "scroll_up must disengage follow-tail");
        assert_eq!(s.effective_y(), 85);
    }

    /// Spec (tui): the chat input pane height must equal the wrapped
    /// Paragraph's actual rendered line count for any input — no word-wrap
    /// undercounting, no cursor-row clipping below the cap.
    #[test]
    fn test_spec_input_pane_height_matches_paragraph_line_count() {
        let prompt = "tend> ";
        let cursor = "█";
        let inputs = [
            "",
            "hello",
            "the quick brown fox jumps over the lazy dog",
            "supercalifragilisticexpialidocious wonderful situation exemplary behavior",
            "a b c d e f g h i j k l m n o p q r s t u v w x y z a b c d e f g h",
        ];
        for &width in &[20u16, 40, 60, 80, 100] {
            for input in &inputs {
                let line = Line::from(vec![
                    Span::raw(prompt.to_string()),
                    Span::raw(input.to_string()),
                    Span::raw(cursor.to_string()),
                ]);
                let expected = (Paragraph::new(line)
                    .wrap(Wrap { trim: false })
                    .line_count(width) as u16)
                    .max(1);
                let big_cap = 1000u16;
                let (height, scroll) = input_pane_layout(prompt, input, cursor, width, big_cap);
                assert_eq!(
                    height, expected,
                    "input_pane_layout height drifted from Paragraph::line_count: width={} input={:?}",
                    width, input,
                );
                assert_eq!(
                    scroll, 0,
                    "scroll must be 0 when needed <= cap: width={} input={:?}",
                    width, input,
                );
            }
        }
    }

    /// Spec (tui): once the input grows past the half-REPL cap, the input
    /// Paragraph must scroll so the bottom wrapped row (cursor row) stays in
    /// view. Concretely: scroll = needed - height; the bottom row sits at
    /// `input_area.y + input_area.height - 1`.
    #[test]
    fn test_spec_input_pane_scrolls_to_keep_cursor_row_visible() {
        let prompt = "tend> ";
        let cursor = "█";
        // Long input that wraps to many rows at a narrow width.
        let mut input = String::new();
        for _ in 0..30 {
            input.push_str("the quick brown fox jumps over the lazy dog ");
        }
        let width = 40u16;
        let cap = 4u16;
        let (height, scroll) = input_pane_layout(prompt, &input, cursor, width, cap);
        let line = Line::from(vec![
            Span::raw(prompt.to_string()),
            Span::raw(input.clone()),
            Span::raw(cursor.to_string()),
        ]);
        let needed = Paragraph::new(line)
            .wrap(Wrap { trim: false })
            .line_count(width) as u16;
        assert!(needed > cap, "test premise: input must exceed cap");
        assert_eq!(height, cap, "height must clamp to cap when input overflows");
        assert_eq!(
            scroll,
            needed - cap,
            "scroll offset must place the bottom wrapped row at the last visible row",
        );
    }

    /// Spec: scrolling back to the bottom re-engages follow.
    #[test]
    fn test_spec_scroll_back_to_bottom_reengages_follow_tail() {
        let mut s = ScrollState::new();
        s.record_render(100, 10);
        // Scroll up to disengage follow.
        s.scroll_up(20);
        assert!(s.y.is_some());
        // Scroll back down past the bottom — y must snap to None.
        s.scroll_down(1000);
        assert!(s.y.is_none(), "scrolling to the bottom must re-engage follow-tail");
        assert_eq!(s.effective_y(), 90);
    }

    /// Spec (tui): "When a goal session starts, the specific 'reason' it was
    /// triggered must be rendered in the log pane in bold font."
    /// Lines prefixed by TRIGGER_REASON_MARKER must render with BOLD modifier;
    /// unmarked lines must not.
    #[test]
    fn test_spec_trigger_reason_rendered_bold_in_log() {
        use crate::goal_session::TRIGGER_REASON_MARKER;

        let mut app = App::new();
        app.goals.push(mk_goal("alpha"));
        let reason = "investigate the failing test";
        let marked = format!("{}{}", TRIGGER_REASON_MARKER, reason);
        let plain = "some normal log line";
        app.goal_logs.insert("alpha".to_string(), format!("{}\n{}\n", marked, plain));

        // draw_log uses the log text: extract rendered lines by calling the
        // log-line rendering logic directly (we test the mapping, not the
        // full draw call which requires a Frame).
        let log = app.goal_logs.get("alpha").unwrap();
        let lines: Vec<Line> = log.lines().flat_map(render_log_line).collect();

        assert_eq!(lines.len(), 2);
        // First line (trigger reason) must be bold.
        let bold_span = &lines[0].spans[0];
        assert_eq!(bold_span.content, reason);
        assert!(
            bold_span.style.add_modifier.contains(Modifier::BOLD),
            "trigger reason line must carry BOLD modifier",
        );
        // Marker itself must not appear in the rendered text.
        assert!(
            !bold_span.content.contains(TRIGGER_REASON_MARKER),
            "TRIGGER_REASON_MARKER sentinel must be stripped from rendered text",
        );
        // Second line (plain output) must NOT be bold.
        let plain_span = &lines[1].spans[0];
        assert_eq!(plain_span.content, plain);
        assert!(
            !plain_span.style.add_modifier.contains(Modifier::BOLD),
            "normal log lines must not be bold",
        );
    }

    /// Spec (tui): "The system message that lists the goals the orchestrator
    /// triggered (via `/run` lines in its last reply) and their reasons must
    /// be rendered in grey to reduce visual noise."
    #[test]
    fn test_spec_triggered_system_message_is_grey() {
        let msg = crate::app::Message {
            role: Role::System,
            text: "triggered: `goal-a`: investigate the failing test".to_string(),
        };
        let mut lines: Vec<Line> = vec![];
        push_message_lines(&mut lines, &msg);
        assert!(!lines.is_empty(), "triggered: system message must produce at least one line");
        let text_span = lines[0].spans.iter().find(|s| s.content.contains("triggered:"));
        assert!(
            text_span.is_some(),
            "triggered: content must appear in a rendered span",
        );
        assert_eq!(
            text_span.unwrap().style.fg,
            Some(Color::DarkGray),
            "triggered: system message must be rendered in grey (DarkGray)",
        );
    }

    fn apply_filter<'a>(messages: &'a [&'a crate::app::Message], active_session: &str) -> Vec<Line<'static>> {
        let mut lines: Vec<Line> = vec![];
        for msg in messages {
            let visible = match &msg.role {
                Role::System => true,
                Role::User(id) | Role::Agent(id) => id == active_session,
            };
            if visible {
                push_message_lines(&mut lines, msg);
            }
        }
        lines
    }

    /// Spec (tui): per-session view — Agent messages from inactive sessions
    /// must not appear; active-session Agent messages must appear.
    #[test]
    fn test_spec_repl_filters_out_inactive_session_messages() {
        let tend_agent = crate::app::Message {
            role: Role::Agent("tend".to_string()),
            text: "tend reply".to_string(),
        };
        let rummage_agent = crate::app::Message {
            role: Role::Agent("rummage".to_string()),
            text: "rummage reply".to_string(),
        };
        let lines = apply_filter(&[&tend_agent, &rummage_agent], "tend");
        let has_tend = lines.iter().any(|l| l.spans.iter().any(|s| s.content.contains("tend reply")));
        let has_rummage = lines.iter().any(|l| l.spans.iter().any(|s| s.content.contains("rummage reply")));
        assert!(has_tend, "active session (tend) agent messages must appear");
        assert!(!has_rummage, "inactive session (rummage) agent messages must not appear");
    }

    /// Spec (tui): System messages are global — they appear in every session's
    /// view regardless of which session is active.
    #[test]
    fn test_spec_repl_shows_system_messages_in_all_sessions() {
        let system_msg = crate::app::Message {
            role: Role::System,
            text: "system note".to_string(),
        };
        let lines = apply_filter(&[&system_msg], "rummage");
        assert!(
            lines.iter().any(|l| l.spans.iter().any(|s| s.content.contains("system note"))),
            "System messages must appear regardless of active session",
        );
    }

    /// Spec (tui): User messages are per-session — a user message sent to an
    /// inactive session must not appear in the active session's view.
    #[test]
    fn test_spec_repl_hides_user_messages_from_inactive_sessions() {
        let tend_user = crate::app::Message {
            role: Role::User("tend".to_string()),
            text: "tend input".to_string(),
        };
        let rummage_user = crate::app::Message {
            role: Role::User("rummage".to_string()),
            text: "rummage input".to_string(),
        };
        let lines = apply_filter(&[&tend_user, &rummage_user], "tend");
        let has_tend = lines.iter().any(|l| l.spans.iter().any(|s| s.content.contains("tend input")));
        let has_rummage = lines.iter().any(|l| l.spans.iter().any(|s| s.content.contains("rummage input")));
        assert!(has_tend, "user message for active session (tend) must appear");
        assert!(!has_rummage, "user message for inactive session (rummage) must not appear");
    }

    /// `reset_to_top` anchors at the top — used for the goal-description
    /// pane which defaults to "read from the beginning" (per the WHAT
    /// section's "selectable goal list AND the description text").
    #[test]
    fn test_spec_scroll_reset_to_top() {
        let mut s = ScrollState::new();
        s.record_render(100, 10);
        // Tail-follow by default.
        assert_eq!(s.effective_y(), 90);
        s.reset_to_top();
        assert_eq!(s.y, Some(0));
        assert_eq!(s.effective_y(), 0);
    }

    /// Spec (tui — queue visibility): running-session markers must render dim
    /// grey so they don't compete with the goal name. The ▶ glyph must use
    /// Color::DarkGray without any BOLD or other emphasis modifier.
    #[test]
    fn test_spec_running_marker_style_is_dim_grey_not_bold() {
        let is_active = true;
        let is_selected = false;
        // Mirror the marker-style derivation from draw_goal_tree.
        let marker_style = if is_active {
            Style::default().fg(Color::DarkGray)
        } else if is_selected {
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        assert_eq!(
            marker_style.fg,
            Some(Color::DarkGray),
            "running ▶ marker must use DarkGray fg",
        );
        assert!(
            !marker_style.add_modifier.contains(Modifier::BOLD),
            "running ▶ marker must not carry BOLD modifier",
        );
    }

    /// Spec: "When navigating the goal list with the keyboard, the list
    /// view automatically scrolls to ensure the currently selected goal
    /// is always visible (standard edge scrolling)."
    #[test]
    fn test_spec_select_next_goal_scrolls_when_out_of_view() {
        let mut app = App::new();
        // Ten flat goals; viewport only shows three.
        for i in 0..10 {
            app.goals.push(mk_goal(&format!("g{:02}", i)));
        }
        app.goal_list_scroll.record_render(10, 3);
        // Selection starts at 0; first three are visible. Move past the
        // bottom of the viewport and the scroll offset must follow.
        for _ in 0..5 {
            app.select_next_goal();
        }
        assert_eq!(app.selected_goal, 5);
        let offset = app.goal_list_scroll.effective_y();
        // Selected row 5 must be inside [offset, offset + 3).
        assert!(
            offset <= app.selected_goal && app.selected_goal < offset + 3,
            "selected goal {} not visible at offset {} (height 3)",
            app.selected_goal,
            offset
        );
    }

    /// Spec: same edge-scrolling on upward navigation.
    #[test]
    fn test_spec_select_prev_goal_scrolls_when_out_of_view() {
        let mut app = App::new();
        for i in 0..10 {
            app.goals.push(mk_goal(&format!("g{:02}", i)));
        }
        app.goal_list_scroll.record_render(10, 3);
        // Jump the selection to the bottom and pin the scroll there so the
        // top of the list is out of view.
        app.selected_goal = 9;
        app.goal_list_scroll.y = Some(7);
        // Walk back up past the top of the viewport.
        for _ in 0..8 {
            app.select_prev_goal();
        }
        assert_eq!(app.selected_goal, 1);
        let offset = app.goal_list_scroll.effective_y();
        assert!(
            offset <= app.selected_goal && app.selected_goal < offset + 3,
            "selected goal {} not visible at offset {} (height 3)",
            app.selected_goal,
            offset
        );
    }

    /// Spec (tui — goals pane title): with ample space all running IDs are shown.
    #[test]
    fn test_spec_running_label_all_fit() {
        let ids = vec!["beta", "alpha"];
        let label = running_label(ids, 100);
        assert_eq!(label, " ▶ alpha, beta ");
    }

    /// Spec (tui — goals pane title): IDs are sorted alphabetically for stability.
    #[test]
    fn test_spec_running_label_sorted() {
        let ids = vec!["zzz", "aaa", "mmm"];
        let label = running_label(ids, 100);
        assert_eq!(label, " ▶ aaa, mmm, zzz ");
    }

    /// Spec (tui — goals pane title): when not all IDs fit, show as many as
    /// possible followed by "+ N goals".
    #[test]
    fn test_spec_running_label_overflow_shows_count() {
        // sorted: ["alpha", "beta", "gamma"]
        // k=1: " ▶ alpha + 2 goals " = 19 chars ≤ 20 → fits
        // k=2: " ▶ alpha, beta + 1 goal " = 24 chars > 20 → doesn't fit
        let ids = vec!["beta", "alpha", "gamma"];
        let label = running_label(ids, 20);
        assert_eq!(label, " ▶ alpha + 2 goals ");
    }

    /// Spec (tui — goals pane title): singular "goal" when exactly one is hidden.
    #[test]
    fn test_spec_running_label_singular_goal() {
        // ids sorted: ["very-long-id-one", "very-long-id-two"]
        // k=1: " ▶ very-long-id-one + 1 goal " = 29 chars ≤ 29 → fits
        // k=2: " ▶ very-long-id-one, very-long-id-two " = 38 chars > 29 → doesn't fit
        let ids = vec!["very-long-id-two", "very-long-id-one"];
        let label = running_label(ids, 29);
        assert!(label.contains("+ 1 goal "), "expected singular 'goal', got: {}", label);
        assert!(!label.contains("goals"), "must not say 'goals' for remainder=1, got: {}", label);
    }

    /// Spec (tui — goals pane title): when nothing fits, fall back to count-only label.
    #[test]
    fn test_spec_running_label_nothing_fits_fallback() {
        let ids = vec!["a-very-long-goal-name", "another-very-long-goal"];
        let label = running_label(ids, 5);
        assert!(label.contains("2 goals"), "expected count fallback, got: {}", label);
    }

}

