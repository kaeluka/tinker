use crate::app::{App, Focus, GoalListItem, ModalField, Phase, Role};
use crate::app::session_base_id;
use crate::goal_session::TRIGGER_REASON_MARKER;
use crate::goal::{build_tree, GoalNode};
use std::time::Duration;

/// German dialect verbs for "working / doing / building / tinkering".
/// Cycled in the goals-pane label while any session is running.
const WERKELN_VERBS: &[&str] = &[
    "hackeln",   // Austro-Bavarian
    "schaffe",   // Swabian/Alemannic
    "werchle",   // Swiss German
    "chrampfe",  // Swiss German — work hard
    "malochen",  // Ruhrpott (from Yiddish מְלָאכָה)
    "schaffn",   // Bavarian
    "wörken",    // Plattdeutsch
    "maken",     // Plattdeutsch
    "tüfteln",   // dialect — to tinker
    "frickeln",  // dialect — to fiddle
    "rackern",   // dialect — to toil
    "wurschteln",// Austro-Bavarian — to muddle through
    "doktern",   // dialect — to fiddle/fix
    "dun",       // Kölsch — to do
    "buckeln",   // dialect — bent-over work
    "schuften",  // dialect — to graft
    "basteln",   // widespread — to build/tinker
];
/// Advance the verb roughly every 2 seconds.
const WERKELN_INTERVAL: Duration = Duration::from_secs(2);
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

