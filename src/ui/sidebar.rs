/// Sidebar tree navigator.
///
/// Shows a two-section tree:
///   ▼ Databases
///     ▸ my_db  (selected)
///       ├── table_a
///       ├── table_b
///       └── table_c
///
/// Keyboard navigation (j/k/↑/↓) and Enter to select are handled in
/// `app.rs`; this module only renders.
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Widget},
};

use crate::state::{AppState, FocusPanel, SidebarFocus};

// ── Theme ─────────────────────────────────────────────────────────────────────
// Most static palette stays here; the row-selected background is pulled from
// `app.theme` at render time so that `--theme light` / `--theme high-contrast`
// flip the visible highlight colour.
const ACCENT: Color = Color::Cyan;
const SELECTED_FG: Color = Color::White;
const DB_FG: Color = Color::Rgb(97, 175, 239);
const TABLE_FG: Color = Color::Rgb(200, 200, 200);
const MUTED: Color = Color::Rgb(110, 110, 110);
const SECTION_FG: Color = Color::Rgb(86, 182, 194);
const SEARCH_BG: Color = Color::Rgb(28, 40, 58);

fn rgb((r, g, b): (u8, u8, u8)) -> Color {
    Color::Rgb(r, g, b)
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Render the sidebar tree inside `area` (which already has a border drawn
/// by `layout.rs`).  We draw *inside* the border, so we shrink by 1 on each
/// side.
pub fn render_sidebar(area: Rect, buf: &mut Buffer, app: &AppState) {
    // The outer border is drawn by layout.rs.  We receive the full bordered
    // area, so compute the inner area manually.
    let inner = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let focused = app.focus == FocusPanel::Sidebar;

    // If there is a search string, show a small search bar at the top.
    let (search_h, content_area) = if !app.search_query.is_empty() {
        let areas = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(2), Constraint::Min(0)])
            .split(inner);
        render_search_bar(areas[0], buf, &app.search_query);
        (2u16, areas[1])
    } else {
        (0, inner)
    };

    let _ = search_h;

    render_tree(content_area, buf, app, focused);
}

// ── Search bar ────────────────────────────────────────────────────────────────

fn render_search_bar(area: Rect, buf: &mut Buffer, query: &str) {
    let block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(ACCENT));
    let inner = block.inner(area);
    block.render(area, buf);

    let line = Line::from(vec![
        Span::styled(
            "/ ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(query, Style::default().fg(SELECTED_FG).bg(SEARCH_BG)),
    ]);
    buf.set_line(inner.x, inner.y, &line, inner.width);
}

// ── Tree renderer ─────────────────────────────────────────────────────────────

fn render_tree(area: Rect, buf: &mut Buffer, app: &AppState, focused: bool) {
    if area.height == 0 {
        return;
    }

    // Build a flat list of visible tree items.
    let items = build_items(app);

    if items.is_empty() {
        let msg = Line::from(Span::styled("  (no databases)", Style::default().fg(MUTED)));
        buf.set_line(area.x, area.y, &msg, area.width);
        return;
    }

    let selected_bg = rgb(app.theme.bg_selected);

    // Determine scroll so the selected item is always visible.
    let visible_h = area.height as usize;
    let selected_idx = find_selected_idx(&items, app);
    let scroll = compute_scroll(selected_idx, visible_h, items.len());

    for (screen_row, item) in items.iter().skip(scroll).take(visible_h).enumerate() {
        let y = area.y + screen_row as u16;
        let is_selected = Some(scroll + screen_row) == selected_idx;

        let bg = if is_selected && focused {
            selected_bg
        } else {
            Color::Reset
        };
        let fg = if is_selected && focused {
            SELECTED_FG
        } else {
            item.fg
        };

        // Fill row background
        for x in area.x..area.x + area.width {
            buf[(x, y)].set_char(' ').set_style(Style::default().bg(bg));
        }

        let style = if is_selected && focused {
            Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(fg).bg(bg)
        };

        let line = Line::from(Span::styled(&item.label, style));
        buf.set_line(area.x, y, &line, area.width);
    }

    // Scroll indicator
    if items.len() > visible_h {
        let pct = scroll * 100 / items.len().max(1);
        let ind = format!("{pct}%");
        let ind_x = area.x + area.width.saturating_sub(ind.len() as u16 + 1);
        let ind_y = area.y + area.height - 1;
        let ind_line = Line::from(Span::styled(format!(" {ind}"), Style::default().fg(MUTED)));
        buf.set_line(ind_x, ind_y, &ind_line, ind.len() as u16 + 1);
    }
}

// ── Database name display helper ──────────────────────────────────────────────

/// Return a display-friendly version of a database identifier.
///
/// SpacetimeDB 2.0 returns 256-bit hex identity strings (64 chars) as database
/// references.  We truncate these for display; human-readable names pass through
/// unchanged.
fn display_db_name(db: &str) -> String {
    if db.len() >= 32 && db.chars().all(|c| c.is_ascii_hexdigit()) {
        format!("{}…", &db[..12])
    } else {
        db.to_string()
    }
}

// ── Tree item model ───────────────────────────────────────────────────────────

#[derive(Debug)]
struct TreeItem {
    label: String,
    fg: Color,
    /// Whether this item represents a database (vs. a table).
    is_db: bool,
    /// Index into `app.databases` or `app.tables`.
    idx: usize,
}

