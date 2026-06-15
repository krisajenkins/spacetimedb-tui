/// SQL Console tab.
///
/// Layout (vertical):
///   ┌─ SQL Console ─────────────────────────────────────────────────────────┐
///   │  [history list — scrollable]                                           │
///   ├────────────────────────────────────────────────────────────────────────┤
///   │  > SQL input line                                                      │
///   ├────────────────────────────────────────────────────────────────────────┤
///   │  [result grid]                                                         │
///   └────────────────────────────────────────────────────────────────────────┘
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, StatefulWidget, Widget},
};

use crate::state::{AppState, FocusPanel};
use crate::ui::components::input::{InputState, InputWidget};
use crate::ui::components::table_grid::{TableGrid, TableGridState, render_empty};

fn rgb((r, g, b): (u8, u8, u8)) -> Color {
    Color::Rgb(r, g, b)
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Render the SQL console tab.
pub fn render_sql(
    area: Rect,
    buf: &mut Buffer,
    app: &AppState,
    input_state: &InputState,
    grid_state: &mut TableGridState,
) {
    let theme = &app.theme;
    let accent = rgb(theme.accent);
    let border_focused = rgb(theme.border_focused);
    let border_normal = rgb(theme.border_normal);

    let focused = app.focus == FocusPanel::Main || app.focus == FocusPanel::SqlInput;
    let border_color = if focused {
        border_focused
    } else {
        border_normal
    };

    let outer_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(
            " ⌨  SQL Console ",
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ));
    let inner = outer_block.inner(area);
    outer_block.render(area, buf);

    if inner.height < 4 {
        return;
    }

    // ── Layout ────────────────────────────────────────────────────────────
    // history (min 3) | input (3) | results (rest, min 3)
    let history_h = inner.height.clamp(3, 8);
    let input_h = 3u16;
    let results_h = inner.height.saturating_sub(history_h + input_h);

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(history_h),
            Constraint::Length(input_h),
            Constraint::Min(results_h.max(3)),
        ])
        .split(inner);

    let history_area = sections[0];
    let input_area = sections[1];
    let results_area = sections[2];

    // ── History panel ─────────────────────────────────────────────────────
    render_history(history_area, buf, app);

    // ── Input widget ──────────────────────────────────────────────────────
    let sql_focused = app.focus == FocusPanel::SqlInput;
    InputWidget::new(input_state)
        .title("SQL  (Enter=run  Tab=complete  ↑↓=history  Ctrl+K=clear)")
        .placeholder("SELECT * FROM <table> LIMIT 100")
        .focused(sql_focused)
        .highlight_sql(true)
        .render(input_area, buf);

    // ── Results panel ─────────────────────────────────────────────────────
    render_results(results_area, buf, app, grid_state, focused);
}

// ── History ───────────────────────────────────────────────────────────────────

