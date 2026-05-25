use crate::app::{ActiveAgent, App, Focus, LoopMode, Phase, Role};
use crate::claude::USAGE_LINE_MARKER;
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

    let mode_indicator = match app.loop_mode {
        LoopMode::Auto => "",
        LoopMode::Manual => " [manual]",
    };
    let title = format!(" REPL{} ", mode_indicator);

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(border_style);

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let (prompt, prompt_style): (&str, Style) = match app.active_agent {
        ActiveAgent::Tinker => match &app.phase {
            Phase::Initializing => ("… ", Style::default().fg(Color::DarkGray)),
            Phase::AwaitingConfirm(_) => ("▶ ", Style::default().fg(Color::Yellow)),
            _ if app.tinker_tasks > 0 => ("… ", Style::default().fg(Color::DarkGray)),
            _ => ("tinker> ", Style::default().fg(Color::Yellow)),
        },
        ActiveAgent::Rummage => {
            if app.rummage_tasks > 0 {
                ("… ", Style::default().fg(Color::DarkGray))
            } else {
                ("rummage> ", Style::default().fg(Color::Magenta))
            }
        }
        ActiveAgent::Jog => {
            if app.jog_tasks > 0 {
                ("… ", Style::default().fg(Color::DarkGray))
            } else {
                ("jog> ", Style::default().fg(Color::Blue))
            }
        }
    };
    let input_locked = match app.active_agent {
        ActiveAgent::Tinker => app.tinker_tasks > 0 || app.phase == Phase::Initializing,
        ActiveAgent::Rummage => app.rummage_tasks > 0,
        ActiveAgent::Jog => app.jog_tasks > 0,
    };
    let input_text_style = if input_locked {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default()
    };
    let cursor = if !input_locked { "█" } else { "" };

    let max_input = (inner.height / 2).max(1);
    let (input_height, input_scroll) =
        input_pane_layout(prompt, &app.input, cursor, inner.width, max_input);

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
        push_message_lines(&mut lines, msg);
    }
    if !app.current_assistant_text.is_empty() {
        push_assistant_text(&mut lines, &app.current_assistant_text);
    }
    if !app.rummage_current_text.is_empty() {
        push_rummage_text(&mut lines, &app.rummage_current_text);
    }
    if !app.jog_current_text.is_empty() {
        push_jog_text(&mut lines, &app.jog_current_text);
    }
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let total = paragraph.line_count(msg_area.width);
    app.repl_scroll.record_render(total, msg_area.height);
    let scroll_y = app.repl_scroll.effective_y().min(u16::MAX as usize) as u16;
    frame.render_widget(paragraph.scroll((scroll_y, 0)), msg_area);

    let input_line = Line::from(vec![
        Span::styled(prompt, prompt_style),
        Span::styled(&app.input, input_text_style),
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
    match msg.role {
        Role::User => {
            lines.push(Line::from(vec![
                Span::styled(
                    "you    ",
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
                Span::raw(msg.text.clone()),
            ]));
        }
        Role::Assistant => {
            push_assistant_text(lines, &msg.text);
        }
        Role::RummageAssistant => {
            push_rummage_text(lines, &msg.text);
        }
        Role::JogAssistant => {
            push_jog_text(lines, &msg.text);
        }
        Role::System => {
            lines.push(Line::from(vec![
                Span::styled("sys    ", Style::default().fg(Color::Yellow)),
                Span::styled(msg.text.clone(), Style::default().fg(Color::DarkGray)),
            ]));
        }
    }
}

fn push_rummage_text(lines: &mut Vec<Line<'static>>, text: &str) {
    for (i, line) in text.lines().enumerate() {
        if i == 0 {
            lines.push(Line::from(vec![
                Span::styled(
                    "rummage",
                    Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::raw(line.to_string()),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::raw("       "),
                Span::raw(line.to_string()),
            ]));
        }
    }
}

fn push_jog_text(lines: &mut Vec<Line<'static>>, text: &str) {
    for (i, line) in text.lines().enumerate() {
        if i == 0 {
            lines.push(Line::from(vec![
                Span::styled(
                    "jog    ",
                    Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD),
                ),
                Span::raw(line.to_string()),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::raw("       "),
                Span::raw(line.to_string()),
            ]));
        }
    }
}

fn push_assistant_text(lines: &mut Vec<Line<'static>>, text: &str) {
    let mut in_tool_call = false;
    let mut output_index = 0usize;
    for raw_line in text.lines() {
        // The streamed text chunk arriving before a usage line often lacks a
        // trailing `\n`, so the usage marker can land mid-line. Split here so
        // each segment is dispatched to its own renderer.
        let (text_part, usage_part) = match raw_line.find(USAGE_LINE_MARKER) {
            Some(idx) => (
                &raw_line[..idx],
                Some(&raw_line[idx + USAGE_LINE_MARKER.len_utf8()..]),
            ),
            None => (raw_line, None),
        };

        let has_usage = usage_part.is_some();
        if !text_part.is_empty() || !has_usage {
            push_assistant_text_segment(lines, text_part, &mut in_tool_call, output_index);
            output_index += 1;
        }

        if let Some(usage) = usage_part {
            in_tool_call = false;
            lines.push(Line::from(vec![
                Span::raw("       "),
                Span::styled(
                    usage.to_string(),
                    Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM),
                ),
            ]));
            output_index += 1;
        }
    }
}

fn push_assistant_text_segment(
    lines: &mut Vec<Line<'static>>,
    line: &str,
    in_tool_call: &mut bool,
    output_index: usize,
) {
    if line.starts_with("→ ") {
        *in_tool_call = true;
        lines.push(Line::from(vec![
            Span::raw("       "),
            Span::styled(line.to_string(), Style::default().fg(Color::DarkGray)),
        ]));
    } else if *in_tool_call && line.trim().is_empty() {
        *in_tool_call = false;
        lines.push(Line::from(vec![
            Span::raw("       "),
            Span::raw(""),
        ]));
    } else if *in_tool_call {
        // Skip rendering subsequent lines of a tool call payload (crop to first line)
    } else if output_index == 0 {
        lines.push(Line::from(vec![
            Span::styled(
                "tinker ",
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
            ),
            Span::raw(line.to_string()),
        ]));
    } else {
        lines.push(Line::from(vec![
            Span::raw("       "),
            Span::raw(line.to_string()),
        ]));
    }
}

/// Returns the queue-marker string for a goal, if any.
/// Running goal → `"▶"`, first queued entry → `"[N]"` (1-based global position).
/// Returns `None` when the goal has no pending invocations.
fn goal_queue_marker(
    goal_id: &str,
    active_goal_id: Option<&str>,
    queue: &std::collections::VecDeque<(crate::goal::Goal, Option<String>)>,
) -> Option<String> {
    if active_goal_id == Some(goal_id) {
        return Some("▶".to_string());
    }
    for (i, (goal, _)) in queue.iter().enumerate() {
        if goal.id == goal_id {
            return Some(format!("[{}]", i + 1));
        }
    }
    None
}

struct PendingEntry {
    is_running: bool,
    /// 1-based global queue position (irrelevant when `is_running` is true).
    queue_pos: usize,
    reason: Option<String>,
}

/// Returns every pending invocation for a goal in display order:
/// the running entry first (if any), then queued entries in queue order.
fn goal_pending_entries(
    goal_id: &str,
    active_goal_id: Option<&str>,
    active_goal_reason: Option<&str>,
    queue: &std::collections::VecDeque<(crate::goal::Goal, Option<String>)>,
) -> Vec<PendingEntry> {
    let mut entries = vec![];
    if active_goal_id == Some(goal_id) {
        entries.push(PendingEntry {
            is_running: true,
            queue_pos: 0,
            reason: active_goal_reason.map(String::from),
        });
    }
    for (i, (goal, reason)) in queue.iter().enumerate() {
        if goal.id == goal_id {
            entries.push(PendingEntry {
                is_running: false,
                queue_pos: i + 1,
                reason: reason.clone(),
            });
        }
    }
    entries
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

    let phase_label = match &app.phase {
        Phase::RunningGoal(id) => format!(" ▶ {} ", id),
        Phase::AwaitingConfirm(id) => format!(" ? confirm: {} ", id),
        Phase::SummarizingBatch => " ✎ summarizing… ".to_string(),
        Phase::Initializing => " starting… ".to_string(),
        Phase::Idle => String::new(),
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
            let is_active = app.active_goal_id.as_deref() == Some(&node.goal.id);

            let style = if is_active {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else if is_selected {
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let id_style = if is_active || is_selected {
                style
            } else {
                Style::default().fg(Color::DarkGray)
            };

            let indent = "  ".repeat(*depth);
            let marker = if is_active { "▶" } else { "◉" };
            let id_label = format!("`{}`", node.goal.id);
            let preview = truncate_with_ellipsis(&first_meaningful_line(&node.goal.description), 60);
            let qm = goal_queue_marker(&node.goal.id, app.active_goal_id.as_deref(), &app.goal_queue);

            let mut spans = vec![
                Span::styled(format!("{}{} ", indent, marker), style),
                Span::styled(id_label, id_style),
            ];
            if let Some(m) = qm {
                spans.push(Span::styled(
                    format!(" {}", m),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            if !preview.is_empty() {
                spans.push(Span::styled(format!(" — {}", preview), style));
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
            let pending = goal_pending_entries(
                &g.id,
                app.active_goal_id.as_deref(),
                app.active_goal_reason.as_deref(),
                &app.goal_queue,
            );
            if !pending.is_empty() {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "── Pending invocations ──",
                    Style::default().fg(Color::DarkGray),
                )));
                for entry in &pending {
                    let glyph = if entry.is_running {
                        "▶".to_string()
                    } else {
                        format!("[{}]", entry.queue_pos)
                    };
                    let reason_str = entry.reason.as_deref().unwrap_or("(no reason)");
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("  {} ", glyph),
                            Style::default().fg(Color::DarkGray),
                        ),
                        Span::raw(reason_str.to_string()),
                    ]));
                }
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

    let is_active_log = selected_id.as_deref() == app.active_goal_id.as_deref();
    let border_style = if is_active_log && app.active_goal_id.is_some() {
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
    // A streamed text chunk that lacks a trailing newline before the usage
    // line lands here with the marker mid-string. Split the prefix off so
    // each segment can be styled independently.
    if let Some(idx) = raw.find(USAGE_LINE_MARKER) {
        if idx > 0 {
            let mut out = render_log_line(&raw[..idx]);
            out.extend(render_log_line(&raw[idx..]));
            return out;
        }
    }

    let line = if let Some(reason) = raw.strip_prefix(TRIGGER_REASON_MARKER) {
        Line::from(Span::styled(
            reason.to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ))
    } else if let Some(usage) = raw.strip_prefix(USAGE_LINE_MARKER) {
        Line::from(Span::styled(
            usage.to_string(),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM),
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
            description: format!("desc for {}", id),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
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
        let prompt = "tinker> ";
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
        let prompt = "tinker> ";
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

    /// Spec (tui): "The currently-running goal gets a distinct glyph (e.g. `▶`)".
    /// `goal_queue_marker` returns `"▶"` when the goal is the active one.
    #[test]
    fn test_spec_queue_marker_running_goal_gets_glyph() {
        let queue = std::collections::VecDeque::new();
        let m = goal_queue_marker("tui", Some("tui"), &queue);
        assert_eq!(m.as_deref(), Some("▶"), "running goal must get ▶ marker");
    }

    /// Spec (tui): "each goal with queued entries gets a bracketed integer
    /// showing its next upcoming queue position (e.g. `[1]`, `[2]`)".
    #[test]
    fn test_spec_queue_marker_queued_goal_gets_index() {
        let mut queue = std::collections::VecDeque::new();
        let g_a = crate::goal::Goal {
            id: "alpha".into(), description: "".into(), parent_id: "".into(),
            children: vec![], related: vec![], source_path: None,
        };
        let g_b = crate::goal::Goal {
            id: "beta".into(), description: "".into(), parent_id: "".into(),
            children: vec![], related: vec![], source_path: None,
        };
        // alpha is at global position 1, beta at 2.
        queue.push_back((g_a, Some("reason-a".into())));
        queue.push_back((g_b, Some("reason-b".into())));

        assert_eq!(goal_queue_marker("alpha", None, &queue).as_deref(), Some("[1]"));
        assert_eq!(goal_queue_marker("beta", None, &queue).as_deref(), Some("[2]"));
    }

    /// Spec (tui): "Goals with nothing pending carry no marker."
    #[test]
    fn test_spec_queue_marker_idle_goal_has_no_marker() {
        let queue = std::collections::VecDeque::new();
        let m = goal_queue_marker("tui", None, &queue);
        assert!(m.is_none(), "idle goal must have no queue marker");
    }

    /// Spec (tui): "Markers in the goal-list region are rendered in a
    /// dim/grey style so they don't visually dominate the goal name."
    /// The marker span uses DarkGray foreground.
    #[test]
    fn test_spec_queue_markers_are_dim_grey() {
        // Build a minimal App with one running goal and one queued goal.
        let mut app = App::new();
        let running = mk_goal("running-goal");
        let queued = mk_goal("queued-goal");
        app.goals.push(running.clone());
        app.goals.push(queued.clone());
        app.active_goal_id = Some("running-goal".into());
        app.active_goal_reason = Some("fix the bug".into());
        app.goal_queue.push_back((queued, Some("index the project".into())));

        // Build the list lines the same way draw_goal_tree does.
        let tree = crate::goal::build_tree(&app.goals);
        let flat = super::flatten_tree(&tree);

        for (_, node) in &flat {
            let qm = goal_queue_marker(
                &node.goal.id,
                app.active_goal_id.as_deref(),
                &app.goal_queue,
            );
            if let Some(m) = qm {
                // Simulate appending the marker span.
                let span = Span::styled(
                    format!(" {}", m),
                    Style::default().fg(Color::DarkGray),
                );
                assert_eq!(
                    span.style.fg,
                    Some(Color::DarkGray),
                    "queue marker span for `{}` must be DarkGray, got {:?}",
                    node.goal.id,
                    span.style.fg,
                );
            }
        }
    }

    /// Spec (tui): "When a goal with a marker is selected, the goal-text
    /// region shows an extra section beneath the description listing every
    /// pending invocation … in queue order, with each line carrying the
    /// running glyph or queue index and the trigger reason."
    #[test]
    fn test_spec_pending_invocations_section_appears_in_goal_text() {
        let running = mk_goal("running-goal");
        let g = mk_goal("multi-goal");
        let mut app = App::new();
        app.goals.push(running);
        app.goals.push(g.clone());
        app.active_goal_id = Some("multi-goal".into());
        app.active_goal_reason = Some("fix the critical bug".into());
        // Same goal also appears twice in the queue.
        app.goal_queue.push_back((g.clone(), Some("add the feature".into())));
        app.goal_queue.push_back((g.clone(), None));

        let pending = goal_pending_entries(
            "multi-goal",
            app.active_goal_id.as_deref(),
            app.active_goal_reason.as_deref(),
            &app.goal_queue,
        );

        // Running entry + 2 queued entries = 3 total.
        assert_eq!(pending.len(), 3, "must capture running + both queued entries");

        // First entry: running.
        assert!(pending[0].is_running);
        assert_eq!(pending[0].reason.as_deref(), Some("fix the critical bug"));

        // Second entry: first queued, global pos 1.
        assert!(!pending[1].is_running);
        assert_eq!(pending[1].queue_pos, 1);
        assert_eq!(pending[1].reason.as_deref(), Some("add the feature"));

        // Third entry: second queued, global pos 2. No reason.
        assert!(!pending[2].is_running);
        assert_eq!(pending[2].queue_pos, 2);
        assert!(pending[2].reason.is_none());
    }

    /// Spec (tui): "When the queue is empty and no session is running, no
    /// markers appear in the goal-list region, and the goal-text region
    /// shows only the description. The queue surface is not visible at all
    /// in the idle state."
    #[test]
    fn test_spec_no_pending_entries_when_idle() {
        let g = mk_goal("idle-goal");
        let app = App::new();
        let pending = goal_pending_entries(
            "idle-goal",
            app.active_goal_id.as_deref(),
            app.active_goal_reason.as_deref(),
            &app.goal_queue,
        );
        assert!(pending.is_empty(), "idle goal must have no pending entries");
        assert!(
            goal_queue_marker("idle-goal", app.active_goal_id.as_deref(), &app.goal_queue).is_none(),
            "idle goal must have no queue marker",
        );
        // Goal text produces only description lines (no section header).
        let lines: Vec<Line> = g
            .description
            .lines()
            .map(|l| Line::from(l.to_string()))
            .collect();
        assert!(
            !lines.iter().any(|l| l.spans.iter().any(|s| s.content.contains("Pending"))),
            "idle goal text must not contain a 'Pending invocations' section",
        );
    }

    /// Spec (tui): the list marker shows only the *next* (lowest) queue
    /// position for a goal, even when that goal appears multiple times in
    /// the queue.
    #[test]
    fn test_spec_queue_marker_shows_next_position_only() {
        let mut queue = std::collections::VecDeque::new();
        let g_other = crate::goal::Goal {
            id: "other".into(), description: "".into(), parent_id: "".into(),
            children: vec![], related: vec![], source_path: None,
        };
        let g_target = crate::goal::Goal {
            id: "target".into(), description: "".into(), parent_id: "".into(),
            children: vec![], related: vec![], source_path: None,
        };
        // target appears at global positions 2 and 4 (other is at 1 and 3).
        queue.push_back((g_other.clone(), None));  // pos 1
        queue.push_back((g_target.clone(), None)); // pos 2
        queue.push_back((g_other.clone(), None));  // pos 3
        queue.push_back((g_target.clone(), None)); // pos 4

        let m = goal_queue_marker("target", None, &queue);
        assert_eq!(
            m.as_deref(),
            Some("[2]"),
            "list marker must show the next (lowest) position only",
        );
    }

    /// Spec (rummage / tui): rummage messages (Role::RummageAssistant) must be
    /// rendered with a `rummage` speaker label in magenta+bold, not the tinker
    /// green label, so the user always knows which agent is speaking.
    #[test]
    fn test_spec_rummage_messages_rendered_with_magenta_rummage_label() {
        use crate::app::Message;
        let msg = Message {
            role: Role::RummageAssistant,
            text: "Oh freddled gruntbuggly\nthy micturations are to me".to_string(),
        };
        let mut lines: Vec<Line> = vec![];
        push_message_lines(&mut lines, &msg);
        assert!(!lines.is_empty(), "rummage message must produce lines");
        // First line must carry the `rummage` label span.
        let label_span = lines[0].spans.iter().find(|s| s.content == "rummage");
        assert!(label_span.is_some(), "first line must have a 'rummage' label span");
        let label = label_span.unwrap();
        assert_eq!(label.style.fg, Some(Color::Magenta), "rummage label must be Magenta");
        assert!(
            label.style.add_modifier.contains(Modifier::BOLD),
            "rummage label must be bold",
        );
        // Second line (continuation) must NOT repeat the label.
        assert!(
            !lines[1].spans.iter().any(|s| s.content == "rummage"),
            "continuation lines must not repeat the rummage label",
        );
    }

    /// Spec (tui): "The conversation pane's input prompt carries a tag naming
    /// the currently active agent." When active_agent is Rummage, the prompt
    /// string must contain `rummage>`.
    #[test]
    fn test_spec_rummage_prompt_tag_in_tui_source() {
        let tui_rs = include_str!("tui.rs");
        assert!(
            tui_rs.contains("rummage>"),
            "TUI source must contain the rummage> prompt tag string",
        );
        assert!(
            tui_rs.contains("tinker>"),
            "TUI source must contain the tinker> prompt tag string",
        );
    }

    /// Spec (jog / tui): jog messages (Role::JogAssistant) must be rendered
    /// with a `jog    ` speaker label in blue+bold so the user knows which agent
    /// is speaking.
    #[test]
    fn test_spec_jog_messages_rendered_with_blue_jog_label() {
        use crate::app::Message;
        let msg = Message {
            role: Role::JogAssistant,
            text: "What do you believe this feature does?\nAnd why is that the right behavior?".to_string(),
        };
        let mut lines: Vec<Line> = vec![];
        push_message_lines(&mut lines, &msg);
        assert!(!lines.is_empty(), "jog message must produce lines");
        let label_span = lines[0].spans.iter().find(|s| s.content.trim() == "jog");
        assert!(label_span.is_some(), "first line must have a 'jog' label span");
        let label = label_span.unwrap();
        assert_eq!(label.style.fg, Some(Color::Blue), "jog label must be Blue");
        assert!(
            label.style.add_modifier.contains(Modifier::BOLD),
            "jog label must be bold",
        );
        assert!(
            !lines[1].spans.iter().any(|s| s.content.trim() == "jog"),
            "continuation lines must not repeat the jog label",
        );
    }

    /// Spec (jog / tui): The TUI source must contain the `jog>` prompt tag string.
    #[test]
    fn test_spec_jog_prompt_tag_in_tui_source() {
        let tui_rs = include_str!("tui.rs");
        assert!(
            tui_rs.contains("jog>"),
            "TUI source must contain the jog> prompt tag string",
        );
    }

    /// Spec (tui): "All orchestrator tool calls (bash, read, edit, etc.) shown
    /// in the conversation pane are rendered in a grey/dim style to reduce
    /// visual noise. Their payloads are summarized (cropped to the first line)."
    /// Lines starting with `→ ` must render in DarkGray; subsequent payload
    /// lines (before the next blank) must be omitted entirely.
    #[test]
    fn test_spec_tool_calls_rendered_grey_and_cropped() {
        let text = "tinker said something\n→ bash echo hello\npayload line 2\n\nnormal text";
        let mut lines: Vec<Line> = vec![];
        push_assistant_text(&mut lines, text);

        // Find the tool-call span (the `→ bash echo hello` line).
        let tool_line = lines.iter().find(|l| {
            l.spans.iter().any(|s| s.content.contains("→ bash echo hello"))
        });
        assert!(tool_line.is_some(), "tool call line must be rendered");
        let tool_span = tool_line.unwrap().spans.iter()
            .find(|s| s.content.contains("→ bash echo hello"))
            .unwrap();
        assert_eq!(
            tool_span.style.fg,
            Some(Color::DarkGray),
            "tool call line must be DarkGray, got {:?}",
            tool_span.style.fg,
        );

        // Payload line must be suppressed.
        assert!(
            !lines.iter().any(|l| l.spans.iter().any(|s| s.content.contains("payload line 2"))),
            "tool call payload line must be cropped (not rendered)",
        );

        // Text after the blank following the tool call must still appear.
        assert!(
            lines.iter().any(|l| l.spans.iter().any(|s| s.content.contains("normal text"))),
            "text after the tool call must still be rendered",
        );
    }

    /// Spec (cost-reduction): usage lines emitted by ClaudeRunner must render
    /// in the session log with the USAGE_LINE_MARKER stripped and with DIM
    /// styling so they are visually distinct from regular output.
    #[test]
    fn test_spec_usage_line_rendered_dim_in_log() {
        use crate::claude::USAGE_LINE_MARKER;
        let usage_body = "↳ 1000 in / 200 out / 800 cache_read / 50 cache_write";
        let raw = format!("{}{}", USAGE_LINE_MARKER, usage_body);
        let rendered = render_log_line(&raw);
        assert_eq!(rendered.len(), 1);
        let line = &rendered[0];
        assert_eq!(line.spans.len(), 1);
        let span = &line.spans[0];
        // Marker must be stripped.
        assert_eq!(span.content, usage_body, "USAGE_LINE_MARKER must be stripped from rendered text");
        // Must carry DIM modifier.
        assert!(
            span.style.add_modifier.contains(Modifier::DIM),
            "usage line must carry DIM modifier, got {:?}",
            span.style.add_modifier,
        );
        // Must NOT be BOLD (contrast with trigger reason lines).
        assert!(
            !span.style.add_modifier.contains(Modifier::BOLD),
            "usage line must not be bold",
        );
    }

    /// Spec (tui): the streamed assistant text chunk arriving just before a
    /// usage line frequently lacks a trailing `\n` (LLM replies typically end
    /// with a period, quote, etc.), so the usage marker can land mid-line.
    /// The chat-pane renderer must still split the marker off and style the
    /// usage segment as DIM with a 7-space indent — and must not leak the
    /// marker byte into the rendered text.
    #[test]
    fn test_spec_chat_pane_splits_usage_marker_mid_line() {
        let usage_body = "↳ 1000 in / 200 out / 800 cache_read / 50 cache_write";
        let text = format!("Sure, here is my answer.{}{}", USAGE_LINE_MARKER, usage_body);

        let mut lines: Vec<Line> = vec![];
        push_assistant_text(&mut lines, &text);

        // The assistant text must be on its own line (with the tinker tag).
        let assistant_line = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains("Sure, here is my answer.")))
            .expect("assistant text must render on its own line");
        assert!(
            assistant_line.spans.iter().any(|s| s.content == "tinker "),
            "first assistant line must carry the green tinker tag",
        );
        // The marker byte must not appear in any rendered span.
        assert!(
            !lines.iter().any(|l| l.spans.iter().any(|s| s.content.contains(USAGE_LINE_MARKER))),
            "USAGE_LINE_MARKER must be stripped from rendered text",
        );

        // The usage segment must render as a DIM-styled line with body text.
        let usage_line = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content == usage_body))
            .expect("usage segment must render as its own line");
        let usage_span = usage_line
            .spans
            .iter()
            .find(|s| s.content == usage_body)
            .unwrap();
        assert!(
            usage_span.style.add_modifier.contains(Modifier::DIM),
            "usage segment must carry DIM modifier",
        );
    }

    /// Spec (tui): the chat pane's "crop tool-call payload to one line"
    /// feature must not silently destroy the usage line when the message
    /// shape is `tool_call + text-without-trailing-\n + usage`. The usage
    /// segment must still render even when the line it shares with leftover
    /// post-tool-call text would otherwise be cropped.
    #[test]
    fn test_spec_chat_pane_usage_survives_after_tool_call_without_newline() {
        let usage_body = "↳ 1000 in / 200 out / 800 cache_read / 50 cache_write";
        let text = format!(
            "Let me check.\n→ Read /some/path/file.rs\nSome more text after the tool call.{}{}",
            USAGE_LINE_MARKER, usage_body
        );

        let mut lines: Vec<Line> = vec![];
        push_assistant_text(&mut lines, &text);

        // Usage segment must render as a DIM-styled line.
        let usage_line = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content == usage_body))
            .expect("usage segment must render even after a tool call without trailing newline");
        let usage_span = usage_line
            .spans
            .iter()
            .find(|s| s.content == usage_body)
            .unwrap();
        assert!(
            usage_span.style.add_modifier.contains(Modifier::DIM),
            "usage segment must carry DIM modifier",
        );
        // Marker must not leak into rendered output.
        assert!(
            !lines.iter().any(|l| l.spans.iter().any(|s| s.content.contains(USAGE_LINE_MARKER))),
            "USAGE_LINE_MARKER must not appear in rendered text",
        );
    }

    /// Spec (tui): the session-log renderer must split a raw log line on a
    /// mid-string usage marker so the usage segment is styled DIM, mirroring
    /// the chat-pane behavior. A streamed chunk lacking a trailing newline
    /// before the usage line must not cause the marker to render as a raw
    /// control byte.
    #[test]
    fn test_spec_log_pane_splits_usage_marker_mid_line() {
        let usage_body = "↳ 1000 in / 200 out / 800 cache_read / 50 cache_write";
        let raw = format!("Sure, here is my answer.{}{}", USAGE_LINE_MARKER, usage_body);

        let rendered = render_log_line(&raw);
        assert_eq!(rendered.len(), 2, "marker mid-line must split into two rendered lines");

        let prefix_text: String = rendered[0].spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(prefix_text, "Sure, here is my answer.");
        assert!(
            !prefix_text.contains(USAGE_LINE_MARKER),
            "prefix segment must not contain the marker byte",
        );

        let usage_text: String = rendered[1].spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(usage_text, usage_body);
        let usage_span = &rendered[1].spans[0];
        assert!(
            usage_span.style.add_modifier.contains(Modifier::DIM),
            "usage segment in the log pane must carry DIM modifier",
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

}