fn build_items(app: &AppState) -> Vec<TreeItem> {
    let mut items: Vec<TreeItem> = Vec::new();

    // Section header
    items.push(TreeItem {
        label: " ◈ DATABASES".to_string(),
        fg: SECTION_FG,
        is_db: false,
        idx: usize::MAX,
    });

    let search = app.search_query.to_lowercase();

    for (di, db) in app.databases.iter().enumerate() {
        // Filter by search
        if !search.is_empty() && !db.name.to_lowercase().contains(&search) {
            continue;
        }

        let is_selected_db = app.selected_database_idx == Some(di);
        let arrow = if is_selected_db { "▼" } else { "▶" };
        let display = display_db_name(&db.name);
        // Flag paused databases — maincloud suspends inactive ones.
        let suffix = if db.is_paused() { "  ⏸ paused" } else { "" };
        let label = format!("  {arrow} {display}{suffix}");

        items.push(TreeItem {
            label,
            fg: if db.is_paused() { MUTED } else { DB_FG },
            is_db: true,
            idx: di,
        });

        // If this DB is selected, show its tables below it
        if is_selected_db {
            if app.tables.is_empty() {
                // Three terminal states the placeholder can be in:
                //   1. schema fetch in flight → spinner
                //   2. schema fetch failed    → error hint
                //   3. neither flag set       → truly empty schema
                let placeholder = if app.schema_loading {
                    "      (loading…)"
                } else if app.schema_load_failed {
                    "      (schema unavailable — press r to retry)"
                } else {
                    "      (no tables)"
                };
                items.push(TreeItem {
                    label: placeholder.to_string(),
                    fg: MUTED,
                    is_db: false,
                    idx: usize::MAX,
                });
            } else {
                for (ti, table) in app.tables.iter().enumerate() {
                    // Filter tables too
                    if !search.is_empty()
                        && !table.table_name.to_lowercase().contains(&search)
                        && !db.name.to_lowercase().contains(&search)
                    {
                        continue;
                    }

                    let is_last = ti == app.tables.len() - 1;
                    let branch = if is_last { "└──" } else { "├──" };
                    let is_selected_tbl = app.selected_table_idx == Some(ti);
                    let marker = if is_selected_tbl { "▸ " } else { "  " };
                    // Reuse the module inspector's access glyphs so a private
                    // table reads the same way in both views.
                    let access_icon = if table.table_access == "private" {
                        "🔒"
                    } else {
                        "🌐"
                    };
                    let label =
                        format!("      {branch} {marker}{access_icon} {}", table.table_name);

                    items.push(TreeItem {
                        label,
                        fg: if is_selected_tbl { ACCENT } else { TABLE_FG },
                        is_db: false,
                        idx: ti,
                    });
                }
            }
        }
    }

    items
}

/// Find the flat index of the currently selected item.
fn find_selected_idx(items: &[TreeItem], app: &AppState) -> Option<usize> {
    match app.sidebar_focus {
        SidebarFocus::Databases => items
            .iter()
            .position(|it| it.is_db && Some(it.idx) == app.selected_database_idx),
        SidebarFocus::Tables => items.iter().position(|it| {
            !it.is_db && it.idx != usize::MAX && Some(it.idx) == app.selected_table_idx
        }),
    }
}

fn compute_scroll(selected: Option<usize>, visible_h: usize, total: usize) -> usize {
    match selected {
        None => 0,
        Some(sel) => {
            if sel < visible_h {
                0
            } else if sel >= total.saturating_sub(visible_h) {
                total.saturating_sub(visible_h)
            } else {
                sel.saturating_sub(visible_h / 2)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Database, DatabaseStatus};

    #[test]
    fn paused_database_row_is_flagged() {
        let mut app = AppState::new("http://localhost:3000");
        app.databases = vec![
            Database::new("active-db"),
            Database {
                name: "paused-db".to_string(),
                status: DatabaseStatus::Paused,
            },
        ];

        let items = build_items(&app);
        let active = items
            .iter()
            .find(|i| i.label.contains("active-db"))
            .unwrap();
        let paused = items
            .iter()
            .find(|i| i.label.contains("paused-db"))
            .unwrap();

        assert!(!active.label.contains("paused"));
        assert!(paused.label.contains("⏸ paused"));
    }

    #[test]
    fn table_rows_flag_private_vs_public_access() {
        use crate::api::types::TableInfo;

        let table = |name: &str, access: &str| TableInfo {
            table_name: name.to_string(),
            product_type_ref: 0,
            table_type: "user".to_string(),
            table_access: access.to_string(),
            columns: Vec::new(),
            primary_key_cols: Vec::new(),
            indexes: Vec::new(),
            constraints: Vec::new(),
        };

        let mut app = AppState::new("http://localhost:3000");
        app.databases = vec![Database::new("db")];
        app.selected_database_idx = Some(0);
        app.tables = vec![
            table("public_tbl", "public"),
            table("secret_tbl", "private"),
        ];

        let items = build_items(&app);
        let public = items
            .iter()
            .find(|i| i.label.contains("public_tbl"))
            .unwrap();
        let private = items
            .iter()
            .find(|i| i.label.contains("secret_tbl"))
            .unwrap();

        assert!(public.label.contains('🌐'));
        assert!(!public.label.contains('🔒'));
        assert!(private.label.contains('🔒'));
        assert!(!private.label.contains('🌐'));
    }
}