fn render_history(area: Rect, buf: &mut Buffer, app: &AppState) {
    let theme = &app.theme;
    let border_normal = rgb(theme.border_normal);
    let fg_muted = rgb(theme.fg_muted);
    let fg_primary = rgb(theme.fg_primary);
    let success = rgb(theme.success);
    let error_fg = rgb(theme.error);
    let history_bg = rgb(theme.bg_secondary);
    let history_sel = rgb(theme.bg_selected);

    let block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(border_normal))
        .title(Span::styled(" History ", Style::default().fg(fg_muted)));
    let inner = block.inner(area);
    block.render(area, buf);

    if inner.height == 0 {
        return;
    }

    // Fill background
    for y in inner.y..inner.y + inner.height {
        for x in inner.x..inner.x + inner.width {
            buf[(x, y)]
                .set_char(' ')
                .set_style(Style::default().bg(history_bg));
        }
    }

    if app.sql_history.is_empty() {
        let line = Line::from(Span::styled(
            "  (no history yet — execute a query with Enter)",
            Style::default().fg(fg_muted),
        ));
        buf.set_line(inner.x, inner.y, &line, inner.width);
        return;
    }

    // Show most-recent entries at the bottom
    let visible_h = inner.height as usize;
    let total = app.sql_history.len();
    let skip = total.saturating_sub(visible_h);

    for (row, entry) in app.sql_history.iter().skip(skip).enumerate() {
        let y = inner.y + row as u16;
        if y >= inner.y + inner.height {
            break;
        }

        // Highlight the currently browsed history entry
        let is_selected = app
            .history_cursor
            .map(|c| {
                // history_cursor counts from end: 0 = latest
                total.saturating_sub(1).saturating_sub(c) == skip + row
            })
            .unwrap_or(false);

        let bg = if is_selected { history_sel } else { history_bg };

        // Fill row
        for x in inner.x..inner.x + inner.width {
            buf[(x, y)].set_char(' ').set_style(Style::default().bg(bg));
        }

        let dur = format_duration(entry.duration);
        let status_span = if entry.error.is_some() {
            Span::styled(" ✗ ", Style::default().fg(error_fg).bg(bg))
        } else {
            Span::styled(" ✓ ", Style::default().fg(success).bg(bg))
        };

        let time_span = Span::styled(
            format!("{} ", entry.executed_at.format("%H:%M:%S")),
            Style::default().fg(fg_muted).bg(bg),
        );
        let dur_span = Span::styled(format!("[{dur}] "), Style::default().fg(fg_muted).bg(bg));
        let sql_span = Span::styled(
            truncate_str(&entry.sql, inner.width as usize - 20),
            Style::default()
                .fg(fg_primary)
                .bg(bg)
                .add_modifier(if is_selected {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        );

        let line = Line::from(vec![status_span, time_span, dur_span, sql_span]);
        buf.set_line(inner.x, y, &line, inner.width);
    }
}

// ── Results ───────────────────────────────────────────────────────────────────

fn render_results(
    area: Rect,
    buf: &mut Buffer,
    app: &AppState,
    grid_state: &mut TableGridState,
    focused: bool,
) {
    match &app.query_result {
        None if app.query_loading => {
            render_empty(area, buf, "  ⟳ Executing query…", focused);
        }
        None => {
            render_empty(
                area,
                buf,
                "  Results will appear here — type SQL above and press Enter",
                focused,
            );
        }
        Some(qr) => {
            if qr.schema.is_empty() && qr.rows.is_empty() {
                render_empty(area, buf, "  Query executed — no rows returned", focused);
                return;
            }

            let headers: Vec<String> = qr.column_names().iter().map(|s| s.to_string()).collect();
            let rows: Vec<Vec<String>> = crate::ui::tabs::tables::display_rows(qr);

            let dur = format_micros(qr.total_duration_micros);
            let base_title = format!("Results — {} rows  ({})", rows.len(), dur);
            let title = match app.grid_search.as_deref() {
                Some(q) if app.grid_search_editing => format!("{base_title}  /{q}_"),
                Some(q) if !q.is_empty() => format!("{base_title}  [/{q}]"),
                _ => base_title,
            };

            TableGrid::new(&headers, &rows)
                .title(title)
                .focused(focused)
                .max_col_width(60)
                .highlight_query(app.grid_search.as_deref())
                .render(area, buf, grid_state);
        }
    }
}

// ── Utilities ─────────────────────────────────────────────────────────────────

fn format_duration(d: std::time::Duration) -> String {
    let ms = d.as_millis();
    if ms >= 1000 {
        format!("{:.1}s", d.as_secs_f64())
    } else {
        format!("{ms}ms")
    }
}

fn format_micros(us: u64) -> String {
    if us >= 1_000_000 {
        format!("{:.1}s", us as f64 / 1_000_000.0)
    } else if us >= 1_000 {
        format!("{:.1}ms", us as f64 / 1_000.0)
    } else {
        format!("{us}µs")
    }
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max.saturating_sub(1)])
    }
}
