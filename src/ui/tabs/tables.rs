/// Tables tab — browse the rows of the currently selected table.
///
/// Layout:
///   ┌─ Table Browser ──────────────────────────────────────────────────────┐
///   │  [pagination info]                                                    │
///   │  ┌── table_grid ──────────────────────────────────────────────────┐  │
///   │  │  col1 │ col2 │ col3 │ …                                        │  │
///   │  │  ──────┼──────┼──────┼                                         │  │
///   │  │  …     │ …    │ …    │                                         │  │
///   │  └────────────────────────────────────────────────────────────────┘  │
///   └───────────────────────────────────────────────────────────────────────┘
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, StatefulWidget, Widget},
};

use crate::state::AppState;
use crate::ui::components::table_grid::{TableGrid, TableGridState, render_empty};

fn rgb((r, g, b): (u8, u8, u8)) -> Color {
    Color::Rgb(r, g, b)
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Render the tables tab into `area`.
pub fn render_tables(
    area: Rect,
    buf: &mut Buffer,
    app: &AppState,
    grid_state: &mut TableGridState,
) {
    let theme = &app.theme;
    let accent = rgb(theme.accent);
    let border_focused = rgb(theme.border_focused);
    let border_normal = rgb(theme.border_normal);

    // Outer block
    let focused = matches!(app.focus, crate::state::FocusPanel::Main);
    let border_color = if focused {
        border_focused
    } else {
        border_normal
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(
            " 📋 Table Browser ",
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    block.render(area, buf);

    if inner.height == 0 {
        return;
    }

    // ── Info bar ──────────────────────────────────────────────────────────
    let info_area = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: 1,
    };
    let grid_area = Rect {
        x: inner.x,
        y: inner.y + 1,
        width: inner.width,
        height: inner.height.saturating_sub(1),
    };

    render_info_bar(info_area, buf, app);

    // ── Grid ──────────────────────────────────────────────────────────────
    match build_table_data(app) {
        Some((headers, rows, title)) => {
            // Decorate the title with either the live search prompt
            // or the spreadsheet edit-mode indicator so the user
            // always knows which mode they're in.
            let display_title = if let Some(ref em) = app.edit_mode {
                format!("{title}  [EDIT — {} pending]", em.pending_count())
            } else {
                match app.grid_search.as_deref() {
                    Some(q) if app.grid_search_editing => {
                        format!("{title}  /{q}_")
                    }
                    Some(q) if !q.is_empty() => format!("{title}  [/{q}]"),
                    _ => title,
                }
            };

            // Flatten the pending edits into the `(row, col, value)`
            // shape the grid widget consumes.
            let pending_tuples: Vec<(usize, usize, String)> = app
                .edit_mode
                .as_ref()
                .map(|em| {
                    em.pending
                        .iter()
                        .map(|p| (p.data_row_idx, p.col_idx, p.new_value.clone()))
                        .collect()
                })
                .unwrap_or_default();

            let mut widget = TableGrid::new(&headers, &rows)
                .title(display_title)
                .focused(focused)
                .highlight_query(app.grid_search.as_deref())
                .pending_edits(&pending_tuples);

            // If the inline editor is open, tell the grid which
            // cell to paint as an input box plus where the cursor
            // sits inside the buffer.
            if let Some(ref em) = app.edit_mode {
                if let Some(ref editor) = em.editor {
                    let data_row = crate::ui::components::table_grid::sorted_data_index(
                        &rows,
                        grid_state.sort_col,
                        grid_state.sort_desc,
                        grid_state.selected_row,
                    )
                    .unwrap_or(0);
                    widget = widget.active_editor(
                        data_row,
                        grid_state.selected_col,
                        &editor.value,
                        editor.cursor,
                    );
                }
            }

            widget.render(grid_area, buf, grid_state);
        }
        None => {
            let msg = if app.selected_database().is_none() {
                "  Select a database from the sidebar"
            } else if app.selected_table().is_none() {
                "  Select a table from the sidebar"
            } else if app.query_loading {
                "  Loading table data…"
            } else {
                "  No data — press r to refresh"
            };
            render_empty(grid_area, buf, msg, focused);
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn render_info_bar(area: Rect, buf: &mut Buffer, app: &AppState) {
    let theme = &app.theme;
    let accent = rgb(theme.accent);
    let fg_primary = rgb(theme.fg_primary);
    let fg_muted = rgb(theme.fg_muted);
    let warning = rgb(theme.warning);
    let success = rgb(theme.success);
    let bg_info = rgb(theme.bg_secondary);

    // Fill background
    for x in area.x..area.x + area.width {
        buf[(x, area.y)]
            .set_char(' ')
            .set_style(Style::default().bg(bg_info));
    }

    let mut spans: Vec<Span> = Vec::new();

    if let Some(db) = app.selected_database() {
        spans.push(Span::styled(
            format!(" 🗄 {db}"),
            Style::default().fg(accent),
        ));
    }
    if let Some(tbl) = app.selected_table() {
        // Views read as 👁 so it's clear the grid is a read-only view.
        let name = if tbl.is_view {
            format!("  ›  👁️ {}", tbl.table_name)
        } else {
            format!("  ›  {}", tbl.table_name)
        };
        spans.push(Span::styled(
            name,
            Style::default().fg(fg_primary).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!("  ({} columns)", tbl.columns.len()),
            Style::default().fg(fg_muted),
        ));
    }

    if let Some(ref qr) = app.table_browse_result {
        spans.push(Span::styled(
            format!("  {} rows", qr.row_count()),
            Style::default().fg(fg_muted),
        ));
    }

    // Live subscription badge for the currently selected table.
    if let Some(tbl) = app.selected_table() {
        if let Some(live_rows) = app.live_table_data.get(&tbl.table_name) {
            spans.push(Span::styled(
                format!("  ● live: {} rows", live_rows.len()),
                Style::default().fg(success).add_modifier(Modifier::BOLD),
            ));
        }
    }

    if app.query_loading {
        spans.push(Span::styled(
            "  ⟳ loading…",
            Style::default().fg(warning).add_modifier(Modifier::BOLD),
        ));
    }

    // Right-align hint
    let hint = Span::styled(" r:refresh  n:next  p:prev ", Style::default().fg(fg_muted));
    let hint_w = hint.content.len() as u16;
    let hint_x = area.x + area.width.saturating_sub(hint_w);

    let line = Line::from(spans);
    buf.set_line(area.x, area.y, &line, area.width.saturating_sub(hint_w));
    buf.set_line(hint_x, area.y, &Line::from(hint), hint_w);
}

/// Build (headers, rows, title) from the latest table-browse result or cache.
fn build_table_data(app: &AppState) -> Option<(Vec<String>, Vec<Vec<String>>, String)> {
    // Prefer the most recent table-browse result; fall back to the cache if
    // available (e.g. when navigating back to a table that was loaded earlier
    // in the session).
    let qr = app.table_browse_result.as_ref().or_else(|| {
        let db = app.selected_database()?;
        let tbl = app.selected_table()?;
        let key = AppState::cache_key(db, &tbl.table_name);
        app.table_cache.get(&key).map(|c| &c.result)
    })?;

    if qr.schema.is_empty() {
        return None;
    }

    let headers: Vec<String> = qr.column_names().iter().map(|s| s.to_string()).collect();
    let rows: Vec<Vec<String>> = display_rows(qr);

    let title = app
        .selected_table()
        .map(|t| t.table_name.clone())
        .unwrap_or_else(|| "Results".to_string());

    Some((headers, rows, title))
}

/// Like [`value_to_display`] but aware of the column's algebraic type, so
/// SpacetimeDB special types (Timestamp, TimeDuration, …) render in
/// human-readable form instead of as raw JSON.
pub fn value_to_display_typed(v: &serde_json::Value, col_type: &serde_json::Value) -> String {
    crate::api::types::format_special_value(v, col_type).unwrap_or_else(|| value_to_display(v))
}

/// Project every row of a [`QueryResult`](crate::api::types::QueryResult) into
/// display strings, formatting each cell according to its column's algebraic
/// type. Used everywhere the rendered rows must agree (grid, sort, search,
/// copy, export).
pub fn display_rows(qr: &crate::api::types::QueryResult) -> Vec<Vec<String>> {
    qr.rows
        .iter()
        .map(|row| {
            row.iter()
                .enumerate()
                .map(|(i, v)| match qr.schema.get(i) {
                    Some(col) => value_to_display_typed(v, &col.algebraic_type),
                    None => value_to_display(v),
                })
                .collect()
        })
        .collect()
}

/// Convert a `serde_json::Value` to a compact display string.
pub fn value_to_display(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => "NULL".to_string(),
        serde_json::Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(a) => {
            let items: Vec<String> = a.iter().map(value_to_display).collect();
            format!("[{}]", items.join(", "))
        }
        serde_json::Value::Object(o) => {
            let pairs: Vec<String> = o
                .iter()
                .map(|(k, v)| format!("{k}:{}", value_to_display(v)))
                .collect();
            format!("{{{}}}", pairs.join(", "))
        }
    }
}