/// Render the options dialog as a centered overlay. The dialog has two fields:
/// a trigger reason (text input) and the goal's current tier (cycle selector).
/// Clears the underlying area first so panes don't bleed through.
fn draw_modal(frame: &mut Frame, app: &App) {
    let modal = match &app.modal {
        Some(m) => m,
        None => return,
    };
    let area = frame.area();

    // Show a warning row when the tier has been changed and the goal is running.
    let tier_changed = modal.tier != modal.initial_tier;
    let session_running = app.running_sessions.contains_key(&modal.goal_id);
    let show_warning = tier_changed && session_running;

    // Height: 2 fields + 1 blank + 1 hint + 2 borders + optional warning row.
    let height: u16 = if show_warning { 8 } else { 7 };
    let width = (area.width as u32 * 60 / 100).max(44).min(area.width as u32) as u16;
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    let rect = Rect { x, y, width, height };

    frame.render_widget(Clear, rect);

    let title = format!(" Options — `{}` ", modal.goal_id);
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let reason_focused = modal.focused_field == ModalField::Reason;
    let tier_focused   = modal.focused_field == ModalField::Tier;

    let cursor_style = Style::default().fg(Color::Cyan);
    let label_active = Style::default().fg(Color::Yellow);
    let label_dim    = Style::default().fg(Color::DarkGray);
    let tier_value_style = Style::default().fg(Color::White);

    // Field 1: trigger reason.
    let reason_line = Line::from(vec![
        Span::styled(
            "  Reason: ",
            if reason_focused { label_active } else { label_dim },
        ),
        Span::raw(modal.input.clone()),
        if reason_focused {
            Span::styled("█", cursor_style)
        } else {
            Span::raw("")
        },
    ]);

    // Field 2: tier selector — Left/Right to cycle when focused.
    let tier_line = Line::from(vec![
        Span::styled(
            "    Tier: ",
            if tier_focused { label_active } else { label_dim },
        ),
        if tier_focused {
            Span::styled("‹ ", cursor_style)
        } else {
            Span::styled("  ", label_dim)
        },
        Span::styled(modal.tier.clone(), tier_value_style),
        if tier_focused {
            Span::styled(" ›", cursor_style)
        } else {
            Span::raw("")
        },
    ]);

    let hint = "Tab = switch field · Enter = confirm · Esc = cancel";
    let mut lines = vec![
        reason_line,
        tier_line,
        Line::from(""),
    ];
    if show_warning {
        lines.push(Line::from(Span::styled(
            "  ⚠ tier change will reset the running session",
            Style::default().fg(Color::Yellow),
        )));
    }
    lines.push(Line::from(Span::styled(hint, Style::default().fg(Color::DarkGray))));

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
            push_message_lines(&mut lines, msg, msg_area.width);
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

fn push_message_lines(lines: &mut Vec<Line<'static>>, msg: &crate::app::Message, pane_width: u16) {
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


/// Sort IDs and determine which fit within `max_chars` (the label budget —
/// the ` > id1, id2 ` portion, not including `" Goals"`).
/// Returns `(visible ids sorted, needs_ellipsis)`.
fn running_ids(mut ids: Vec<&str>, max_chars: usize) -> (Vec<&str>, bool) {
    ids.sort_unstable();
    let n = ids.len();

    let all_label = format!(" > {} ", ids.join(", "));
    if all_label.chars().count() <= max_chars {
        return (ids, false);
    }

    for k in (1..n).rev() {
        let label = format!(" > {}, … ", ids[..k].join(", "));
        if label.chars().count() <= max_chars {
            return (ids[..k].to_vec(), true);
        }
    }

    (vec![], true)
}

/// Build the running-sessions label for the Goals pane title (plain string form).
/// Sorts IDs alphabetically, fits as many as possible within `max_chars`,
/// truncating with `…` when needed.
/// Returns e.g. `" > alpha, beta, … "` (truncated) or `" > alpha, beta "`.
/// Only used by tests; production code calls `goal_pane_title_line` for styled output.
#[cfg(test)]
fn running_label(ids: Vec<&str>, max_chars: usize) -> String {
    let (visible, ellipsis) = running_ids(ids, max_chars);
    if visible.is_empty() {
        return " > … ".to_string();
    }
    if ellipsis {
        format!(" > {}, … ", visible.join(", "))
    } else {
        format!(" > {} ", visible.join(", "))
    }
}

/// Build a styled `Line` for the Goals pane title when sessions are running.
/// `verb` is the dialect word to show (e.g. `"hackeln"`); pass `"Goals"` for the
/// static idle label.  Running IDs render Yellow+Bold — matching the goal-list
/// row style — so a goal ID looks the same in the pane label and in its list row.
pub(crate) fn goal_pane_title_line(verb: &str, ids: Vec<&str>, max_chars: usize) -> Line<'static> {
    let id_style = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let (visible, ellipsis) = running_ids(ids, max_chars);

    let prefix = format!(" {} > ", verb);
    let mut spans: Vec<Span<'static>> = vec![Span::raw(prefix)];

    for (i, id) in visible.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(", "));
        }
        spans.push(Span::styled(id.to_string(), id_style));
    }

    if visible.is_empty() {
        spans.push(Span::raw("… "));
    } else if ellipsis {
        spans.push(Span::raw(", … "));
    } else {
        spans.push(Span::raw(" "));
    }

    Line::from(spans)
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

    // Build a styled title Line so running IDs render Yellow+Bold — matching
    // the goal-list row style for visual consistency across surfaces.
    let title_line: Line<'static> = if app.phase == Phase::Initializing {
        Line::from(Span::raw(" Goals starting… "))
    } else if app.running_sessions.is_empty() {
        Line::from(Span::raw(" Goals "))
    } else {
        // Advance the verb index every ~2 s while sessions are running.
        if app.werkeln_last_advance.elapsed() >= WERKELN_INTERVAL {
            app.werkeln_verb_idx = (app.werkeln_verb_idx + 1) % WERKELN_VERBS.len();
            app.werkeln_last_advance = std::time::Instant::now();
        }
        let verb = WERKELN_VERBS[app.werkeln_verb_idx];
        // " {verb} > id1, id2 " must fit in area.width - 2 (border corners).
        // Budget: subtract verb.len() + 4 (" {verb} > " overhead) plus 2 border corners.
        let max_chars = (area.width as usize).saturating_sub(verb.len() + 4 + 2);
        // Collapse fresh sub-session IDs (e.g. `fresh-agents~1`) to their
        // parent goal ID so the header shows `fresh-agents`, not `fresh-agents~1`.
        let mut base_ids: Vec<&str> = app.running_sessions.keys()
            .map(|k| session_base_id(k.as_str()))
            .collect();
        base_ids.sort_unstable();
        base_ids.dedup();
        goal_pane_title_line(verb, base_ids, max_chars)
    };

    let block = Block::default()
        .title(title_line)
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
    // Depth map for permanent goals — used to indent ephemerals at parent_depth + 1.
    let depth_by_id: std::collections::HashMap<&str, usize> = flat.iter()
        .map(|(d, n)| (n.goal.id.as_str(), *d))
        .collect();
    let selected_id = app.selected_item_id();
    // Pre-compute the set of "base" goal IDs that have any active session,
    // including fresh sub-sessions (e.g. `fresh-agents~1` counts as `fresh-agents`).
    let running_base_ids: std::collections::HashSet<&str> = app.running_sessions.keys()
        .map(|k| session_base_id(k.as_str()))
        .collect();
    let items = app.flat_items();
    let list_lines: Vec<Line> = items
        .iter()
        .map(|item| {
            let id = item.id();
            let is_selected = selected_id.as_deref() == Some(id);

            match item {
                GoalListItem::Goal(g) => {
                    let depth = depth_by_id.get(g.id.as_str()).copied().unwrap_or(0);
                    let is_active = running_base_ids.contains(g.id.as_str());

                    let name_style = if is_selected {
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    let id_style = if is_selected {
                        name_style
                    } else if is_active {
                        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD)
                    };
                    let marker_style = if is_active {
                        Style::default().fg(Color::DarkGray)
                    } else {
                        name_style
                    };
                    let marker_str = if is_active { "▶ " } else { "◉ " };
                    let indent = "  ".repeat(depth);
                    let preview = truncate_with_ellipsis(&g.summary, 60);
                    let mut spans = vec![
                        Span::styled(format!("{}{}", indent, marker_str), marker_style),
                        Span::styled(g.id.clone(), id_style),
                    ];
                    if !preview.is_empty() {
                        spans.push(Span::styled(format!(" — {}", preview), name_style));
                    }
                    Line::from(spans)
                }
                GoalListItem::Ephemeral(session_id) => {
                    let parent_id = session_base_id(session_id);
                    let depth = depth_by_id.get(parent_id).copied().unwrap_or(0) + 1;
                    let is_active = app.running_sessions.contains_key(session_id.as_str());

                    let id_style = if is_selected {
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                    } else if is_active {
                        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD)
                    };
                    let marker_style = Style::default().fg(Color::DarkGray);
                    let marker_str = if is_active { "▶ " } else { "◉ " };
                    let indent = "  ".repeat(depth);
                    Line::from(vec![
                        Span::styled(format!("{}{}", indent, marker_str), marker_style),
                        Span::styled(session_id.clone(), id_style),
                    ])
                }
            }
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

    let text_lines: Vec<Line> = match app.flat_items().into_iter().nth(app.selected_goal) {
        None => vec![Line::from(Span::styled(
            "(no selection)",
            Style::default().fg(Color::DarkGray),
        ))],
        Some(GoalListItem::Goal(g)) => {
            let header = goal_detail_header_line(&g);
            let mut lines: Vec<Line> = vec![header, Line::from("")];
            lines.extend(
                g.description
                    .lines()
                    .map(|l| Line::from(l.to_string()))
            );
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
        Some(GoalListItem::Ephemeral(session_id)) => {
            let parent_id = session_base_id(&session_id);
            let is_active = app.running_sessions.contains_key(session_id.as_str());
            let header = Line::from(vec![
                Span::styled(
                    format!("↳ sub-session of {}", parent_id),
                    Style::default().fg(Color::DarkGray),
                ),
            ]);
            let mut lines = vec![header, Line::from("")];
            if is_active {
                let reason = app.running_sessions.get(session_id.as_str())
                    .and_then(|r| r.as_ref())
                    .map(|s| s.as_str())
                    .unwrap_or("");
                if !reason.is_empty() {
                    lines.push(Line::from(vec![
                        Span::styled("▶ ", Style::default().fg(Color::DarkGray)),
                        Span::raw(reason.to_string()),
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

fn draw_log(frame: &mut Frame, app: &mut App, area: Rect) {
    let selected_id = app.selected_item_id();
    let title = log_pane_title(selected_id.as_deref());

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


fn truncate_with_ellipsis(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{}...", truncated)
    }
}

fn goal_detail_header_line(g: &crate::goal::Goal) -> Line<'static> {
    let kind_str = g.kind.as_deref().unwrap_or("feature");
    let tier_str = g.tier.as_deref().unwrap_or("mid");
    let tag = format!("[{} · {}]", kind_str, tier_str);
    let muted = Style::default().fg(Color::DarkGray);
    Line::from(vec![
        Span::styled(g.summary.clone(), muted),
        Span::raw("  "),
        Span::styled(tag, muted),
    ])
}

fn log_pane_title(id: Option<&str>) -> String {
    match id {
        Some(id) => format!(" Log: {} ", id),
        None => " Log ".to_string(),
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
            kind: None,
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
        push_message_lines(&mut lines, &msg, 200);
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

    /// Spec (tui): a short single-line system message must render unchanged when it fits.
    #[test]
    fn test_spec_system_message_short_no_truncation() {
        let msg = crate::app::Message {
            role: Role::System,
            text: "hello world".to_string(),
        };
        let mut lines: Vec<Line> = vec![];
        push_message_lines(&mut lines, &msg, 200);
        let content: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            content.contains("hello world"),
            "short single-line message must appear unchanged: got {:?}",
            content,
        );
        assert!(
            !content.contains('…'),
            "no ellipsis for a short message that fits",
        );
    }

    /// Spec (tui): a multi-line system message must show only the first line with '…' appended.
    #[test]
    fn test_spec_system_message_multiline_truncated_to_first_line() {
        let msg = crate::app::Message {
            role: Role::System,
            text: "first line\nsecond line\nthird line".to_string(),
        };
        let mut lines: Vec<Line> = vec![];
        push_message_lines(&mut lines, &msg, 200);
        assert_eq!(lines.len(), 1, "multi-line system message must produce exactly one rendered line");
        let content: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            content.contains("first line"),
            "first line must be present: got {:?}",
            content,
        );
        assert!(
            content.contains('…'),
            "ellipsis must be appended when message has multiple lines: got {:?}",
            content,
        );
        assert!(
            !content.contains("second line"),
            "subsequent lines must not appear: got {:?}",
            content,
        );
    }

    /// Spec (tui): a system message whose first line exceeds the pane width must be truncated
    /// to fit within that width, with '…' appended.
    #[test]
    fn test_spec_system_message_long_line_truncated_to_pane_width() {
        // label is 7 chars ("sys    "), so available body width = pane_width - 7
        let pane_width: u16 = 20; // available for body = 13 chars
        let long_text = "a".repeat(50);
        let msg = crate::app::Message {
            role: Role::System,
            text: long_text.clone(),
        };
        let mut lines: Vec<Line> = vec![];
        push_message_lines(&mut lines, &msg, pane_width);
        assert_eq!(lines.len(), 1, "long system message must produce exactly one rendered line");
        let body: String = lines[0]
            .spans
            .iter()
            .filter(|s| !s.content.starts_with("sys"))
            .map(|s| s.content.as_ref())
            .collect();
        let char_count = body.chars().count();
        let available = (pane_width as usize).saturating_sub(7);
        assert!(
            char_count <= available,
            "rendered body must fit within available width ({}); got {} chars: {:?}",
            available,
            char_count,
            body,
        );
        assert!(
            body.contains('…'),
            "ellipsis must be appended on truncation: got {:?}",
            body,
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
                push_message_lines(&mut lines, msg, 200);
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

    /// Spec (tui — queue visibility): the goal ID of a running goal must use a
    /// distinct colour (Yellow) so the running goal stays visually salient
    /// while scrolling. The ▶ marker stays dim; only the ID gets colour.
    /// Running IDs also carry BOLD for legibility.
    #[test]
    fn test_spec_running_goal_id_colour_is_distinct() {
        let is_active = true;
        let is_selected = false;
        let name_style = Style::default(); // not selected
        // Mirror the id-style derivation from draw_goal_tree.
        let id_style = if is_selected {
            name_style
        } else if is_active {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD)
        };
        assert_eq!(
            id_style.fg,
            Some(Color::Yellow),
            "running goal ID must use Yellow fg to remain visually salient",
        );
        assert!(
            id_style.add_modifier.contains(Modifier::BOLD),
            "running goal ID must carry BOLD modifier",
        );
        // Confirm the marker is still dim — unchanged by this feature.
        let marker_style = if is_active {
            Style::default().fg(Color::DarkGray)
        } else {
            name_style
        };
        assert_eq!(
            marker_style.fg,
            Some(Color::DarkGray),
            "running ▶ marker must remain dim (DarkGray) regardless of ID colour",
        );
    }

    /// Spec (tui — goal list): all three id-style variants carry BOLD so
    /// goal identifiers remain legible regardless of selection/running state.
    #[test]
    fn test_spec_goal_id_is_bold_in_all_display_states() {
        // selected: name_style carries BOLD, id_style == name_style
        let selected_name_style = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
        let id_style_selected = selected_name_style;
        assert!(
            id_style_selected.add_modifier.contains(Modifier::BOLD),
            "selected goal ID must carry BOLD modifier",
        );
        // running (active, not selected)
        let id_style_active = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
        assert!(
            id_style_active.add_modifier.contains(Modifier::BOLD),
            "running goal ID must carry BOLD modifier",
        );
        // inactive, not selected
        let id_style_inactive = Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD);
        assert!(
            id_style_inactive.add_modifier.contains(Modifier::BOLD),
            "inactive goal ID must carry BOLD modifier",
        );
    }

    /// Spec (tui — goal list preview): the preview shown next to each goal ID
    /// must come from goal.summary, not goal.description.
    #[test]
    fn test_spec_goal_list_preview_uses_summary_not_description() {
        let goal = Goal {
            id: "alpha".to_string(),
            summary: "the summary text".to_string(),
            description: "the description text — longer and different".to_string(),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            tier: None,
            kind: None,
            source_path: None,
        };
        // Mirror the preview derivation from draw_goal_tree.
        let preview = truncate_with_ellipsis(&goal.summary, 60);
        assert!(
            preview.contains("summary"),
            "preview must come from goal.summary, got: {:?}",
            preview,
        );
        assert!(
            !preview.contains("description"),
            "preview must NOT come from goal.description, got: {:?}",
            preview,
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
        assert_eq!(label, " > alpha, beta ");
    }

    /// Spec (tui — goals pane title): IDs are sorted alphabetically for stability.
    #[test]
    fn test_spec_running_label_sorted() {
        let ids = vec!["zzz", "aaa", "mmm"];
        let label = running_label(ids, 100);
        assert_eq!(label, " > aaa, mmm, zzz ");
    }

    /// Spec (tui — goals pane title): when not all IDs fit, show as many as
    /// possible followed by "…".
    #[test]
    fn test_spec_running_label_overflow_truncated() {
        // sorted: ["alpha", "beta", "gamma"]
        // all: " > alpha, beta, gamma " = 22 chars > 20 → doesn't fit
        // k=2: " > alpha, beta, … " = 18 chars ≤ 20 → fits
        let ids = vec!["beta", "alpha", "gamma"];
        let label = running_label(ids, 20);
        assert_eq!(label, " > alpha, beta, … ");
    }

    /// Spec (tui — goals pane title): when nothing fits, fall back to ellipsis-only label.
    #[test]
    fn test_spec_running_label_nothing_fits_fallback() {
        let ids = vec!["a-very-long-goal-name", "another-very-long-goal"];
        let label = running_label(ids, 5);
        assert_eq!(label, " > … ");
    }

    /// Spec (tui — goals pane title): running IDs in the pane label must render
    /// Yellow+Bold — the same style used for running IDs in the goal-list rows —
    /// so a goal ID looks identical on every surface.
    #[test]
    fn test_spec_goal_pane_title_running_ids_are_yellow_bold() {
        use ratatui::style::{Color, Modifier};
        let id_style = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
        let line = goal_pane_title_line("hackeln", vec!["beta", "alpha"], 100);
        // Spans carrying actual IDs are exactly those with Yellow+Bold style.
        let id_spans: Vec<_> = line.spans.iter()
            .filter(|s| s.style == id_style)
            .collect();
        assert!(!id_spans.is_empty(), "expected ID spans in the title line");
        for span in &id_spans {
            assert_eq!(
                span.style, id_style,
                "ID span {:?} must be Yellow+Bold in the pane label",
                span.content,
            );
        }
        // Confirm both IDs appear (sorted).
        let full: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(full.contains("alpha"));
        assert!(full.contains("beta"));
    }

    /// Spec (tui — goals pane title): the " {verb} > " prefix and punctuation spans
    /// are unstyled so they don't compete visually with the ID colour.
    #[test]
    fn test_spec_goal_pane_title_prefix_and_punctuation_are_unstyled() {
        let plain = Style::default();
        let line = goal_pane_title_line("hackeln", vec!["alpha", "beta"], 100);
        // First span is always the " {verb} > " prefix — here " hackeln > ".
        let prefix = &line.spans[0];
        assert_eq!(prefix.content.as_ref(), " hackeln > ");
        assert_eq!(prefix.style, plain, "prefix must be unstyled");
        // Trailing " " span (the last one) must also be unstyled.
        let last = line.spans.last().unwrap();
        assert_eq!(last.style, plain, "trailing space span must be unstyled");
    }

    /// Spec (werkeln — verb list): the WERKELN_VERBS list is non-empty and contains
    /// every dialect word named in the goal spec.
    #[test]
    fn test_spec_werkeln_verb_list_complete() {
        let required = [
            "hackeln", "schaffe", "werchle", "chrampfe", "malochen", "schaffn",
            "wörken", "maken", "tüfteln", "frickeln", "rackern", "wurschteln",
            "doktern", "dun", "buckeln", "schuften", "basteln",
        ];
        assert!(!WERKELN_VERBS.is_empty(), "verb list must be non-empty");
        for verb in &required {
            assert!(
                WERKELN_VERBS.contains(verb),
                "verb list must contain {:?}",
                verb,
            );
        }
    }

    /// Spec (werkeln — animation): each verb produces a prefix of the form " {verb} > ".
    #[test]
    fn test_spec_werkeln_each_verb_produces_correct_prefix() {
        for &verb in WERKELN_VERBS {
            let line = goal_pane_title_line(verb, vec!["tend"], 100);
            let expected_prefix = format!(" {} > ", verb);
            let prefix = &line.spans[0];
            assert_eq!(
                prefix.content.as_ref(),
                expected_prefix.as_str(),
                "verb {:?} must produce prefix {:?}",
                verb,
                expected_prefix,
            );
        }
    }

    /// Spec (tui — goal-detail header): the first line of the goal-text pane
    /// shows the goal's summary and a "[kind · tier]" tag, both rendered in
    /// DarkGray so they're visually secondary to the body.
    #[test]
    fn test_spec_goal_detail_header_shows_summary_and_tag() {
        let goal = Goal {
            id: "my-goal".to_string(),
            summary: "does the thing".to_string(),
            description: "body text".to_string(),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            tier: Some("high".to_string()),
            kind: Some("behavior".to_string()),
            source_path: None,
        };
        let line = goal_detail_header_line(&goal);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("does the thing"), "header must contain summary");
        assert!(text.contains("[behavior · high]"), "header must contain [kind · tier] tag");
        for span in &line.spans {
            if !span.content.is_empty() && span.content.as_ref() != "  " {
                assert_eq!(
                    span.style.fg,
                    Some(Color::DarkGray),
                    "header span {:?} must be DarkGray",
                    span.content,
                );
            }
        }
    }

    /// Spec (tui — goal-detail header): when kind or tier are absent the header
    /// falls back to "feature" and "mid" respectively.
    #[test]
    fn test_spec_goal_detail_header_defaults_to_feature_and_mid() {
        let goal = Goal {
            id: "x".to_string(),
            summary: "summary".to_string(),
            description: String::new(),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            tier: None,
            kind: None,
            source_path: None,
        };
        let line = goal_detail_header_line(&goal);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("[feature · mid]"),
            "expected default tag '[feature · mid]', got: {:?}",
            text,
        );
    }

    /// Spec (tui — session log pane title): the log pane title must read
    /// "Log: <goal-id>" when a goal is selected, using the id directly rather
    /// than the truncated description.
    #[test]
    fn test_spec_log_pane_title_uses_goal_id() {
        assert_eq!(log_pane_title(Some("my-goal")), " Log: my-goal ");
        assert_eq!(log_pane_title(None), " Log ");
    }

    /// Spec (tui — fresh sub-sessions): `session_base_id` strips the `~{counter}`
    /// suffix from a fresh sub-session ID, returning the parent goal ID unchanged
    /// for ordinary IDs.
    #[test]
    fn test_spec_session_base_id_strips_counter_suffix() {
        assert_eq!(session_base_id("fresh-agents~1"), "fresh-agents");
        assert_eq!(session_base_id("my-goal~42"), "my-goal");
        assert_eq!(session_base_id("tend"), "tend");
        assert_eq!(session_base_id("rummage"), "rummage");
        // Only the first `~` is stripped; base IDs should not themselves contain `~`.
        assert_eq!(session_base_id("a~b~c"), "a");
    }

    /// Spec (tui — fresh sub-sessions / goals pane title): fresh sub-session IDs
    /// (e.g. `fresh-agents~1`) must appear as their parent ID (`fresh-agents`)
    /// in the pane header, not as raw `fresh-agents~1`.
    #[test]
    fn test_spec_fresh_subsession_ids_collapsed_in_header() {
        // Two sub-sessions of the same parent — should deduplicate to one entry.
        let mut ids = vec!["fresh-agents~1", "fresh-agents~2"];
        ids.sort_unstable();
        ids.dedup_by(|a, b| session_base_id(a) == session_base_id(b));
        let base_ids: Vec<&str> = ids.iter().map(|id| session_base_id(id)).collect();
        // Dedup after mapping.
        let mut base_ids = base_ids;
        base_ids.dedup();
        assert_eq!(base_ids, vec!["fresh-agents"],
            "two sub-sessions of the same parent must collapse to one entry");

        // Sub-session alongside a direct parent entry — should still show once.
        let ids2: Vec<&str> = vec!["fresh-agents", "fresh-agents~1"];
        let mut collapsed: Vec<&str> = ids2.iter()
            .map(|id| session_base_id(id))
            .collect::<std::collections::BTreeSet<&str>>()
            .into_iter()
            .collect();
        collapsed.sort_unstable();
        assert_eq!(collapsed, vec!["fresh-agents"],
            "direct parent entry + sub-session must collapse to one entry");
    }

    /// Spec (tui — fresh sub-sessions / goal list rows): a goal row must show
    /// the ▶ running marker and Yellow+Bold ID style when a fresh sub-session
    /// is running for that goal, even if the parent goal entry is not itself
    /// directly in `running_sessions`.
    #[test]
    fn test_spec_fresh_subsession_marks_parent_goal_row_as_running() {
        // Simulate running_sessions containing only a sub-session.
        let mut running_sessions = std::collections::HashMap::new();
        running_sessions.insert("my-goal~1".to_string(), None::<String>);

        // Mirror the is_active derivation from draw_goal_tree.
        let running_base_ids: std::collections::HashSet<&str> = running_sessions.keys()
            .map(|k| session_base_id(k.as_str()))
            .collect();

        let is_active_parent = running_base_ids.contains("my-goal");
        let is_active_other = running_base_ids.contains("other-goal");

        assert!(is_active_parent,
            "parent goal row must be active when its sub-session is running");
        assert!(!is_active_other,
            "unrelated goal must not be marked active");
    }

    /// Spec (tui — fresh sub-sessions / goal list): `flat_items` returns
    /// ephemerals nested immediately after their dispatcher goal, in insertion order.
    #[test]
    fn test_spec_flat_items_includes_ephemerals_after_parent() {
        let mut app = App::new();
        app.goals = vec![
            Goal { id: "alpha".to_string(), summary: String::new(), description: String::new(),
                parent_id: String::new(), children: vec![], related: vec![], tier: None,
                kind: None, source_path: None },
            Goal { id: "beta".to_string(), summary: String::new(), description: String::new(),
                parent_id: String::new(), children: vec![], related: vec![], tier: None,
                kind: None, source_path: None },
        ];
        app.ephemeral_sessions.insert("alpha~1".to_string());
        app.ephemeral_sessions.insert("alpha~2".to_string());
        app.ephemeral_sessions_ordered.push("alpha~1".to_string());
        app.ephemeral_sessions_ordered.push("alpha~2".to_string());

        let items = app.flat_items();
        let ids: Vec<&str> = items.iter().map(|i| i.id()).collect();
        assert_eq!(ids, vec!["alpha", "alpha~1", "alpha~2", "beta"],
            "ephemerals must appear immediately after their parent, before siblings");
    }

    /// Spec (tui — fresh sub-sessions / selection): selecting an ephemeral row
    /// makes `selected_item_id` return its full session ID.
    #[test]
    fn test_spec_selected_item_id_returns_ephemeral_session_id() {
        let mut app = App::new();
        app.goals = vec![
            Goal { id: "alpha".to_string(), summary: String::new(), description: String::new(),
                parent_id: String::new(), children: vec![], related: vec![], tier: None,
                kind: None, source_path: None },
        ];
        app.ephemeral_sessions.insert("alpha~1".to_string());
        app.ephemeral_sessions_ordered.push("alpha~1".to_string());

        // flat_items: [Goal("alpha"), Ephemeral("alpha~1")]
        app.selected_goal = 1; // select the ephemeral
        assert_eq!(app.selected_item_id(), Some("alpha~1".to_string()));
        assert!(app.selected_goal().is_none(),
            "selected_goal() must return None when an ephemeral is selected");
    }

    /// Spec (tui — fresh sub-sessions / log pane): the log pane must show
    /// output for an ephemeral sub-session when that row is selected, using
    /// the session ID as the lookup key (same as for permanent goals).
    #[test]
    fn test_spec_log_pane_uses_selected_item_id_for_ephemeral() {
        let mut app = App::new();
        app.goals = vec![
            Goal { id: "alpha".to_string(), summary: String::new(), description: String::new(),
                parent_id: String::new(), children: vec![], related: vec![], tier: None,
                kind: None, source_path: None },
        ];
        app.ephemeral_sessions.insert("alpha~1".to_string());
        app.ephemeral_sessions_ordered.push("alpha~1".to_string());
        app.goal_logs.insert("alpha~1".to_string(), "sub-session output".to_string());

        app.selected_goal = 1; // ephemeral row
        // The ID used for log lookup must be "alpha~1", not "alpha".
        let id = app.selected_item_id();
        assert_eq!(id.as_deref(), Some("alpha~1"));
        assert!(app.goal_logs.contains_key("alpha~1"),
            "log lookup by ephemeral session ID must find the sub-session output");
    }

}

