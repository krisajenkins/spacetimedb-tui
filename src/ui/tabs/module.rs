/// Module Inspector tab.
///
/// Lists reducers, scheduled reducers, and views from the current database
/// schema.  Data is read from `app.current_schema`.
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Widget},
};

use crate::state::{AppState, FocusPanel};

// ── Theme ─────────────────────────────────────────────────────────────────────
const ACCENT: Color = Color::Cyan;
const BORDER_FOCUSED: Color = Color::Cyan;
const BORDER_NORMAL: Color = Color::Rgb(40, 50, 65);
const FG_MUTED: Color = Color::Rgb(110, 110, 110);
const FG_REDUCER: Color = Color::Rgb(229, 192, 123);
const FG_TABLE: Color = Color::Rgb(97, 175, 239);
const FG_SYSTEM: Color = Color::Rgb(86, 182, 194);
const FG_VIEW: Color = Color::Rgb(198, 160, 246);
const FG_PARAM: Color = Color::Rgb(180, 180, 180);
const SELECTED_BG: Color = Color::Rgb(36, 52, 72);
const SECTION_FG: Color = Color::Rgb(86, 182, 194);

// ── Public entry point ────────────────────────────────────────────────────────

/// Render the module inspector tab.
pub fn render_module(area: Rect, buf: &mut Buffer, app: &AppState, selected_reducer: usize) {
    let focused = app.focus == FocusPanel::Main;
    let border_color = if focused {
        BORDER_FOCUSED
    } else {
        BORDER_NORMAL
    };

    let outer = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(
            " 🔧 Module Inspector ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    let inner = outer.inner(area);
    outer.render(area, buf);

    if inner.height == 0 {
        return;
    }

    match &app.current_schema {
        None => {
            let msg = Line::from(Span::styled(
                "  Select a database to inspect its module",
                Style::default().fg(FG_MUTED),
            ));
            let y = inner.y + inner.height / 2;
            buf.set_line(inner.x, y, &msg, inner.width);
        }
        Some(schema) => {
            // Split into left (reducers) | right (tables / system tables)
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(inner);

            render_reducers(cols[0], buf, schema, selected_reducer, focused);
            render_tables_panel(cols[1], buf, schema);
        }
    }
}

// ── Reducers panel ────────────────────────────────────────────────────────────

fn render_reducers(
    area: Rect,
    buf: &mut Buffer,
    schema: &crate::api::types::Schema,
    selected: usize,
    focused: bool,
) {
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(Color::Rgb(40, 55, 75)))
        .title(Span::styled(
            " Reducers ",
            Style::default().fg(SECTION_FG).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    block.render(area, buf);

    if inner.height == 0 {
        return;
    }

    if schema.reducers.is_empty() {
        let msg = Line::from(Span::styled(
            "  (no reducers)",
            Style::default().fg(FG_MUTED),
        ));
        buf.set_line(inner.x, inner.y, &msg, inner.width);
        return;
    }

    let visible_h = inner.height as usize;
    let scroll = if selected >= visible_h {
        selected - visible_h + 1
    } else {
        0
    };

    for (i, reducer) in schema
        .reducers
        .iter()
        .enumerate()
        .skip(scroll)
        .take(visible_h)
    {
        let y = inner.y + (i - scroll) as u16;
        if y >= inner.y + inner.height {
            break;
        }

        let is_selected = i == selected && focused;
        let bg = if is_selected {
            SELECTED_BG
        } else {
            Color::Reset
        };

        // Fill row
        for x in inner.x..inner.x + inner.width {
            buf[(x, y)].set_char(' ').set_style(Style::default().bg(bg));
        }

        // Determine if this is a scheduled reducer (name contains "scheduled" or "__schedule")
        let is_scheduled = reducer.name.contains("scheduled")
            || reducer.name.starts_with("__")
            || reducer.name.contains("_schedule");

        let prefix = if is_scheduled { "⏰ " } else { "⚡ " };
        let fg = if is_scheduled { FG_SYSTEM } else { FG_REDUCER };

        // Build parameter signature
        let params: Vec<String> = reducer
            .params
            .iter()
            .map(|p| format!("{}: {}", p.name, type_display(&p.algebraic_type)))
            .collect();
        let sig = params.join(", ");

        let name_span = Span::styled(
            format!("  {prefix}{}", reducer.name),
            Style::default().fg(fg).bg(bg).add_modifier(if is_selected {
                Modifier::BOLD
            } else {
                Modifier::empty()
            }),
        );
        let sig_span = Span::styled(format!("({sig})"), Style::default().fg(FG_PARAM).bg(bg));

        let line = Line::from(vec![name_span, sig_span]);
        buf.set_line(inner.x, y, &line, inner.width);
    }

    // Count indicator
    let count_line = Line::from(Span::styled(
        format!("  {} reducers", schema.reducers.len()),
        Style::default().fg(FG_MUTED),
    ));
    if inner.height > 0 {
        let footer_y = inner.y + inner.height - 1;
        buf.set_line(inner.x, footer_y, &count_line, inner.width);
    }
}

// ── Tables panel ──────────────────────────────────────────────────────────────

fn render_tables_panel(area: Rect, buf: &mut Buffer, schema: &crate::api::types::Schema) {
    let block = Block::default().borders(Borders::NONE).title(Span::styled(
        " Tables ",
        Style::default().fg(SECTION_FG).add_modifier(Modifier::BOLD),
    ));
    let inner = block.inner(area);
    block.render(area, buf);

    if inner.height == 0 {
        return;
    }

    if schema.tables.is_empty() && schema.views.is_empty() {
        let msg = Line::from(Span::styled("  (no tables)", Style::default().fg(FG_MUTED)));
        buf.set_line(inner.x, inner.y, &msg, inner.width);
        return;
    }

    // Split into user tables and system tables
    let user_tables: Vec<_> = schema
        .tables
        .iter()
        .filter(|t| {
            t.table_type != "system"
                && !crate::api::types::SYSTEM_TABLES.contains(&t.table_name.as_str())
        })
        .collect();
    let system_tables: Vec<_> = schema
        .tables
        .iter()
        .filter(|t| {
            t.table_type == "system"
                || crate::api::types::SYSTEM_TABLES.contains(&t.table_name.as_str())
        })
        .collect();

    let visible_h = inner.height as usize;
    let mut row = 0usize;

    // User tables section
    if !user_tables.is_empty() && row < visible_h {
        let section_line = Line::from(Span::styled(
            "  USER TABLES",
            Style::default()
                .fg(SECTION_FG)
                .add_modifier(Modifier::UNDERLINED),
        ));
        buf.set_line(inner.x, inner.y + row as u16, &section_line, inner.width);
        row += 1;
    }

    for tbl in &user_tables {
        if row >= visible_h {
            break;
        }
        let access_icon = if tbl.table_access == "private" {
            "🔒"
        } else {
            "🌐"
        };
        let line = Line::from(vec![
            Span::styled(
                format!("    {access_icon} {}", tbl.table_name),
                Style::default().fg(FG_TABLE),
            ),
            Span::styled(
                format!("  ({} cols)", tbl.columns.len()),
                Style::default().fg(FG_MUTED),
            ),
        ]);
        buf.set_line(inner.x, inner.y + row as u16, &line, inner.width);
        row += 1;

        // Show columns if there's room
        for col in &tbl.columns {
            if row >= visible_h {
                break;
            }
            let col_line = Line::from(Span::styled(
                format!(
                    "       ├─ {}: {}{}",
                    col.col_name,
                    type_display(&col.col_type),
                    if col.is_autoinc { " (autoinc)" } else { "" }
                ),
                Style::default().fg(FG_MUTED),
            ));
            buf.set_line(inner.x, inner.y + row as u16, &col_line, inner.width);
            row += 1;
        }
    }

    // Views section — read-only, server-defined queries from `misc_exports`.
    if !schema.views.is_empty() && row < visible_h {
        row += 1; // blank line
        if row < visible_h {
            let section_line = Line::from(Span::styled(
                "  VIEWS",
                Style::default()
                    .fg(FG_VIEW)
                    .add_modifier(Modifier::UNDERLINED),
            ));
            buf.set_line(inner.x, inner.y + row as u16, &section_line, inner.width);
            row += 1;
        }
    }

    for view in &schema.views {
        if row >= visible_h {
            break;
        }
        let access = if view.table_access == "private" {
            "private"
        } else {
            "public"
        };
        let line = Line::from(vec![
            Span::styled(
                format!("    👁️ {}", view.table_name),
                Style::default().fg(FG_VIEW),
            ),
            Span::styled(
                format!("  ({access}, {} cols)", view.columns.len()),
                Style::default().fg(FG_MUTED),
            ),
        ]);
        buf.set_line(inner.x, inner.y + row as u16, &line, inner.width);
        row += 1;
    }

    // System tables section
    if !system_tables.is_empty() && row < visible_h {
        row += 1; // blank line
        if row < visible_h {
            let section_line = Line::from(Span::styled(
                "  SYSTEM TABLES",
                Style::default()
                    .fg(FG_SYSTEM)
                    .add_modifier(Modifier::UNDERLINED),
            ));
            buf.set_line(inner.x, inner.y + row as u16, &section_line, inner.width);
            row += 1;
        }
    }

    for tbl in &system_tables {
        if row >= visible_h {
            break;
        }
        let line = Line::from(Span::styled(
            format!("    ⚙ {}", tbl.table_name),
            Style::default().fg(FG_SYSTEM),
        ));
        buf.set_line(inner.x, inner.y + row as u16, &line, inner.width);
        row += 1;
    }
}

// ── Utilities ─────────────────────────────────────────────────────────────────

/// Convert an algebraic type JSON value to a compact display string.
fn type_display(v: &serde_json::Value) -> String {
    // Timestamp/Identity/… are products tagged by a magic field name; surface
    // the friendly label rather than the bare `Product` tag.
    if let Some(label) = crate::api::types::special_type_label(v) {
        return label.to_string();
    }
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Object(o) => {
            // SpacetimeDB encodes types as `{"tag": ...}` objects
            if let Some(tag) = o.keys().next() {
                tag.clone()
            } else {
                "{}".to_string()
            }
        }
        serde_json::Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}
