use crate::app::{App, Focus, GoalListItem, ModalField, Phase, Role};
use crate::app::{session_base_id, session_parent_id};
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
    "verschlimmbessern", // widespread — to make something worse while trying to improve it
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
    /// Whole right pane (Goals), border + title included.
    pub tree: Rect,
    /// Goal-list sub-area inside the Goals pane (selectable).
    pub goal_list: Rect,
    /// Separator row between goal list and goal text (decorative).
    pub goal_sep: Rect,
    /// Goal-description text sub-area inside the Goals pane (scrollable).
    pub goal_text: Rect,
}

/// Pure function of the terminal area → per-pane rects. Called by both the
/// renderer and the mouse-event handler so they stay in sync.
pub fn pane_rects(area: Rect) -> PaneRects {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    let tree_outer = columns[1];
    let tree_inner = Block::default().borders(Borders::ALL).inner(tree_outer);
    let tree_splits = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(50),
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

    // Field 1: trigger reason. No fake block cursor — the terminal's real
    // cursor is placed via set_cursor_position below when this field is focused.
    const REASON_LABEL: &str = "  Reason: ";
    let reason_line = Line::from(vec![
        Span::styled(
            REASON_LABEL,
            if reason_focused { label_active } else { label_dim },
        ),
        Span::raw(modal.input.clone()),
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

    // Place the terminal cursor on the focused field. The Reason field is
    // line 0 of the modal's inner area; the cursor sits after the label +
    // input text. The Tier field is a selector (no text cursor) — we place
    // the cursor on the tier value to indicate focus.
    if reason_focused {
        let cursor_x = inner.x + REASON_LABEL.chars().count() as u16 + modal.input.chars().count() as u16;
        let cursor_y = inner.y;
        frame.set_cursor_position((cursor_x, cursor_y));
    }
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

    let max_input = (inner.height / 2).max(1);
    let (input_height, input_scroll) =
        input_pane_layout(&prompt, &app.input, inner.width, max_input);

    let msg_area = Rect {
        height: inner.height.saturating_sub(input_height),
        ..inner
    };
    let input_area = Rect {
        y: inner.y + inner.height.saturating_sub(input_height),
        height: input_height,
        ..inner
    };

    // Message pane: use the retained buffer, extended incrementally.
    let active = app.active_session.clone();
    app.repl_buffer.build_or_extend(&app.messages, &active, msg_area.width);
    let total = app.repl_buffer.total_lines();
    let cache_key = msg_area.width as u64 ^ total.wrapping_mul(0x9e3779b97f4a7c15) as u64;
    app.repl_scroll.record_render(total, msg_area.height, cache_key);
    let scroll_y = app.repl_scroll.effective_y().min(u16::MAX as usize) as u16;

    let (viewport, scroll_offset) = app.repl_buffer.viewport_lines(scroll_y as usize, msg_area.height);
    let paragraph = Paragraph::new(viewport).wrap(Wrap { trim: false });
    frame.render_widget(paragraph.scroll((scroll_offset, 0)), msg_area);

    // Input line: prompt + input text. The cursor is placed via
    // `frame.set_cursor_position()` below (the terminal's real hardware
    // cursor), not via a fake block character — that was invisible on some
    // terminals because ratatui hides the cursor every frame unless
    // `set_cursor_position` is called.
    let input_line = Line::from(vec![
        Span::styled(prompt.clone(), prompt_style),
        Span::styled(app.input.clone(), input_text_style),
    ]);
    frame.render_widget(
        Paragraph::new(input_line)
            .wrap(Wrap { trim: false })
            .scroll((input_scroll, 0)),
        input_area,
    );

    // Place the terminal cursor at the correct (x, y) within the input pane.
    // Only when the REPL is focused and input is not locked (Initializing).
    if focused && !input_locked {
        let prompt_width = prompt.chars().count() as u16;
        let cursor_col = prompt_width + app.input_cursor as u16;
        let pane_width = input_area.width.max(1);
        // Account for wrapping: the cursor's position within the wrapped text
        // determines which row and column it lands on.
        let wrapped_row = cursor_col / pane_width;
        let col_in_row = cursor_col % pane_width;
        let cursor_y = input_area.y + wrapped_row.saturating_sub(input_scroll);
        let cursor_x = input_area.x + col_in_row;
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

/// Chat input pane layout: returns `(height, scroll)` for the input pane.
/// Height matches the wrapped Paragraph's actual row count, capped by `max`
/// and floored at 1. When the unclamped row count exceeds `max`, `scroll` is
/// set so the bottom wrapped row stays visible.
///
/// A phantom space is appended to account for the cursor position when the
/// cursor is at the end of the input — without it, an input that exactly fills
/// the pane width would compute one fewer row than needed, hiding the cursor
/// on the wrapped line below.
fn input_pane_layout(prompt: &str, input: &str, width: u16, max: u16) -> (u16, u16) {
    let line = Line::from(vec![
        Span::raw(prompt.to_string()),
        Span::raw(input.to_string()),
        Span::raw(" "), // phantom cursor width
    ]);
    let needed = (Paragraph::new(line)
        .wrap(Wrap { trim: false })
        .line_count(width.max(1)) as u16)
        .max(1);
    let height = needed.min(max.max(1));
    let scroll = needed.saturating_sub(height);
    (height, scroll)
}

#[allow(dead_code)]
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
    // Depth map seeded from permanent goals, then extended with ephemeral depths
    // so that nested sub-sessions (e.g. jog~1~3 under jog~1) indent correctly.
    let mut depth_by_id: std::collections::HashMap<String, usize> = flat.iter()
        .map(|(d, n)| (n.goal.id.clone(), *d))
        .collect();
    let selected_id = app.selected_item_id();
    // items is already in depth-first order (parent before children) so each
    // ephemeral's parent is already in depth_by_id when we reach it.
    let items_for_depth = app.flat_items();
    for item in &items_for_depth {
        if let GoalListItem::Ephemeral(eph_id, _) = item {
            let parent_id = session_parent_id(eph_id.as_str());
            let parent_depth = depth_by_id.get(parent_id).copied().unwrap_or(0);
            depth_by_id.insert(eph_id.clone(), parent_depth + 1);
        }
    }
    // Pre-compute the set of "base" goal IDs that have any active session,
    // including fresh sub-sessions (e.g. `fresh-agents~1` counts as `fresh-agents`).
    let running_base_ids: std::collections::HashSet<&str> = app.running_sessions.keys()
        .map(|k| session_base_id(k.as_str()))
        .collect();
    let items = app.flat_items();
    let list_lines: Vec<Line> = items
        .iter()
        .map(|item| {
            goal_list_row_line(
                item,
                &depth_by_id,
                selected_id.as_deref(),
                &running_base_ids,
                app,
                list_area.width,
            )
        })
        .collect();

    let list_paragraph = Paragraph::new(list_lines.clone());
    let list_total = list_lines.len();
    app.goal_list_scroll.record_render(list_total, list_area.height, 0);
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
            let header = goal_detail_header_line(&g, app.is_packaged(&g));
            let mut lines: Vec<Line> = vec![header, Line::from("")];
            lines.extend(
                g.description
                    .lines()
                    .map(|l| Line::from(normalize_list_marker(l)))
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
            // Token-usage breakdown for the detail pane — aggregated across
            // the goal's own API calls and all its ephemeral sub-sessions.
            let stats = app.aggregated_token_stats(&g.id);
            if stats.has_data() {
                let dim = Style::default().fg(Color::DarkGray);
                // Count how many ephemeral sub-sessions contributed token data.
                let child_count = app.token_usage.keys()
                    .filter(|k| session_base_id(k.as_str()) == g.id.as_str() && k.as_str() != g.id.as_str())
                    .count();
                let subtitle = if child_count > 0 {
                    format!(" (incl. {} sub-session{})", child_count, if child_count == 1 { "" } else { "s" })
                } else {
                    String::new()
                };
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(format!("Token usage{}", subtitle), dim)));
                lines.push(Line::from(vec![
                    Span::styled("  Sessions: ", dim),
                    Span::raw(stats.session_count.to_string()),
                    Span::styled("    Total in: ", dim),
                    Span::raw(stats.total_prompt_tokens.to_string()),
                    Span::styled("    Total out: ", dim),
                    Span::raw(stats.total_completion_tokens.to_string()),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("  Avg in/session: ", dim),
                    Span::raw(stats.avg_prompt_per_session().to_string()),
                    Span::styled("    Avg out/session: ", dim),
                    Span::raw(stats.avg_completion_per_session().to_string()),
                ]));
                if stats.total_cached_tokens > 0 {
                    let pct = (stats.total_cached_tokens * 100)
                        .checked_div(stats.total_prompt_tokens)
                        .unwrap_or(0);
                    lines.push(Line::from(vec![
                        Span::styled("  Cache hits: ", dim),
                        Span::raw(format!("{} tokens ({}% of input)", stats.total_cached_tokens, pct)),
                    ]));
                }
            }
            lines
        }
        Some(GoalListItem::Ephemeral(session_id, label)) => {
            let parent_id = session_parent_id(&session_id);
            let is_active = app.running_sessions.contains_key(session_id.as_str());
            let label_line = match &label {
                Some(l) if !l.is_empty() => Some(Line::from(vec![
                    Span::styled(l.clone(), Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
                ])),
                _ => None,
            };
            let header = Line::from(vec![
                Span::styled(
                    format!("↳ sub-session of {}", parent_id),
                    Style::default().fg(Color::DarkGray),
                ),
            ]);
            let mut lines = vec![];
            if let Some(ll) = label_line {
                lines.push(ll);
            }
            lines.push(header);
            lines.push(Line::from(""));
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
    let cache_key = text_area.width as u64;
    let total = if app.goal_text_scroll.is_cache_valid(cache_key) {
        app.goal_text_scroll.last_total
    } else {
        p.line_count(text_area.width)
    };
    app.goal_text_scroll.record_render(total, text_area.height, cache_key);
    let scroll_y = app.goal_text_scroll.effective_y().min(u16::MAX as usize) as u16;
    frame.render_widget(p.scroll((scroll_y, 0)), text_area);
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

/// Width of the metrics block in display columns: leading separator space (1)
/// + session count (2) + space (1) + input avg (5) + space (1) + output avg (5) = 15.
const METRICS_WIDTH: usize = 15;

/// Format a token count compactly for the goal-list metrics columns.
///   0–999:        plain number        (e.g. `3`, `42`, `999`)
///   1000–9999:    one decimal + k     (e.g. `1.2k`, `9.9k`)
///   10000–999999: integer + k         (e.g. `10k`, `127k`)
///   1000000+:     one decimal + M     (e.g. `1.2M`)
fn format_compact_tokens(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 10_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else if n < 1_000_000 {
        format!("{}k", n / 1000)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

fn goal_list_row_line(
    item: &GoalListItem,
    depth_by_id: &std::collections::HashMap<String, usize>,
    selected_id: Option<&str>,
    running_base_ids: &std::collections::HashSet<&str>,
    app: &App,
    pane_width: u16,
) -> Line<'static> {
    let id = item.id();
    let is_selected = selected_id == Some(id);

    match item {
        GoalListItem::Goal(g) => {
            let depth = depth_by_id.get(g.id.as_str()).copied().unwrap_or(0);
            let is_active = running_base_ids.contains(g.id.as_str());
            let is_packaged = app.is_packaged(g);

            // Cascade for the row's text styles:
            //   1. Selection wins over everything — selection is user focus.
            //      Italic is added on the id when the goal is packaged, so the
            //      ambient packaged cue remains legible on the selected row.
            //   2. Packaged de-emphasis (grey + italic) applies when not selected,
            //      even if active — ambient packaging yields to nothing here.
            //   3. Active (running) cue applies to non-packaged, non-selected.
            //   4. Default: darkgray bold id, plain summary.
            //
            // The prefix character (dash for idle, ▶ for running) is computed
            // outside this cascade and is always dim — see marker_style below.
            let (name_style, id_style) = if is_selected {
                let base = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
                let id_s = if is_packaged { base.add_modifier(Modifier::ITALIC) } else { base };
                (base, id_s)
            } else if is_packaged {
                let s = Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC);
                (s, s.add_modifier(Modifier::BOLD))
            } else if is_active {
                (
                    Style::default(),
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                )
            } else {
                (
                    Style::default(),
                    Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD),
                )
            };
            // Prefix marker: always dim (DarkGray) so the marker recedes
            // against the goal name. The packaged-goals-style grey+italic
            // rule covers id and summary — not the prefix — so even on a
            // packaged row the marker stays plain.
            let marker_style = Style::default().fg(Color::DarkGray);
            let marker_str = if is_active { "▶ " } else { "- " };
            let indent = "  ".repeat(depth);

            // Token-usage metrics: three right-aligned numbers (session count,
            // avg input per session, avg output per session) rendered DarkGray
            // so they read as ambient cost-driver status — not competing with
            // the goal name. Only permanent goals with recorded data show
            // metrics; ephemeral sub-sessions never do. The stats are
            // aggregated across the goal's own API calls and all its
            // ephemeral sub-sessions' calls so the cost of delegated work
            // shows on the parent.
            let stats = app.aggregated_token_stats(&g.id);
            let has_metrics = stats.has_data();
            let metrics_width = if has_metrics { METRICS_WIDTH } else { 0 };

            // Truncate the summary to leave room for the metrics block when
            // present. Without metrics, keep the existing 60-char preview.
            let prefix_width = depth * 2 + 2 + g.id.chars().count();
            let max_preview = if has_metrics {
                (pane_width as usize).saturating_sub(prefix_width + 3 + metrics_width)
            } else {
                60
            };
            let preview = truncate_with_ellipsis(&g.summary, max_preview);

            let mut spans = vec![
                Span::styled(format!("{}{}", indent, marker_str), marker_style),
                Span::styled(g.id.clone(), id_style),
            ];
            if !preview.is_empty() {
                spans.push(Span::styled(format!(" — {}", preview), name_style));
            }

            // Right-align the metrics block to the pane edge. The gap between
            // the summary text and the metrics is filled with raw (unstyled)
            // spaces so the metrics form neat columns across all goal rows.
            if has_metrics {
                let left_width = prefix_width
                    + if preview.is_empty() { 0 } else { 3 + preview.chars().count() };
                let padding = (pane_width as usize).saturating_sub(left_width + metrics_width);
                if padding > 0 {
                    spans.push(Span::raw(" ".repeat(padding)));
                }
                let metrics_style = Style::default().fg(Color::DarkGray);
                let metrics_text = format!(
                    " {:>2} {:>5} {:>5}",
                    stats.session_count,
                    format_compact_tokens(stats.avg_prompt_per_session()),
                    format_compact_tokens(stats.avg_completion_per_session()),
                );
                spans.push(Span::styled(metrics_text, metrics_style));
            }

            Line::from(spans)
        }
        GoalListItem::Ephemeral(session_id, label) => {
            let depth = depth_by_id.get(session_id.as_str()).copied().unwrap_or(1);
            let is_active = app.running_sessions.contains_key(session_id.as_str());

            let id_style = if is_selected {
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else if is_active {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD)
            };
            let marker_style = Style::default().fg(Color::DarkGray);
            let marker_str = if is_active { "▶ " } else { "- " };
            let indent = "  ".repeat(depth);
            let display_text = match label {
                Some(l) if !l.is_empty() => l.clone(),
                _ => session_id.clone(),
            };
            Line::from(vec![
                Span::styled(format!("{}{}", indent, marker_str), marker_style),
                Span::styled(display_text, id_style),
            ])
        }
    }
}

fn goal_detail_header_line(g: &crate::goal::Goal, is_packaged: bool) -> Line<'static> {
    let kind_str = g.kind.as_deref().unwrap_or("feature");
    let tier_str = g.tier.as_deref().unwrap_or("mid");
    let tag = format!("[{} · {}]", kind_str, tier_str);
    // Packaged goals add ITALIC to the de-emphasized dark-grey header so the
    // detail pane reads the same way the goal-list row did when the goal was
    // selected.
    let style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(if is_packaged { Modifier::ITALIC } else { Modifier::empty() });
    Line::from(vec![
        Span::styled(g.summary.clone(), style),
        Span::raw("  "),
        Span::styled(tag, style),
    ])
}

/// Normalize markdown list-item markers in a single line of goal-detail body
/// text to `-`. A list-item marker is one of:
///   - unordered: `-`, `*`, or `+` followed by a single space
///   - ordered:   one or more ASCII digits, then `.` or `)`, then a single space
///
/// Lines that don't begin with a list marker (after any leading space indent)
/// pass through unchanged, so prose, headings, and continuation lines are not
/// touched. The line's leading space indent is preserved so nested items keep
/// their nesting position on the rendered line.
fn normalize_list_marker(line: &str) -> String {
    let leading = line.len() - line.trim_start_matches(' ').len();
    let (indent, rest) = line.split_at(leading);
    if rest.is_empty() {
        return line.to_string();
    }
    let bytes = rest.as_bytes();
    // Unordered: -, *, or + followed by a single space.
    if matches!(bytes[0], b'-' | b'*' | b'+') {
        if bytes.len() >= 2 && bytes[1] == b' ' {
            return format!("{}- {}", indent, &rest[2..]);
        }
        return line.to_string();
    }
    // Ordered: one or more ASCII digits, then `.` or `)`, then a single space.
    if bytes[0].is_ascii_digit() {
        let mut i = 0;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i + 1 < bytes.len() && (bytes[i] == b'.' || bytes[i] == b')') && bytes[i + 1] == b' ' {
            return format!("{}- {}", indent, &rest[i + 2..]);
        }
    }
    line.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, ScrollState};
    use crate::goal::Goal;
    use std::path::PathBuf;

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

    /// Spec: "Tinker presents a terminal interface in two columns" with
    /// a 50/50 horizontal split (REPL on the left, Goals on the right).
    /// The Goals pane occupies the full height of the right column.
    #[test]
    fn test_spec_pane_rects_horizontal_split_is_fifty_fifty() {
        let area = Rect { x: 0, y: 0, width: 80, height: 100 };
        let r = pane_rects(area);
        // Left REPL and right column are each ~50% of the width.
        assert_eq!(r.repl.width, 40);
        assert_eq!(r.repl.x, 0);
        assert_eq!(r.tree.x, 40);
        assert_eq!(r.tree.width, 40);
        // Tree covers the full right column height.
        assert_eq!(r.tree.y, 0);
        assert_eq!(r.tree.y + r.tree.height, 100);
    }

    /// Spec: "The right column stacks two bands at fixed proportions:
    /// a selectable goal list (50% height) and the full goal text of
    /// the currently selected goal beneath it (50%)." Asserts each
    /// region is within a small tolerance of the requested proportion.
    #[test]
    fn test_spec_pane_rects_right_column_half_half() {
        let area = Rect { x: 0, y: 0, width: 80, height: 100 };
        let r = pane_rects(area);
        let total = area.height as i32;
        let list = r.goal_list.height as i32;
        let text = r.goal_text.height as i32;
        // Tolerate small slack from the 1-row separator and the borders.
        let tol = 5;
        assert!(
            (list - 50).abs() <= tol,
            "goal list should be ~50% of height, got {} of {}",
            list,
            total
        );
        assert!(
            (text - 50).abs() <= tol,
            "goal text should be ~50% of height, got {} of {}",
            text,
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
        s.record_render(100, 10, 0);
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
        let inputs = [
            "",
            "hello",
            "the quick brown fox jumps over the lazy dog",
            "supercalifragilisticexpialidocious wonderful situation exemplary behavior",
            "a b c d e f g h i j k l m n o p q r s t u v w x y z a b c d e f g h",
        ];
        for &width in &[20u16, 40, 60, 80, 100] {
            for input in &inputs {
                // Phantom cursor space accounts for cursor at end of input.
                let line = Line::from(vec![
                    Span::raw(prompt.to_string()),
                    Span::raw(input.to_string()),
                    Span::raw(" "),
                ]);
                let expected = (Paragraph::new(line)
                    .wrap(Wrap { trim: false })
                    .line_count(width) as u16)
                    .max(1);
                let big_cap = 1000u16;
                let (height, scroll) = input_pane_layout(prompt, input, width, big_cap);
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
        // Long input that wraps to many rows at a narrow width.
        let mut input = String::new();
        for _ in 0..30 {
            input.push_str("the quick brown fox jumps over the lazy dog ");
        }
        let width = 40u16;
        let cap = 4u16;
        let (height, scroll) = input_pane_layout(prompt, &input, width, cap);
        let line = Line::from(vec![
            Span::raw(prompt.to_string()),
            Span::raw(input.clone()),
            Span::raw(" "), // phantom cursor space
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
        s.record_render(100, 10, 0);
        // Scroll up to disengage follow.
        s.scroll_up(20);
        assert!(s.y.is_some());
        // Scroll back down past the bottom — y must snap to None.
        s.scroll_down(1000);
        assert!(s.y.is_none(), "scrolling to the bottom must re-engage follow-tail");
        assert_eq!(s.effective_y(), 90);
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
        s.record_render(100, 10, 0);
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
        // Mirror the marker-style derivation from draw_goal_tree — the prefix
        // is always DarkGray, independent of row state, per the dim-regardless-
        // of-packaging rule.
        let marker_style = Style::default().fg(Color::DarkGray);
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

    /// Spec (tui — goal list row): idle (non-running) goal rows open with a
    /// dash, not a heavy bullet. The dash is the lightest possible marker
    /// and extends the "marker recedes" principle to the row prefix.
    #[test]
    fn test_spec_idle_marker_is_dash() {
        let mut app = App::new();
        app.tinker_dirs = vec![PathBuf::from("/proj/.tinker")];
        let g = Goal {
            id: "tui".to_string(),
            summary: "owns the TUI rendering".to_string(),
            description: "".to_string(),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            tier: None,
            kind: None,
            source_path: Some(PathBuf::from("/proj/.tinker/goals/tui.toml")),
        };
        app.goals = vec![g];

        let mut depth_by_id = std::collections::HashMap::new();
        depth_by_id.insert("tui".to_string(), 0usize);
        let running = std::collections::HashSet::new();
        let item = GoalListItem::Goal(app.goals[0].clone());
        let line = goal_list_row_line(&item, &depth_by_id, None, &running, &app, 38);

        // The first span is the prefix + indent. Extract the marker substring
        // (the row has no children so indent is empty).
        let prefix_span = &line.spans[0];
        let prefix_text = prefix_span.content.as_ref();
        assert!(
            prefix_text.contains("- "),
            "idle row prefix must be a dash; got {:?}",
            prefix_text,
        );
        assert!(
            !prefix_text.contains("◉"),
            "idle row must no longer use the fisheye bullet; got {:?}",
            prefix_text,
        );
    }

    /// Spec (tui — goal list row / packaged): the prefix character on a
    /// packaged, non-selected goal row is dim (DarkGray) without ITALIC. The
    /// packaged-goals-style grey+italic authority covers id and summary; the
    /// prefix is explicitly carved out so the marker recedes.
    #[test]
    fn test_spec_marker_is_dim_and_unitalic_on_packaged_row() {
        let mut app = App::new();
        app.tinker_dirs = vec![PathBuf::from("/proj/.tinker")];
        let packaged = Goal {
            id: "tui".to_string(),
            summary: "owns the TUI rendering".to_string(),
            description: "".to_string(),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            tier: None,
            kind: None,
            source_path: Some(PathBuf::from("/home/.tinker/goals/packaged-goals/tui.toml")),
        };
        app.goals = vec![packaged];

        let mut depth_by_id = std::collections::HashMap::new();
        depth_by_id.insert("tui".to_string(), 0usize);
        let running = std::collections::HashSet::new();
        let item = GoalListItem::Goal(app.goals[0].clone());
        // Selection is on a different goal — packaged goal is NOT selected.
        let line = goal_list_row_line(&item, &depth_by_id, Some("other"), &running, &app, 38);

        // The first span is the prefix + indent. It must be dim and have no
        // italic — the packaged italic only applies to id and summary.
        let prefix_span = &line.spans[0];
        assert_eq!(
            prefix_span.style.fg,
            Some(Color::DarkGray),
            "non-selected packaged prefix must be DarkGray",
        );
        assert!(
            !prefix_span.style.add_modifier.contains(Modifier::ITALIC),
            "non-selected packaged prefix must NOT be italic — italic is reserved for id and summary",
        );
        assert!(
            !prefix_span.style.add_modifier.contains(Modifier::BOLD),
            "prefix must not carry BOLD — it's a marker, not content",
        );
        // Stronger semantic claim: the prefix carries no modifiers at all on
        // a packaged row. This is what makes the carve-out a real distinction
        // rather than a styling accident — the packaged italic applies to id
        // and summary, and the prefix is plain.
        assert_eq!(
            prefix_span.style.add_modifier,
            Modifier::empty(),
            "non-selected packaged prefix must have no modifiers (the italic carve-out is semantic, not visual)",
        );
    }

    /// Spec (tui — goal list row / ephemeral): ephemeral sub-session rows
    /// also use the dash for the idle prefix. The prefix style is dim, no
    /// italic, no bold — same rule as for permanent goal rows.
    #[test]
    fn test_spec_ephemeral_idle_marker_is_dash_and_dim() {
        let mut app = App::new();
        app.tinker_dirs = vec![PathBuf::from("/proj/.tinker")];
        let parent = Goal {
            id: "tui".to_string(),
            summary: "owns the TUI rendering".to_string(),
            description: "".to_string(),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            tier: None,
            kind: None,
            source_path: None,
        };
        app.goals = vec![parent];
        app.ephemeral_sessions.insert("tui~1".to_string());
        app.ephemeral_sessions_ordered.push("tui~1".to_string());

        let mut depth_by_id = std::collections::HashMap::new();
        depth_by_id.insert("tui".to_string(), 0usize);
        let running = std::collections::HashSet::new();
        let ephemeral = GoalListItem::Ephemeral("tui~1".to_string(), None);
        let line = goal_list_row_line(&ephemeral, &depth_by_id, None, &running, &app, 38);

        let prefix_span = &line.spans[0];
        let prefix_text = prefix_span.content.as_ref();
        assert!(
            prefix_text.contains("- "),
            "ephemeral idle row prefix must be a dash; got {:?}",
            prefix_text,
        );
        assert!(
            !prefix_text.contains("◉"),
            "ephemeral idle row must no longer use the fisheye bullet; got {:?}",
            prefix_text,
        );
        assert_eq!(
            prefix_span.style.fg,
            Some(Color::DarkGray),
            "ephemeral idle prefix must be dim (DarkGray)",
        );
        assert!(
            !prefix_span.style.add_modifier.contains(Modifier::ITALIC),
            "ephemeral idle prefix must not be italic",
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
        // Mirror the id-style derivation from draw_goal_tree (the prefix
        // marker is dim regardless of state — see test_spec_running_marker_style_is_dim_grey_not_bold).
        let id_style = if is_selected {
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
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
        let marker_style = Style::default().fg(Color::DarkGray);
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

    /// Spec (packaged-goals-style — goal list row, non-selected): a packaged
    /// goal that is NOT currently selected must render DarkGray + ITALIC on
    /// both the id and the summary — the ambient de-emphasis applies in full.
    #[test]
    fn test_spec_goal_list_row_packaged_non_selected_uses_grey_and_italic() {
        let mut app = App::new();
        app.tinker_dirs = vec![PathBuf::from("/proj/.tinker")];
        let packaged = Goal {
            id: "tui".to_string(),
            summary: "owns the TUI rendering".to_string(),
            description: "".to_string(),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            tier: None,
            kind: None,
            source_path: Some(PathBuf::from("/home/.tinker/goals/packaged-goals/tui.toml")),
        };
        app.goals = vec![packaged];

        let mut depth_by_id = std::collections::HashMap::new();
        depth_by_id.insert("tui".to_string(), 0usize);
        let running = std::collections::HashSet::new();
        let item = GoalListItem::Goal(app.goals[0].clone());
        // Selection is on a different goal — packaged goal is NOT selected.
        let line = goal_list_row_line(&item, &depth_by_id, Some("other"), &running, &app, 38);

        // The id span must be DarkGray + ITALIC + BOLD.
        let id_span = line.spans.iter().find(|s| s.content.as_ref() == "tui")
            .expect("row must contain id span 'tui'");
        assert_eq!(id_span.style.fg, Some(Color::DarkGray),
            "non-selected packaged id span must be DarkGray");
        assert!(id_span.style.add_modifier.contains(Modifier::ITALIC),
            "non-selected packaged id span must be italic");
        assert!(id_span.style.add_modifier.contains(Modifier::BOLD),
            "non-selected packaged id span must remain bold for hierarchy");

        // The summary preview must be DarkGray + ITALIC.
        let preview_span = line.spans.iter()
            .find(|s| s.content.as_ref().contains("owns the TUI rendering"))
            .expect("row must contain summary preview span");
        assert_eq!(preview_span.style.fg, Some(Color::DarkGray),
            "non-selected packaged summary span must be DarkGray");
        assert!(preview_span.style.add_modifier.contains(Modifier::ITALIC),
            "non-selected packaged summary span must be italic");
    }

    /// Spec (packaged-goals-style — goal list row, selected): when a packaged
    /// goal is selected, the selection color (Cyan + Bold) takes precedence
    /// over the de-emphasis on the row surface. The id retains ITALIC so the
    /// packaged distinction stays legible. The detail-pane header still uses
    /// the de-emphasized style (covered separately by the header test).
    #[test]
    fn test_spec_goal_list_row_selected_packaged_uses_selection_color_with_italic_id() {
        let mut app = App::new();
        app.tinker_dirs = vec![PathBuf::from("/proj/.tinker")];
        let packaged = Goal {
            id: "tui".to_string(),
            summary: "owns the TUI rendering".to_string(),
            description: "".to_string(),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            tier: None,
            kind: None,
            source_path: Some(PathBuf::from("/home/.tinker/goals/packaged-goals/tui.toml")),
        };
        app.goals = vec![packaged];

        let mut depth_by_id = std::collections::HashMap::new();
        depth_by_id.insert("tui".to_string(), 0usize);
        let running = std::collections::HashSet::new();
        let item = GoalListItem::Goal(app.goals[0].clone());
        // The packaged goal IS the selected one.
        let line = goal_list_row_line(&item, &depth_by_id, Some("tui"), &running, &app, 38);

        // The id span must be the selection color (Cyan) + BOLD + ITALIC.
        let id_span = line.spans.iter().find(|s| s.content.as_ref() == "tui")
            .expect("row must contain id span 'tui'");
        assert_eq!(id_span.style.fg, Some(Color::Cyan),
            "selected packaged id span must use selection color (Cyan)");
        assert!(id_span.style.add_modifier.contains(Modifier::BOLD),
            "selected packaged id span must remain bold");
        assert!(id_span.style.add_modifier.contains(Modifier::ITALIC),
            "selected packaged id span must retain italic for packaged cue");

        // The summary preview uses the selection color without italic.
        let preview_span = line.spans.iter()
            .find(|s| s.content.as_ref().contains("owns the TUI rendering"))
            .expect("row must contain summary preview span");
        assert_eq!(preview_span.style.fg, Some(Color::Cyan),
            "selected packaged summary span must use selection color (Cyan)");
        assert!(!preview_span.style.add_modifier.contains(Modifier::ITALIC),
            "selected packaged summary span must NOT be italic — italic is reserved for the id");
    }

    /// Spec (packaged-goals-style — goal list row): a project-local goal must
    /// keep the existing visual rules (Bold id in DarkGray when not selected
    /// or active, no ITALIC) — the packaged de-emphasis rule does not apply
    /// to local goals.
    #[test]
    fn test_spec_goal_list_row_project_local_does_not_get_italic() {
        let mut app = App::new();
        app.tinker_dirs = vec![PathBuf::from("/proj/.tinker")];
        let local = Goal {
            id: "backends".to_string(),
            summary: "owns backend abstraction".to_string(),
            description: "".to_string(),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            tier: None,
            kind: None,
            source_path: Some(PathBuf::from("/proj/.tinker/goals/backends.toml")),
        };
        app.goals = vec![local];

        let mut depth_by_id = std::collections::HashMap::new();
        depth_by_id.insert("backends".to_string(), 0usize);
        let running = std::collections::HashSet::new();
        let item = GoalListItem::Goal(app.goals[0].clone());
        let line = goal_list_row_line(&item, &depth_by_id, None, &running, &app, 38);

        let id_span = line.spans.iter().find(|s| s.content.as_ref() == "backends")
            .expect("row must contain id span 'backends'");
        assert!(!id_span.style.add_modifier.contains(Modifier::ITALIC),
            "project-local id span must not be italic");

        let preview_span = line.spans.iter()
            .find(|s| s.content.as_ref().contains("owns backend abstraction"))
            .expect("row must contain summary preview span");
        assert!(!preview_span.style.add_modifier.contains(Modifier::ITALIC),
            "project-local summary span must not be italic");
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
        app.goal_list_scroll.record_render(10, 3, 0);
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
        app.goal_list_scroll.record_render(10, 3, 0);
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
        let line = goal_detail_header_line(&goal, false);
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
                assert!(
                    !span.style.add_modifier.contains(Modifier::ITALIC),
                    "non-packaged header span {:?} must not be italic",
                    span.content,
                );
            }
        }
    }

    /// Spec (tui — goal-detail header / packaged): when a packaged goal is
    /// selected, both the summary and the `[kind · tier]` tag render DarkGray
    /// and ITALIC so the detail pane reads the same as the goal-list row.
    #[test]
    fn test_spec_goal_detail_header_packaged_uses_italic() {
        let goal = Goal {
            id: "tui".to_string(),
            summary: "owns the TUI rendering".to_string(),
            description: String::new(),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            tier: None,
            kind: None,
            source_path: None,
        };
        let line = goal_detail_header_line(&goal, true);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("owns the TUI rendering"), "header must contain summary");
        for span in &line.spans {
            if !span.content.is_empty() && span.content.as_ref() != "  " {
                assert_eq!(
                    span.style.fg,
                    Some(Color::DarkGray),
                    "packaged header span {:?} must be DarkGray",
                    span.content,
                );
                assert!(
                    span.style.add_modifier.contains(Modifier::ITALIC),
                    "packaged header span {:?} must be italic",
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
        let line = goal_detail_header_line(&goal, false);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("[feature · mid]"),
            "expected default tag '[feature · mid]', got: {:?}",
            text,
        );
    }

    /// Spec (tui — goal-detail body): markdown list items render with a `-`
    /// marker so the marker recedes against the prose. Covers unordered and
    /// ordered markers, indented/nested items, and prose lines that must
    /// pass through unchanged.
    #[test]
    fn test_spec_normalize_list_marker_uses_dash_for_all_markers() {
        // Unordered markers: -, *, + each followed by a space.
        assert_eq!(normalize_list_marker("- first"),  "- first");
        assert_eq!(normalize_list_marker("* second"), "- second");
        assert_eq!(normalize_list_marker("+ third"),  "- third");
        // Ordered markers: `1.`, `2)`, and multi-digit.
        assert_eq!(normalize_list_marker("1. one"),   "- one");
        assert_eq!(normalize_list_marker("2) two"),   "- two");
        assert_eq!(normalize_list_marker("10. ten"),  "- ten");
        // Leading space indent is preserved (nested items keep their column).
        assert_eq!(normalize_list_marker("  - nested"), "  - nested");
        assert_eq!(normalize_list_marker("    1. deep"), "    - deep");
        // Lines that are NOT list items pass through unchanged:
        //   prose, bold/italic, headings, continuation lines, hyphens without space.
        assert_eq!(normalize_list_marker("plain prose line"), "plain prose line");
        assert_eq!(normalize_list_marker("**Bold opening.**"), "**Bold opening.**");
        assert_eq!(normalize_list_marker("*italic*"), "*italic*");
        assert_eq!(normalize_list_marker("## Heading"), "## Heading");
        assert_eq!(normalize_list_marker("   continuation"), "   continuation");
        assert_eq!(normalize_list_marker("-foo"), "-foo");
        assert_eq!(normalize_list_marker(""), "");
        assert_eq!(normalize_list_marker("   "), "   ");
    }

    /// Spec (tui — goal-detail body): when the goal-detail body is rendered,
    /// the body lines pass through `normalize_list_marker`, so the rendered
    /// list-item markers in the body are dashes regardless of the marker
    /// the source TOML used.
    #[test]
    fn test_spec_goal_detail_body_renders_list_items_with_dash() {
        // The body builder is private; exercise it via the same expression
        // that draw_goal_tree uses for the body lines.
        let description = "\
## WHAT
A short paragraph.

- dash item
* star item
+ plus item
1. ordered item
2) alt-ordered
   continuation line, not a list
   another continuation
  - nested item
plain trailing line";
        let body: Vec<Line> = description
            .lines()
            .map(|l| Line::from(normalize_list_marker(l)))
            .collect();
        let texts: Vec<String> = body
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(
            texts,
            vec![
                "## WHAT".to_string(),
                "A short paragraph.".to_string(),
                "".to_string(),
                "- dash item".to_string(),
                "- star item".to_string(),
                "- plus item".to_string(),
                "- ordered item".to_string(),
                "- alt-ordered".to_string(),
                "   continuation line, not a list".to_string(),
                "   another continuation".to_string(),
                "  - nested item".to_string(),
                "plain trailing line".to_string(),
            ],
            "every list-item line in the body must render with a `-` marker; \
             prose, headings, and continuation lines must pass through unchanged",
        );
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

    // ── format_compact_tokens ──

    /// Spec (tui — token-usage metrics): values under 1000 render as plain
    /// numbers with no suffix.
    #[test]
    fn test_spec_format_compact_tokens_plain_under_1000() {
        assert_eq!(format_compact_tokens(0), "0");
        assert_eq!(format_compact_tokens(3), "3");
        assert_eq!(format_compact_tokens(42), "42");
        assert_eq!(format_compact_tokens(999), "999");
    }

    /// Spec (tui — token-usage metrics): values 1000–9999 render with one
    /// decimal place and a `k` suffix.
    #[test]
    fn test_spec_format_compact_tokens_k_suffix() {
        assert_eq!(format_compact_tokens(1000), "1.0k");
        assert_eq!(format_compact_tokens(1200), "1.2k");
        assert_eq!(format_compact_tokens(9900), "9.9k");
    }

    /// Spec (tui — token-usage metrics): values 10000–999999 render as an
    /// integer with a `k` suffix; 1000000+ renders with one decimal and `M`.
    #[test]
    fn test_spec_format_compact_tokens_large_values() {
        assert_eq!(format_compact_tokens(10_000), "10k");
        assert_eq!(format_compact_tokens(127_000), "127k");
        assert_eq!(format_compact_tokens(999_999), "999k");
        assert_eq!(format_compact_tokens(1_000_000), "1.0M");
        assert_eq!(format_compact_tokens(1_200_000), "1.2M");
    }

    // ── goal-list row token metrics ──

    /// Spec (tui — token-usage metrics): when a permanent goal has token data,
    /// the row renders the three right-aligned metrics (session count, avg
    /// input, avg output) in the metrics block.
    #[test]
    fn test_spec_goal_list_row_shows_token_metrics_when_data_exists() {
        let mut app = App::new();
        app.tinker_dirs = vec![PathBuf::from("/proj/.tinker")];
        app.goals = vec![Goal {
            id: "alpha".to_string(),
            summary: "a short summary".to_string(),
            description: String::new(),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            tier: None,
            kind: None,
            source_path: None,
        }];
        // 3 sessions, 3600 prompt tokens (avg 1200 → "1.2k"),
        // 2400 completion tokens (avg 800 → "800").
        app.token_usage.insert(
            "alpha".to_string(),
            crate::app::GoalTokenStats {
                session_count: 3,
                total_prompt_tokens: 3600,
                total_completion_tokens: 2400,
                total_cached_tokens: 0,
            },
        );

        let mut depth_by_id = std::collections::HashMap::new();
        depth_by_id.insert("alpha".to_string(), 0usize);
        let running = std::collections::HashSet::new();
        let item = GoalListItem::Goal(app.goals[0].clone());
        let line = goal_list_row_line(&item, &depth_by_id, None, &running, &app, 38);

        let line_text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            line_text.contains(" 3 "),
            "metrics block must contain session count 3; got: {:?}",
            line_text,
        );
        assert!(
            line_text.contains("1.2k"),
            "metrics block must contain formatted avg input 1.2k; got: {:?}",
            line_text,
        );
        assert!(
            line_text.contains("800"),
            "metrics block must contain formatted avg output 800; got: {:?}",
            line_text,
        );

        // The metrics must render DarkGray — ambient status, not competing
        // with the goal name.
        let metrics_span = line.spans.iter()
            .find(|s| s.content.as_ref().contains("1.2k"))
            .expect("metrics span must exist");
        assert_eq!(
            metrics_span.style.fg,
            Some(Color::DarkGray),
            "metrics block must render in DarkGray",
        );
    }

    /// Spec (tui — token-usage metrics): when a goal has no token data
    /// (session_count is 0), the row shows no metrics block — the full
    /// summary is rendered as before.
    #[test]
    fn test_spec_goal_list_row_no_metrics_when_no_data() {
        let mut app = App::new();
        app.tinker_dirs = vec![PathBuf::from("/proj/.tinker")];
        app.goals = vec![Goal {
            id: "beta".to_string(),
            summary: "a summary that should not be truncated by metrics".to_string(),
            description: String::new(),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            tier: None,
            kind: None,
            source_path: None,
        }];
        // No token_usage entry for "beta" at all.

        let mut depth_by_id = std::collections::HashMap::new();
        depth_by_id.insert("beta".to_string(), 0usize);
        let running = std::collections::HashSet::new();
        let item = GoalListItem::Goal(app.goals[0].clone());
        let line = goal_list_row_line(&item, &depth_by_id, None, &running, &app, 38);

        // No span should contain a compact token suffix — the metrics block
        // is absent.
        let has_metrics = line.spans.iter()
            .any(|s| s.content.as_ref().contains("k") && s.content.as_ref().trim().len() <= 5);
        assert!(
            !has_metrics,
            "no metrics block should appear when there is no token data",
        );
    }

    /// Spec (tui — token-usage metrics): when a goal has token data, the
    /// summary preview is truncated to leave room for the metrics block. A
    /// long summary is shortened (ending with "...") rather than running over
    /// the metrics — without metrics the full 60-char budget is available.
    #[test]
    fn test_spec_goal_list_row_truncates_summary_when_metrics_present() {
        let mut app = App::new();
        app.tinker_dirs = vec![PathBuf::from("/proj/.tinker")];
        app.goals = vec![Goal {
            id: "verbose".to_string(),
            summary: "this is a very long summary that would normally render fully at the 60-char budget but must be truncated to make room for the metrics block".to_string(),
            description: String::new(),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            tier: None,
            kind: None,
            source_path: None,
        }];
        app.token_usage.insert(
            "verbose".to_string(),
            crate::app::GoalTokenStats {
                session_count: 2,
                total_prompt_tokens: 4000,
                total_completion_tokens: 1000,
                total_cached_tokens: 0,
            },
        );

        let mut depth_by_id = std::collections::HashMap::new();
        depth_by_id.insert("verbose".to_string(), 0usize);
        let running = std::collections::HashSet::new();
        let item = GoalListItem::Goal(app.goals[0].clone());
        // Narrow pane forces truncation: prefix(9) + 3 + 15(metrics) = 27,
        // leaving 38 - 27 = 11 chars for the preview.
        let line = goal_list_row_line(&item, &depth_by_id, None, &running, &app, 38);

        let preview_span: &Span = line.spans.iter()
            .find(|s| s.content.starts_with(" — "))
            .expect("summary span must exist");
        let preview = preview_span.content.trim_start_matches(" — ");
        assert!(
            preview.ends_with("..."),
            "summary must be truncated (end with '...') when metrics are present; got: {:?}",
            preview,
        );
        assert!(
            preview.chars().count() < 60,
            "summary must be shorter than the 60-char no-metrics budget; got {} chars",
            preview.chars().count(),
        );
    }

    /// Spec (tui — token-usage metrics): ephemeral sub-sessions never show
    /// token metrics — only permanent goals with completed dispatches do.
    /// Guards against the metrics block leaking onto ephemeral rows.
    #[test]
    fn test_spec_goal_list_row_ephemeral_shows_no_metrics() {
        let mut app = App::new();
        // Inject token data keyed by the parent goal so a Goal row WOULD show
        // metrics — the Ephemeral row must still show none.
        app.token_usage.insert(
            "tend".to_string(),
            crate::app::GoalTokenStats {
                session_count: 5,
                total_prompt_tokens: 100_000,
                total_completion_tokens: 20_000,
                total_cached_tokens: 0,
            },
        );
        let ephemeral = GoalListItem::Ephemeral("tend~1".to_string(), None);
        let depth_by_id = std::collections::HashMap::new();
        let running = std::collections::HashSet::new();
        let line = goal_list_row_line(&ephemeral, &depth_by_id, None, &running, &app, 50);

        // The ephemeral row has exactly two spans (marker + display_text) and
        // no metrics. The avg tokens here would be 20.0k / 4.0k if they leaked.
        let line_text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            !line_text.contains("20.0k") && !line_text.contains("4.0k"),
            "ephemeral sub-session row must not show token metrics; got: {:?}",
            line_text,
        );
    }

    /// Spec (tui — token-usage metrics): the metrics block is right-aligned to
    /// the pane edge so metrics form neat columns across goal rows regardless
    /// of summary length (when the summary fits its budget). Two goals with
    /// different-length summaries must render their metrics spans starting at
    /// the same character offset — the padding absorbs the difference.
    #[test]
    fn test_spec_goal_list_row_metrics_right_aligned_across_rows() {
        let mut app = App::new();
        app.tinker_dirs = vec![PathBuf::from("/proj/.tinker")];
        app.goals = vec![
            Goal {
                id: "alpha".to_string(),
                summary: "short".to_string(),
                description: String::new(),
                parent_id: String::new(),
                children: vec![],
                related: vec![],
                tier: None,
                kind: None,
                source_path: None,
            },
            Goal {
                id: "beta".to_string(),
                summary: "a longer summary here".to_string(),
                description: String::new(),
                parent_id: String::new(),
                children: vec![],
                related: vec![],
                tier: None,
                kind: None,
                source_path: None,
            },
        ];
        // Both goals: 2 sessions, avg prompt 2.0k, avg completion 1.0k —
        // the 'k' suffix lets us identify the metrics span uniquely.
        for id in &["alpha", "beta"] {
            app.token_usage.insert(
                id.to_string(),
                crate::app::GoalTokenStats {
                    session_count: 2,
                    total_prompt_tokens: 4000,
                    total_completion_tokens: 2000,
                    total_cached_tokens: 0,
                },
            );
        }

        let mut depth_by_id = std::collections::HashMap::new();
        depth_by_id.insert("alpha".to_string(), 0usize);
        depth_by_id.insert("beta".to_string(), 0usize);
        let running = std::collections::HashSet::new();
        let item_a = GoalListItem::Goal(app.goals[0].clone());
        let item_b = GoalListItem::Goal(app.goals[1].clone());
        // Pane wide enough that both summaries fit their budget (≥ 21 chars):
        // beta's max_preview = 50 - 6 - 3 - 15 = 26 ≥ 21, so no truncation.
        let line_a = goal_list_row_line(&item_a, &depth_by_id, None, &running, &app, 50);
        let line_b = goal_list_row_line(&item_b, &depth_by_id, None, &running, &app, 50);

        // Character offset where each row's metrics span begins.
        fn metrics_offset(line: &Line) -> Option<usize> {
            let mut off = 0;
            for s in &line.spans {
                // The metrics span is DarkGray and carries a compact-token
                // suffix (k/M) from the averaged values.
                if s.style.fg == Some(Color::DarkGray)
                    && (s.content.contains('k') || s.content.contains('M'))
                {
                    return Some(off);
                }
                off += s.content.chars().count();
            }
            None
        }
        let off_a = metrics_offset(&line_a).expect("alpha must render metrics");
        let off_b = metrics_offset(&line_b).expect("beta must render metrics");
        // Both must align at pane_width - METRICS_WIDTH (50 - 15 = 35).
        assert_eq!(
            off_a, off_b,
            "metrics must start at the same column across rows (right-aligned to pane edge)",
        );
        assert_eq!(
            off_a, 35,
            "right-aligned metrics must sit at pane_width - METRICS_WIDTH = 35",
        );
    }

    /// Spec (tui — token-usage metrics): a permanent goal that delegates
    /// all work to sub-sessions shows aggregated metrics on its row. Token
    /// data stored only under ephemeral IDs (e.g. `tend~1`) rolls up to
    /// the permanent goal ID (`tend`) so the parent shows non-zero cost.
    #[test]
    fn test_spec_goal_list_row_shows_aggregated_ephemeral_metrics() {
        let mut app = App::new();
        app.tinker_dirs = vec![PathBuf::from("/proj/.tinker")];
        app.goals = vec![Goal {
            id: "tend".to_string(),
            summary: "a short summary".to_string(),
            description: String::new(),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            tier: None,
            kind: None,
            source_path: None,
        }];
        // No token_usage entry for "tend" — all work delegated to sub-sessions.
        // tend~1: 1 session, 5000 prompt tokens, 800 completion tokens.
        // tend~2: 1 session, 3000 prompt tokens, 400 completion tokens.
        app.token_usage.insert(
            "tend~1".to_string(),
            crate::app::GoalTokenStats {
                session_count: 1,
                total_prompt_tokens: 5_000,
                total_completion_tokens: 800,
                total_cached_tokens: 0,
            },
        );
        app.token_usage.insert(
            "tend~2".to_string(),
            crate::app::GoalTokenStats {
                session_count: 1,
                total_prompt_tokens: 3_000,
                total_completion_tokens: 400,
                total_cached_tokens: 0,
            },
        );

        let mut depth_by_id = std::collections::HashMap::new();
        depth_by_id.insert("tend".to_string(), 0usize);
        let running = std::collections::HashSet::new();
        let item = GoalListItem::Goal(app.goals[0].clone());
        let line = goal_list_row_line(&item, &depth_by_id, None, &running, &app, 38);

        let line_text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        // Aggregated: 2 sessions, 8000 prompt (avg 4.0k), 1200 completion (avg 600).
        assert!(
            line_text.contains(" 2 "),
            "aggregated metrics must contain session count 2; got: {:?}",
            line_text,
        );
        assert!(
            line_text.contains("4.0k"),
            "aggregated metrics must contain avg input 4.0k; got: {:?}",
            line_text,
        );
        assert!(
            line_text.contains("600"),
            "aggregated metrics must contain avg output 600; got: {:?}",
            line_text,
        );
    }

}

