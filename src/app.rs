/// Application orchestrator.
///
/// [`App`] owns both the [`AppState`] (all UI state) and the
/// [`SpacetimeClient`] (async HTTP API).  The main event loop:
///
/// 1. Draws the current frame via [`draw_frame`].
/// 2. Waits for either a crossterm keyboard/resize event **or** an async API
///    event arriving on the mpsc channel.
/// 3. Dispatches the event to the appropriate handler.
/// 4. Loops until `app_state.should_quit` is set.
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::mpsc;

use ratatui::widgets::Widget;

use crate::{
    api::{
        SpacetimeClient,
        ws::{WsConfig, WsEvent, WsHandle},
    },
    config::Config,
    state::{
        AppState, ConnectionStatus, FocusPanel, HistoryAdvance, SidebarFocus, SqlHistoryEntry, Tab,
    },
    ui::components::input::InputState,
    ui::components::table_grid::TableGridState,
};

// ── Tick rate ─────────────────────────────────────────────────────────────────

/// How often we redraw even when there is no event.
const TICK_RATE: Duration = Duration::from_millis(200);

// ── Async API events ──────────────────────────────────────────────────────────

/// Events produced by background async tasks and delivered to the event loop.
#[derive(Debug)]
pub enum AppEvent {
    /// Databases list fetched.
    DatabasesLoaded(Vec<String>),
    /// Tables / schema fetched for the selected database.
    SchemaLoaded(crate::api::types::SchemaResponse),
    /// Schema fetch failed — carries a pre-formatted error message.
    /// Separate from the generic `Error` variant so the handler can
    /// clear `schema_loading` and flip `schema_load_failed` atomically.
    SchemaError(String),
    /// Schema fetch found the database paused (maincloud suspends
    /// inactive databases). Flags the database as paused and shows the
    /// message, rather than reporting a generic schema failure.
    SchemaPaused { database: String, message: String },
    /// SQL query result arrived (user-typed SQL in the SQL console tab).
    QueryResult {
        result: crate::api::types::QueryResult,
        duration: Duration,
        sql: String,
    },
    /// Table-browse result arrived (triggered by selecting a table from the
    /// sidebar). Kept separate from `QueryResult` so the Tables tab and the
    /// SQL tab do not share state.
    TableBrowseResult {
        result: crate::api::types::QueryResult,
    },
    /// Table-browse load failed.
    TableBrowseError { error: String },
    /// SQL query failed.
    QueryError { sql: String, error: String },
    /// Log lines fetched.
    LogsLoaded(Vec<crate::api::types::LogEntry>),
    /// Metrics fetched.
    MetricsLoaded(crate::state::MetricsSnapshot),
    /// Live tab's periodic `st_client` poll returned.
    LiveClientsLoaded(Vec<crate::state::app_state::LiveClientEntry>),
    /// A reducer call (or write-SQL exec) finished successfully.
    /// `op` is a short human label like `call insert_user` or
    /// `delete row from users` so we can surface it in the status bar
    /// and the Live tab without re-deriving the description here.
    WriteOpSuccess {
        op: String,
        response: serde_json::Value,
    },
    /// A reducer call (or write-SQL exec) failed.
    WriteOpError { op: String, error: String },
    /// A live log line from WebSocket.
    LogLine(crate::api::types::LogEntry),
    /// Ping result.
    PingResult(bool),
    /// Generic notification.
    Notification(String),
    /// Generic error.
    Error(String),
}

// ── App struct ────────────────────────────────────────────────────────────────

/// Top-level application struct.
pub struct App {
    pub state: AppState,
    pub client: SpacetimeClient,
    /// Sender half — cloned into background tasks.
    pub event_tx: mpsc::UnboundedSender<AppEvent>,
    /// Receiver half — consumed by the event loop.
    event_rx: mpsc::UnboundedReceiver<AppEvent>,
    /// Persistent user preferences from `~/.config/spacetimedb-tui/`.
    /// Read once at startup; the only thing we mutate at runtime is
    /// `SessionState`, which is written back on quit.
    user_config: crate::user_config::UserConfig,
    /// In-memory copy of the last-known session state, applied to the
    /// UI once the database list arrives in `bootstrap`.
    pending_session: Option<crate::user_config::SessionState>,
    /// SQL input state — single source of truth for the SQL editor buffer.
    pub sql_input: InputState,
    /// Table grid state for the tables tab.
    pub tables_grid: TableGridState,
    /// Table grid state for the SQL results.
    pub sql_grid: TableGridState,
    /// Active WebSocket subscription handle (set after database selection).
    ws_handle: Option<WsHandle>,
    /// WebSocket base URL (e.g. `ws://localhost:3000`).
    ws_url: String,
    /// Auth token for WebSocket connections.
    auth_token: Option<String>,
    /// Last time the metrics tab pulled fresh data — used to throttle the
    /// background refresh task to one fetch every `METRICS_REFRESH_INTERVAL`.
    last_metrics_fetch: Option<Instant>,
    /// Last time the Live tab polled `st_client` for the connected-client
    /// list. Throttled the same way metrics are.
    last_live_clients_fetch: Option<Instant>,
}

/// How often the Metrics tab automatically refreshes server-side metrics.
const METRICS_REFRESH_INTERVAL: Duration = Duration::from_secs(10);
/// How often the Live tab re-polls `st_client` for connected clients.
const LIVE_CLIENTS_REFRESH_INTERVAL: Duration = Duration::from_secs(10);

/// Maximum time we wait for any single HTTP-backed background request before
/// surfacing a timeout error to the user.
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Send an [`AppEvent`] from a background task, logging a warning if the
/// receiver has been dropped (which only happens during shutdown).
fn send_event(tx: &mpsc::UnboundedSender<AppEvent>, event: AppEvent) {
    if tx.send(event).is_err() {
        tracing::warn!("AppEvent channel closed; dropping event");
    }
}

impl App {
    /// Create a new [`App`] from config and a pre-built client.
    pub fn new(config: &Config, client: SpacetimeClient) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut state = AppState::new(config.server_url.clone());
        state.theme = config.theme.clone();
        Self {
            state,
            client,
            event_tx: tx,
            event_rx: rx,
            sql_input: InputState::new(),
            tables_grid: TableGridState::new(),
            sql_grid: TableGridState::new(),
            ws_handle: None,
            ws_url: config.ws_url.clone(),
            auth_token: config.auth_token.clone(),
            last_metrics_fetch: None,
            last_live_clients_fetch: None,
            user_config: config.user_config.clone(),
            pending_session: if config.user_config.restore_session {
                Some(crate::user_config::SessionState::load())
            } else {
                None
            },
        }
    }

    // ── Bootstrap ─────────────────────────────────────────────────────────

    /// Perform the initial connection check and database listing.
    pub async fn bootstrap(&mut self) {
        self.state.connection.status = ConnectionStatus::Connecting;
        let client = self.client.clone();
        let tx = self.event_tx.clone();

        tokio::spawn(async move {
            let ping_ok = matches!(
                tokio::time::timeout(HTTP_REQUEST_TIMEOUT, client.ping()).await,
                Ok(true)
            );
            send_event(&tx, AppEvent::PingResult(ping_ok));
            if !ping_ok {
                return;
            }
            match tokio::time::timeout(HTTP_REQUEST_TIMEOUT, client.list_databases()).await {
                Ok(Ok(dbs)) => send_event(&tx, AppEvent::DatabasesLoaded(dbs)),
                Ok(Err(e)) => send_event(&tx, AppEvent::Error(format!("list_databases: {e:#}"))),
                Err(_) => send_event(
                    &tx,
                    AppEvent::Error("list_databases: request timed out".to_string()),
                ),
            }
        });
    }

    // ── Main event loop ───────────────────────────────────────────────────

    /// Run the application until the user quits.
    pub async fn run<B: ratatui::backend::Backend>(
        &mut self,
        terminal: &mut ratatui::Terminal<B>,
    ) -> Result<()> {
        self.bootstrap().await;

        loop {
            // Draw
            terminal
                .draw(|frame| {
                    draw_frame(
                        frame,
                        &mut self.state,
                        &self.sql_input,
                        &mut self.tables_grid,
                        &mut self.sql_grid,
                    )
                })
                .context("Terminal draw failed")?;

            // Poll for crossterm events (non-blocking, timeout = TICK_RATE)
            if event::poll(TICK_RATE).context("event::poll failed")? {
                match event::read().context("event::read failed")? {
                    Event::Key(key) => {
                        self.handle_key(key).await;
                    }
                    Event::Resize(_, _) => {
                        // Crossterm always redraws the next frame after a
                        // resize since we're in non-blocking poll mode, so
                        // we just consume the event and let the tick rate
                        // handle the redraw.
                    }
                    Event::Mouse(_) => {}
                    _ => {}
                }
            }

            // Drain async API events (non-blocking)
            while let Ok(ev) = self.event_rx.try_recv() {
                self.handle_app_event(ev).await;
            }

            // Drain WebSocket events (non-blocking)
            self.drain_ws_events().await;

            // Throttled background refresh of server metrics while the
            // Metrics tab is visible.
            self.maybe_refresh_metrics();

            // Throttled poll of st_client for the Live tab.
            self.maybe_refresh_live_clients();

            // Expire notifications
            self.state.tick_notifications(Duration::from_secs(5));

            if self.state.should_quit {
                break;
            }
        }

        // Persist last-known UI state for the next launch.
        if self.user_config.restore_session {
            let snapshot = crate::user_config::SessionState {
                last_database: self.state.selected_database().map(str::to_string),
                last_table: self.state.selected_table().map(|t| t.table_name.clone()),
                last_tab: Some(tab_to_index(self.state.current_tab)),
            };
            snapshot.save();
        }

        Ok(())
    }

    /// If the Live tab is visible and we haven't polled `st_client`
    /// recently, spawn a background SQL query that fills
    /// `state.live_clients`.
    fn maybe_refresh_live_clients(&mut self) {
        if self.state.current_tab != Tab::Live {
            return;
        }
        let due = match self.last_live_clients_fetch {
            None => true,
            Some(t) => t.elapsed() >= LIVE_CLIENTS_REFRESH_INTERVAL,
        };
        if !due {
            return;
        }
        let db = match self.state.selected_database() {
            Some(d) => d.to_string(),
            None => return,
        };
        self.last_live_clients_fetch = Some(Instant::now());

        let client = self.client.clone();
        let tx = self.event_tx.clone();
        tokio::spawn(async move {
            // `st_client` is a system table; we cap the result so a
            // huge production deployment doesn't hang the UI.
            let sql = "SELECT * FROM st_client LIMIT 200";
            let fetch = tokio::time::timeout(HTTP_REQUEST_TIMEOUT, client.query_sql(&db, sql));
            let Ok(Ok(result)) = fetch.await else {
                // Silent — `st_client` may not be exposed on some
                // deployments; we don't want to spam the error popup.
                return;
            };
            let clients: Vec<crate::state::app_state::LiveClientEntry> = result
                .rows
                .iter()
                .map(|row| {
                    // Best-effort: pick the first string-ish cell as
                    // the identity. We don't have a reliable schema
                    // for st_client across server versions.
                    let identity = row
                        .iter()
                        .find_map(|v| match v {
                            serde_json::Value::String(s) => Some(s.clone()),
                            serde_json::Value::Number(n) => Some(n.to_string()),
                            _ => None,
                        })
                        .unwrap_or_else(|| "(unknown)".to_string());
                    crate::state::app_state::LiveClientEntry {
                        identity,
                        connected_at: None,
                    }
                })
                .collect();
            send_event(&tx, AppEvent::LiveClientsLoaded(clients));
        });
    }

    /// If the user is on the Metrics tab and we haven't fetched metrics
    /// recently, spawn a background fetch. Throttled by
    /// [`METRICS_REFRESH_INTERVAL`] to keep network traffic minimal.
    fn maybe_refresh_metrics(&mut self) {
        if self.state.current_tab != Tab::Metrics {
            return;
        }
        let due = match self.last_metrics_fetch {
            None => true,
            Some(t) => t.elapsed() >= METRICS_REFRESH_INTERVAL,
        };
        if !due {
            return;
        }
        self.last_metrics_fetch = Some(Instant::now());

        let client = self.client.clone();
        let tx = self.event_tx.clone();
        tokio::spawn(async move {
            if let Ok(Ok(text)) =
                tokio::time::timeout(HTTP_REQUEST_TIMEOUT, client.get_metrics()).await
            {
                let snapshot = parse_prometheus_metrics(&text);
                send_event(&tx, AppEvent::MetricsLoaded(snapshot));
            }
        });
    }

    // ── Key dispatch ──────────────────────────────────────────────────────

    /// Dispatch a keyboard event to the appropriate handler.
    ///
    /// Uses explicit `return` statements to make early-exit control flow clear.
    #[allow(clippy::needless_return)]
    async fn handle_key(&mut self, key: KeyEvent) {
        // ── Command palette intercept ─────────────────────────────────────
        // The palette owns every key while it's open. Ctrl+C still quits.
        if self.state.palette.is_some() {
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('c'))
            {
                self.state.should_quit = true;
                return;
            }
            self.handle_palette_key(key).await;
            return;
        }

        // ── Modal dialog intercept ────────────────────────────────────────
        // When a confirm prompt or form is open, the modal owns every
        // key. Ctrl+C still quits as a panic-button escape hatch.
        if self.state.modal.is_some() {
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('c'))
            {
                self.state.should_quit = true;
                return;
            }
            self.handle_modal_key(key).await;
            return;
        }

        // ── Spreadsheet edit-mode intercept ───────────────────────────────
        // When edit mode is active on the Tables tab, the whole key map
        // changes (cell cursor / inline editor / save / revert). We still
        // honour Ctrl+C as an escape hatch and Ctrl+E to toggle off.
        if self.state.edit_mode.is_some()
            && self.state.focus == FocusPanel::Main
            && self.state.current_tab == Tab::Tables
        {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                match key.code {
                    KeyCode::Char('c') => {
                        self.state.should_quit = true;
                        return;
                    }
                    KeyCode::Char('e') => {
                        self.exit_edit_mode();
                        return;
                    }
                    _ => {}
                }
            }
            self.handle_edit_mode_key(key).await;
            return;
        }

        // ── Global always-active bindings ─────────────────────────────────
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => {
                    self.state.should_quit = true;
                    return;
                }
                KeyCode::Char('r') => {
                    // Force a fresh WebSocket connection (e.g. after a server bounce).
                    self.connect_ws().await;
                    self.state
                        .set_notification("Reconnecting WebSocket…".to_string());
                    return;
                }
                KeyCode::Char('e')
                    if self.state.focus == FocusPanel::Main
                        && self.state.current_tab == Tab::Tables =>
                {
                    // Ctrl+E on the Tables tab enters spreadsheet edit
                    // mode. Only fires when we're *not* already in it —
                    // the intercept above catches the toggle-off case.
                    self.enter_edit_mode();
                    return;
                }
                KeyCode::Char('p') => {
                    // Open the command palette.
                    self.state.palette = Some(crate::state::palette::CommandPalette::new());
                    return;
                }
                KeyCode::Char('a') | KeyCode::Home if self.state.focus == FocusPanel::SqlInput => {
                    self.sql_input.home();
                    return;
                }
                KeyCode::Char('e') | KeyCode::End if self.state.focus == FocusPanel::SqlInput => {
                    self.sql_input.end();
                    return;
                }
                KeyCode::Char('k') if self.state.focus == FocusPanel::SqlInput => {
                    self.sql_input.kill_to_end();
                    self.state.history_cursor = None;
                    return;
                }
                KeyCode::Char('u') if self.state.focus == FocusPanel::SqlInput => {
                    self.sql_input.kill_to_start();
                    self.state.history_cursor = None;
                    return;
                }
                KeyCode::Char('l') if self.state.focus == FocusPanel::SqlInput => {
                    self.sql_input.clear();
                    self.state.history_cursor = None;
                    return;
                }
                KeyCode::Char('f')
                    if matches!(self.state.current_tab, Tab::Tables | Tab::Sql)
                        && self.state.focus == FocusPanel::Main =>
                {
                    // Ctrl+F opens the grid search prompt.
                    self.state.grid_search = Some(String::new());
                    self.state.grid_search_editing = true;
                    return;
                }
                KeyCode::Char('w') if self.state.focus == FocusPanel::SqlInput => {
                    // Delete the previous word (Ctrl+W, classic Unix convention).
                    let before = &self.sql_input.value[..self.sql_input.cursor];
                    let trimmed_end = before.trim_end_matches(|c: char| c.is_whitespace());
                    let word_start = trimmed_end
                        .rfind(|c: char| c.is_whitespace() || !(c.is_alphanumeric() || c == '_'))
                        .map(|i| i + 1)
                        .unwrap_or(0);
                    let range = word_start..self.sql_input.cursor;
                    if !range.is_empty() {
                        self.sql_input.replace_range(range, "");
                        self.state.history_cursor = None;
                    }
                    return;
                }
                _ => {}
            }
        }

        // ── Help overlay ──────────────────────────────────────────────────
        if self.state.show_help {
            // Clamp the scroll offset to the actual number of lines so a
            // user mashing `↓` doesn't push the value into the millions
            // (and then have to bash `↑` for ages to recover).
            let max_scroll =
                crate::ui::components::help::HelpOverlay::total_lines().saturating_sub(1);
            match key.code {
                KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q') => {
                    self.state.show_help = false;
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    self.state.help_scroll =
                        self.state.help_scroll.saturating_add(1).min(max_scroll);
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.state.help_scroll = self.state.help_scroll.saturating_sub(1);
                }
                KeyCode::Home | KeyCode::Char('g') => {
                    self.state.help_scroll = 0;
                }
                KeyCode::End | KeyCode::Char('G') => {
                    self.state.help_scroll = max_scroll;
                }
                _ => {}
            }
            return;
        }

        // ── Error popup — only Esc / Enter dismiss it so accidental keys
        // don't silently swallow the message before the user has read it.
        if self.state.error_message.is_some() {
            if matches!(key.code, KeyCode::Esc | KeyCode::Enter) {
                self.state.clear_error();
            }
            return;
        }

        // ── Grid search prompt mode ───────────────────────────────────────
        // When Ctrl+F is active on a data-grid tab, we intercept every key
        // for the search buffer instead of running the regular bindings.
        if self.state.grid_search_editing {
            match key.code {
                KeyCode::Esc => {
                    // Cancel search entirely — clears the highlight.
                    self.state.grid_search = None;
                    self.state.grid_search_editing = false;
                }
                KeyCode::Enter => {
                    // Commit the query; highlights stay and n/N navigate.
                    self.state.grid_search_editing = false;
                    self.jump_to_next_match(true);
                }
                KeyCode::Backspace => {
                    if let Some(q) = self.state.grid_search.as_mut() {
                        q.pop();
                    }
                }
                KeyCode::Char(ch) => {
                    if let Some(q) = self.state.grid_search.as_mut() {
                        q.push(ch);
                    }
                }
                _ => {}
            }
            return;
        }

        // ── SQL input mode ────────────────────────────────────────────────
        if self.state.focus == FocusPanel::SqlInput {
            match key.code {
                KeyCode::Esc => {
                    self.state.focus = FocusPanel::Main;
                }
                KeyCode::Enter => {
                    self.execute_sql().await;
                }
                KeyCode::Tab => {
                    self.complete_sql_input();
                }
                KeyCode::Up => {
                    if self.state.history_prev() {
                        if let Some(sql) = self.state.current_history_sql() {
                            self.sql_input.set(sql.to_string());
                        }
                    }
                }
                KeyCode::Down => match self.state.history_next() {
                    HistoryAdvance::Moved => {
                        if let Some(sql) = self.state.current_history_sql() {
                            self.sql_input.set(sql.to_string());
                        }
                    }
                    HistoryAdvance::Cleared => {
                        self.sql_input.clear();
                    }
                    HistoryAdvance::Unchanged => {}
                },
                KeyCode::Left => self.sql_input.move_left(),
                KeyCode::Right => self.sql_input.move_right(),
                KeyCode::Home => self.sql_input.home(),
                KeyCode::End => self.sql_input.end(),
                KeyCode::Backspace => {
                    self.sql_input.backspace();
                    self.state.history_cursor = None;
                }
                KeyCode::Delete => {
                    self.sql_input.delete();
                    self.state.history_cursor = None;
                }
                KeyCode::Char(ch) => {
                    self.sql_input.insert(ch);
                    // Any edit drops the user out of "browsing history"
                    // mode so ↓ no longer snaps back to the old entry.
                    self.state.history_cursor = None;
                }
                _ => {}
            }
            return;
        }

        // ── Global bindings (not in SQL input mode) ───────────────────────
        match key.code {
            // Quit
            KeyCode::Char('q') | KeyCode::Char('Q') => {
                self.state.should_quit = true;
                return;
            }

            // Help overlay
            KeyCode::Char('?') => {
                self.state.show_help = !self.state.show_help;
                self.state.help_scroll = 0;
                return;
            }

            // Tab cycling
            KeyCode::Tab => {
                self.state.current_tab = self.state.current_tab.next();
                self.state.focus = FocusPanel::Main;
                return;
            }
            KeyCode::BackTab => {
                self.state.current_tab = self.state.current_tab.prev();
                self.state.focus = FocusPanel::Main;
                return;
            }

            // Direct tab jump
            KeyCode::Char('1') => {
                self.state.current_tab = Tab::Tables;
                return;
            }
            KeyCode::Char('2') => {
                self.state.current_tab = Tab::Sql;
                return;
            }
            KeyCode::Char('3') => {
                self.state.current_tab = Tab::Logs;
                return;
            }
            KeyCode::Char('4') => {
                self.state.current_tab = Tab::Metrics;
                return;
            }
            KeyCode::Char('5') => {
                self.state.current_tab = Tab::Module;
                return;
            }
            KeyCode::Char('6') => {
                self.state.current_tab = Tab::Live;
                return;
            }

            // Sidebar focus: h/← steps up Tables → Databases; l/→ moves
            // focus over into the main pane.
            KeyCode::Left | KeyCode::Char('h') if self.state.focus == FocusPanel::Sidebar => {
                if self.state.sidebar_focus == SidebarFocus::Tables {
                    self.state.sidebar_focus = SidebarFocus::Databases;
                }
                return;
            }
            KeyCode::Right | KeyCode::Char('l') if self.state.focus == FocusPanel::Sidebar => {
                self.state.focus = FocusPanel::Main;
                return;
            }

            // Main focus: h/← and l/→ move the cell cursor inside a data
            // grid (Tables or SQL tabs). Use Esc to drop back to sidebar.
            KeyCode::Left | KeyCode::Char('h')
                if self.state.focus == FocusPanel::Main
                    && matches!(self.state.current_tab, Tab::Tables | Tab::Sql) =>
            {
                let grid = if self.state.current_tab == Tab::Tables {
                    &mut self.tables_grid
                } else {
                    &mut self.sql_grid
                };
                grid.prev_col();
                return;
            }
            KeyCode::Right | KeyCode::Char('l')
                if self.state.focus == FocusPanel::Main
                    && matches!(self.state.current_tab, Tab::Tables | Tab::Sql) =>
            {
                let col_count = if self.state.current_tab == Tab::Tables {
                    self.state
                        .table_browse_result
                        .as_ref()
                        .map(|qr| qr.column_count())
                        .unwrap_or(0)
                } else {
                    self.state
                        .query_result
                        .as_ref()
                        .map(|qr| qr.column_count())
                        .unwrap_or(0)
                };
                let grid = if self.state.current_tab == Tab::Tables {
                    &mut self.tables_grid
                } else {
                    &mut self.sql_grid
                };
                grid.next_col(col_count);
                return;
            }

            // Enter SQL mode
            KeyCode::Char(':') => {
                self.state.current_tab = Tab::Sql;
                self.state.focus = FocusPanel::SqlInput;
                return;
            }

            // Search / filter (sidebar)
            KeyCode::Char('/') => {
                // Toggle search mode — simple: enter a char into search_query
                if self.state.search_query.is_empty() {
                    self.state.focus = FocusPanel::Sidebar;
                } else {
                    self.state.search_query.clear();
                }
                return;
            }

            // Refresh current view
            KeyCode::Char('r') => {
                self.refresh_current_view().await;
                return;
            }

            // Navigation — delegate to focus owner
            KeyCode::Char('j') | KeyCode::Down => {
                self.nav_down().await;
                return;
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.nav_up();
                return;
            }
            KeyCode::Char('g') | KeyCode::Home => {
                self.nav_home();
                return;
            }
            KeyCode::Char('G') | KeyCode::End => {
                self.nav_end();
                return;
            }

            // Enter / select
            KeyCode::Enter => {
                self.nav_enter().await;
                return;
            }

            // Escape — multi-level "go back":
            //   1. clear an active search-as-you-type query, else
            //   2. if sidebar focus is on Tables, step back up to Databases, else
            //   3. snap keyboard focus from the main pane back to the sidebar.
            KeyCode::Esc => {
                if !self.state.search_query.is_empty() {
                    self.state.search_query.clear();
                } else if self.state.focus == FocusPanel::Sidebar
                    && self.state.sidebar_focus == SidebarFocus::Tables
                {
                    self.state.sidebar_focus = SidebarFocus::Databases;
                } else {
                    self.state.focus = FocusPanel::Sidebar;
                }
                return;
            }

            // Clipboard — `y` yanks the currently selected cell, `Y`
            // yanks the whole row (TSV-joined). Works on the data-grid
            // tabs (Tables / SQL) when focus is in the main pane.
            KeyCode::Char('y')
                if self.state.focus == FocusPanel::Main
                    && matches!(self.state.current_tab, Tab::Tables | Tab::Sql) =>
            {
                self.copy_selected_cell();
                return;
            }
            KeyCode::Char('Y')
                if self.state.focus == FocusPanel::Main
                    && matches!(self.state.current_tab, Tab::Tables | Tab::Sql) =>
            {
                self.copy_selected_row();
                return;
            }

            // Insert row (Tables tab only). Opens a form prefilled
            // with the current schema's columns. Submit issues an
            // INSERT INTO ... VALUES (...) SQL statement.
            KeyCode::Char('i')
                if self.state.focus == FocusPanel::Main
                    && self.state.current_tab == Tab::Tables =>
            {
                self.open_insert_form();
                return;
            }

            // Delete row (Tables tab only). Opens a y/n confirm dialog
            // that issues a DELETE FROM ... WHERE pk = ... statement.
            // PK is heuristically the first column of the table.
            KeyCode::Char('d')
                if self.state.focus == FocusPanel::Main
                    && self.state.current_tab == Tab::Tables =>
            {
                self.open_delete_confirm();
                return;
            }

            // Update row (Tables tab only). Opens an edit form
            // prefilled with the current row's values. The first
            // field is the PK display; submit issues an UPDATE.
            KeyCode::Char('U')
                if self.state.focus == FocusPanel::Main
                    && self.state.current_tab == Tab::Tables =>
            {
                self.open_update_form();
                return;
            }

            // ── Destructive admin ops (typed-confirm forms) ───────
            // Shift+D on the Tables tab → truncate the selected
            // table (DELETE FROM <table>). Same key on the sidebar
            // when focused on the Databases panel → delete the
            // entire database via DELETE /v1/database/<name>.
            KeyCode::Char('D')
                if self.state.focus == FocusPanel::Main
                    && self.state.current_tab == Tab::Tables =>
            {
                self.open_truncate_table_form();
                return;
            }
            KeyCode::Char('D')
                if self.state.focus == FocusPanel::Sidebar
                    && self.state.sidebar_focus == SidebarFocus::Databases =>
            {
                self.open_delete_db_form();
                return;
            }

            // `a` on the Databases sidebar panel opens a form to
            // attach a new alias (human name) to the selected DB.
            // Non-destructive, no typed-confirm required.
            KeyCode::Char('a')
                if self.state.focus == FocusPanel::Sidebar
                    && self.state.sidebar_focus == SidebarFocus::Databases =>
            {
                self.open_add_alias_form();
                return;
            }

            // Sort — `s` cycles the sort state (off → asc → desc → off)
            // on the currently-selected column.
            KeyCode::Char('s')
                if self.state.focus == FocusPanel::Main
                    && matches!(self.state.current_tab, Tab::Tables | Tab::Sql) =>
            {
                let col_count = self
                    .active_grid()
                    .map(|(qr, _)| qr.column_count())
                    .unwrap_or(0);
                if col_count == 0 {
                    return;
                }
                // Snapshot the underlying data row the cursor points at
                // *before* mutating the sort state — we'll translate it
                // back into a display row after the permutation changes
                // so the cursor appears to stay on the "same" record
                // instead of jumping to a random position.
                let anchor_data_row = self.active_data_row_index();
                let grid = if self.state.current_tab == Tab::Tables {
                    &mut self.tables_grid
                } else {
                    &mut self.sql_grid
                };
                grid.cycle_sort(grid.selected_col);

                // Re-map the anchor through the new permutation. If
                // anything goes sideways (empty rows, missing anchor)
                // the cursor stays where it was — no worse than the
                // old behaviour.
                if let Some(data_idx) = anchor_data_row {
                    if let Some(new_display) = self.display_row_for_data_idx(data_idx) {
                        let grid = if self.state.current_tab == Tab::Tables {
                            &mut self.tables_grid
                        } else {
                            &mut self.sql_grid
                        };
                        grid.selected_row = new_display;
                    }
                }
                let col_name = self
                    .active_grid()
                    .and_then(|(qr, g)| {
                        qr.column_names().get(g.selected_col).map(|s| s.to_string())
                    })
                    .unwrap_or_default();
                let dir = match (
                    self.active_grid().map(|(_, g)| g.sort_col),
                    self.active_grid().map(|(_, g)| g.sort_desc),
                ) {
                    (Some(Some(_)), Some(false)) => "asc",
                    (Some(Some(_)), Some(true)) => "desc",
                    _ => "off",
                };
                self.state
                    .set_notification(format!("Sort {col_name} {dir}"));
                return;
            }

            // Export — `e` writes a CSV, `E` writes a JSON file under
            // `./exports/` for the currently visible query result.
            KeyCode::Char('e')
                if self.state.focus == FocusPanel::Main
                    && matches!(self.state.current_tab, Tab::Tables | Tab::Sql) =>
            {
                self.export_current_result(crate::ui::export::ExportFormat::Csv);
                return;
            }
            KeyCode::Char('E')
                if self.state.focus == FocusPanel::Main
                    && matches!(self.state.current_tab, Tab::Tables | Tab::Sql) =>
            {
                self.export_current_result(crate::ui::export::ExportFormat::Json);
                return;
            }

            // Log-specific
            KeyCode::Char(' ') if self.state.current_tab == Tab::Logs => {
                self.state.log_follow = !self.state.log_follow;
                return;
            }
            KeyCode::Char('c') if self.state.current_tab == Tab::Logs => {
                self.state.log_buffer.clear();
                self.state.log_scroll = 0;
                return;
            }
            KeyCode::Char('f') if self.state.current_tab == Tab::Logs => {
                self.state.log_filter_level = self.state.log_filter_level.clone().next_filter();
                self.state
                    .set_notification(format!("Log filter: {}", self.state.log_filter_level));
                return;
            }

            // `n` / `N` on data-grid tabs: jump to next / previous
            // search match when a search query is active, otherwise
            // fall through to page-scroll on the Tables tab.
            KeyCode::Char('n')
                if matches!(self.state.current_tab, Tab::Tables | Tab::Sql)
                    && self.state.grid_search.is_some() =>
            {
                self.jump_to_next_match(true);
                return;
            }
            KeyCode::Char('N')
                if matches!(self.state.current_tab, Tab::Tables | Tab::Sql)
                    && self.state.grid_search.is_some() =>
            {
                self.jump_to_next_match(false);
                return;
            }

            // Page navigation in tables (no active search)
            KeyCode::Char('n') if self.state.current_tab == Tab::Tables => {
                self.tables_grid.scroll_row = self.tables_grid.scroll_row.saturating_add(20);
                return;
            }
            KeyCode::Char('p') if self.state.current_tab == Tab::Tables => {
                self.tables_grid.scroll_row = self.tables_grid.scroll_row.saturating_sub(20);
                return;
            }

            // Horizontal scroll in table/SQL results (< / > or H / L)
            KeyCode::Char('<') | KeyCode::Char('H')
                if matches!(self.state.current_tab, Tab::Tables | Tab::Sql) =>
            {
                let grid = if self.state.current_tab == Tab::Tables {
                    &mut self.tables_grid
                } else {
                    &mut self.sql_grid
                };
                grid.scroll_left();
                return;
            }
            KeyCode::Char('>') | KeyCode::Char('L')
                if matches!(self.state.current_tab, Tab::Tables | Tab::Sql) =>
            {
                let (col_count, grid) = if self.state.current_tab == Tab::Tables {
                    let cc = self
                        .state
                        .table_browse_result
                        .as_ref()
                        .map(|qr| qr.column_count())
                        .unwrap_or(0);
                    (cc, &mut self.tables_grid)
                } else {
                    let cc = self
                        .state
                        .query_result
                        .as_ref()
                        .map(|qr| qr.column_count())
                        .unwrap_or(0);
                    (cc, &mut self.sql_grid)
                };
                grid.scroll_right(col_count);
                return;
            }

            // Search input (when in sidebar search mode) — also acts as
            // "step up" when there's no search text and the user is on the
            // Tables sub-panel.
            KeyCode::Backspace if self.state.focus == FocusPanel::Sidebar => {
                if !self.state.search_query.is_empty() {
                    self.state.search_query.pop();
                } else if self.state.sidebar_focus == SidebarFocus::Tables {
                    self.state.sidebar_focus = SidebarFocus::Databases;
                }
                return;
            }
            KeyCode::Char(ch)
                if self.state.focus == FocusPanel::Sidebar && !ch.is_ascii_control() =>
            {
                // In sidebar, typing filters the list
                self.state.search_query.push(ch);
                return;
            }

            _ => {}
        }
    }

    // ── Navigation helpers ────────────────────────────────────────────────

    async fn nav_down(&mut self) {
        match self.state.focus {
            FocusPanel::Sidebar => match self.state.sidebar_focus {
                SidebarFocus::Databases => {
                    let old = self.state.selected_database_idx;
                    self.state.database_next();
                    if self.state.selected_database_idx != old {
                        self.load_schema().await;
                    }
                }
                SidebarFocus::Tables => {
                    self.state.table_next();
                }
            },
            FocusPanel::Main => match self.state.current_tab {
                Tab::Tables => {
                    let row_count = self
                        .state
                        .table_browse_result
                        .as_ref()
                        .map(|qr| qr.row_count())
                        .unwrap_or(0);
                    self.tables_grid.next_row(row_count);
                }
                Tab::Sql => {
                    let row_count = self
                        .state
                        .query_result
                        .as_ref()
                        .map(|qr| qr.row_count())
                        .unwrap_or(0);
                    self.sql_grid.next_row(row_count);
                }
                Tab::Logs => {
                    if !self.state.log_follow {
                        self.state.log_scroll = self
                            .state
                            .log_scroll
                            .saturating_add(1)
                            .min(self.state.log_buffer.len().saturating_sub(1));
                    }
                }
                Tab::Module => {
                    let count = self
                        .state
                        .current_schema
                        .as_ref()
                        .map(|s| s.reducers.len())
                        .unwrap_or(0);
                    if count > 0 {
                        self.state.module_selected_reducer =
                            (self.state.module_selected_reducer + 1).min(count - 1);
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    fn nav_up(&mut self) {
        match self.state.focus {
            FocusPanel::Sidebar => match self.state.sidebar_focus {
                SidebarFocus::Databases => {
                    self.state.database_prev();
                }
                SidebarFocus::Tables => {
                    self.state.table_prev();
                }
            },
            FocusPanel::Main => match self.state.current_tab {
                Tab::Tables => {
                    self.tables_grid.prev_row();
                }
                Tab::Sql => {
                    self.sql_grid.prev_row();
                }
                Tab::Logs => {
                    if !self.state.log_follow {
                        self.state.log_scroll = self.state.log_scroll.saturating_sub(1);
                    }
                }
                Tab::Module => {
                    self.state.module_selected_reducer =
                        self.state.module_selected_reducer.saturating_sub(1);
                }
                _ => {}
            },
            _ => {}
        }
    }

    fn nav_home(&mut self) {
        match self.state.focus {
            FocusPanel::Sidebar => {
                if let SidebarFocus::Tables = self.state.sidebar_focus {
                    self.state.selected_table_idx = if self.state.tables.is_empty() {
                        None
                    } else {
                        Some(0)
                    };
                }
            }
            FocusPanel::Main => match self.state.current_tab {
                Tab::Tables => {
                    self.tables_grid.selected_row = 0;
                    self.tables_grid.scroll_row = 0;
                }
                Tab::Sql => {
                    self.sql_grid.selected_row = 0;
                    self.sql_grid.scroll_row = 0;
                }
                Tab::Logs => {
                    self.state.log_scroll = 0;
                    self.state.log_follow = false;
                }
                _ => {}
            },
            _ => {}
        }
    }

    fn nav_end(&mut self) {
        if self.state.focus == FocusPanel::Main {
            match self.state.current_tab {
                Tab::Tables => {
                    if let Some(ref qr) = self.state.table_browse_result {
                        let count = qr.row_count();
                        self.tables_grid.selected_row = count.saturating_sub(1);
                    }
                }
                Tab::Sql => {
                    if let Some(ref qr) = self.state.query_result {
                        let count = qr.row_count();
                        self.sql_grid.selected_row = count.saturating_sub(1);
                    }
                }
                Tab::Logs => {
                    self.state.log_follow = true;
                }
                _ => {}
            }
        }
    }

    async fn nav_enter(&mut self) {
        match self.state.focus {
            FocusPanel::Sidebar => {
                match self.state.sidebar_focus {
                    SidebarFocus::Databases => {
                        // Move focus to tables
                        self.state.sidebar_focus = SidebarFocus::Tables;
                        if !self.state.tables.is_empty() && self.state.selected_table_idx.is_none()
                        {
                            self.state.selected_table_idx = Some(0);
                        }
                    }
                    SidebarFocus::Tables => {
                        // Load the selected table's data
                        self.load_table_data().await;
                        self.state.focus = FocusPanel::Main;
                        self.state.current_tab = Tab::Tables;
                        self.tables_grid = TableGridState::new();
                    }
                }
            }
            FocusPanel::Main => {
                if self.state.current_tab == Tab::Sql {
                    self.state.focus = FocusPanel::SqlInput;
                } else if self.state.current_tab == Tab::Module {
                    // Enter on a reducer in the module inspector opens
                    // a call form (or a no-arg confirm, when the
                    // reducer has no parameters).
                    self.open_reducer_form();
                }
            }
            _ => {}
        }
    }

    // ── Data loading ──────────────────────────────────────────────────────

    async fn load_schema(&mut self) {
        let db = match self.state.selected_database() {
            Some(d) => d.to_string(),
            None => return,
        };
        self.state.tables.clear();
        self.state.selected_table_idx = None;
        self.state.current_schema = None;
        // Track the in-flight schema fetch so the sidebar can show
        // a real loading spinner and clear it on both success and
        // failure (fixes the "stuck on (loading…)" bug after HTTP
        // 500s).
        self.state.schema_loading = true;
        self.state.schema_load_failed = false;

        let client = self.client.clone();
        let tx = self.event_tx.clone();
        tokio::spawn(async move {
            match tokio::time::timeout(HTTP_REQUEST_TIMEOUT, client.get_schema(&db)).await {
                Ok(Ok(schema)) => send_event(&tx, AppEvent::SchemaLoaded(schema)),
                Ok(Err(e)) => {
                    // A paused database is a distinct, expected state — flag
                    // the database rather than reporting a generic failure.
                    if let Some(paused) = e.downcast_ref::<crate::api::client::DatabasePaused>() {
                        send_event(
                            &tx,
                            AppEvent::SchemaPaused {
                                database: db.clone(),
                                message: paused.to_string(),
                            },
                        );
                    } else {
                        send_event(
                            &tx,
                            AppEvent::SchemaError(format!("Schema load failed: {e:#}")),
                        );
                    }
                }
                Err(_) => send_event(
                    &tx,
                    AppEvent::SchemaError("Schema load timed out".to_string()),
                ),
            }
        });
    }

    async fn load_table_data(&mut self) {
        let db = match self.state.selected_database() {
            Some(d) => d.to_string(),
            None => return,
        };
        let table = match self.state.selected_table() {
            Some(t) => t.table_name.clone(),
            None => return,
        };

        self.state.query_loading = true;
        self.state.table_browse_result = None;

        let sql = format!("SELECT * FROM {table} LIMIT 200");
        let client = self.client.clone();
        let tx = self.event_tx.clone();

        tokio::spawn(async move {
            match tokio::time::timeout(HTTP_REQUEST_TIMEOUT, client.query_sql(&db, &sql)).await {
                Ok(Ok(result)) => send_event(&tx, AppEvent::TableBrowseResult { result }),
                Ok(Err(e)) => send_event(
                    &tx,
                    AppEvent::TableBrowseError {
                        error: format!("{e:#}"),
                    },
                ),
                Err(_) => send_event(
                    &tx,
                    AppEvent::TableBrowseError {
                        error: "table load timed out".to_string(),
                    },
                ),
            }
        });
    }

    /// Return a reference to the `QueryResult` / `TableGridState` pair
    /// that backs the currently focused data-grid tab, together with
    /// the table-name hint (if any) used for notifications.
    fn active_grid(&self) -> Option<(&crate::api::types::QueryResult, &TableGridState)> {
        match self.state.current_tab {
            Tab::Tables => self
                .state
                .table_browse_result
                .as_ref()
                .map(|qr| (qr, &self.tables_grid)),
            Tab::Sql => self
                .state
                .query_result
                .as_ref()
                .map(|qr| (qr, &self.sql_grid)),
            _ => None,
        }
    }

    /// Translate a grid's `selected_row` (which is in display order
    /// when a sort is active) back to the underlying `QueryResult.rows`
    /// index, so clipboard / export operations read the cells the user
    /// is actually looking at.
    fn active_data_row_index(&self) -> Option<usize> {
        let (qr, grid) = self.active_grid()?;
        // Re-project the rows into the same `Vec<Vec<String>>` that the
        // renderer sorts, then ask `sorted_data_index` for the mapping.
        let string_rows: Vec<Vec<String>> = qr
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(crate::ui::tabs::tables::value_to_display)
                    .collect()
            })
            .collect();
        crate::ui::components::table_grid::sorted_data_index(
            &string_rows,
            grid.sort_col,
            grid.sort_desc,
            grid.selected_row,
        )
    }

    /// Reverse of [`active_data_row_index`]: given an underlying
    /// `data_idx` (stable across sort permutations), return the
    /// display row index it currently lives at under the active
    /// grid's sort state. Used by the `s` key binding to keep the
    /// cell cursor anchored to the same record when the permutation
    /// changes.
    fn display_row_for_data_idx(&self, data_idx: usize) -> Option<usize> {
        let (qr, grid) = self.active_grid()?;
        if qr.rows.is_empty() || data_idx >= qr.rows.len() {
            return None;
        }
        let Some(sort_col) = grid.sort_col else {
            return Some(data_idx);
        };
        let string_rows: Vec<Vec<String>> = qr
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(crate::ui::tabs::tables::value_to_display)
                    .collect()
            })
            .collect();
        // Rebuild the permutation the renderer uses and scan for
        // the display index whose mapped data index matches.
        // O(n²) in the worst case but data grids cap at a few
        // hundred rows — acceptable for a one-shot key event.
        (0..string_rows.len()).find(|&display_idx| {
            crate::ui::components::table_grid::sorted_data_index(
                &string_rows,
                Some(sort_col),
                grid.sort_desc,
                display_idx,
            ) == Some(data_idx)
        })
    }

    /// Copy the currently-highlighted cell to the terminal clipboard.
    fn copy_selected_cell(&mut self) {
        let cell_text = {
            let data_idx = match self.active_data_row_index() {
                Some(i) => i,
                None => return,
            };
            let Some((qr, grid)) = self.active_grid() else {
                return;
            };
            let row = match qr.rows.get(data_idx) {
                Some(r) => r,
                None => return,
            };
            let value = match row.get(grid.selected_col) {
                Some(v) => v,
                None => return,
            };
            crate::ui::tabs::tables::value_to_display(value)
        };

        match crate::ui::clipboard::copy_to_clipboard(&cell_text) {
            Ok(n) => {
                let preview: String = cell_text.chars().take(40).collect();
                self.state
                    .set_notification(format!("Copied {n}B: {preview}"));
            }
            Err(e) => {
                tracing::warn!("clipboard copy failed: {e}");
                self.state.set_error(format!("Clipboard copy failed: {e}"));
            }
        }
    }

    /// Copy the currently-selected row to the terminal clipboard as a
    /// TSV (tab-separated values) line.
    fn copy_selected_row(&mut self) {
        let (row_text, col_count) = {
            let data_idx = match self.active_data_row_index() {
                Some(i) => i,
                None => return,
            };
            let Some((qr, _grid)) = self.active_grid() else {
                return;
            };
            let row = match qr.rows.get(data_idx) {
                Some(r) => r,
                None => return,
            };
            let tsv = row
                .iter()
                .map(crate::ui::tabs::tables::value_to_display)
                .collect::<Vec<_>>()
                .join("\t");
            (tsv, row.len())
        };

        match crate::ui::clipboard::copy_to_clipboard(&row_text) {
            Ok(n) => {
                self.state
                    .set_notification(format!("Copied row ({col_count} cells, {n}B)"));
            }
            Err(e) => {
                tracing::warn!("clipboard copy failed: {e}");
                self.state.set_error(format!("Clipboard copy failed: {e}"));
            }
        }
    }

    /// Move the cell cursor to the next (or previous, if `forward` is
    /// `false`) row that contains a match for the current grid search
    /// query. Wraps around the end of the result set.
    ///
    /// A "match" is any cell whose string representation contains the
    /// query as a case-insensitive substring. Used by Enter on the
    /// search prompt and by `n` / `N` afterwards.
    fn jump_to_next_match(&mut self, forward: bool) {
        let query = match self.state.grid_search.as_ref() {
            Some(q) if !q.is_empty() => q.to_ascii_lowercase(),
            _ => return,
        };

        // Snapshot the rows we're searching so we can release the
        // immutable borrow on `state` before mutating the grid.
        let rows: Vec<Vec<String>> = {
            let qr = match self.state.current_tab {
                Tab::Tables => self.state.table_browse_result.as_ref(),
                Tab::Sql => self.state.query_result.as_ref(),
                _ => return,
            };
            let Some(qr) = qr else {
                return;
            };
            qr.rows
                .iter()
                .map(|row| {
                    row.iter()
                        .map(crate::ui::tabs::tables::value_to_display)
                        .collect()
                })
                .collect()
        };

        if rows.is_empty() {
            self.state.set_notification("No rows to search".to_string());
            return;
        }

        let grid = if self.state.current_tab == Tab::Tables {
            &mut self.tables_grid
        } else {
            &mut self.sql_grid
        };

        // Walk display order (which is `rows` when unsorted, or the
        // sort permutation when a sort is active) so `n` / `N`
        // visually steps by one row on screen each time.
        let order: Vec<usize> = match grid.sort_col {
            Some(col) => {
                let mut idxs: Vec<usize> = (0..rows.len()).collect();
                idxs.sort_by(|&a, &b| {
                    let av = rows[a].get(col).map(String::as_str).unwrap_or("");
                    let bv = rows[b].get(col).map(String::as_str).unwrap_or("");
                    // Replicate `compare_cells` locally so we don't have
                    // to expose it outside `table_grid`.
                    match (av.parse::<f64>(), bv.parse::<f64>()) {
                        (Ok(na), Ok(nb)) => {
                            na.partial_cmp(&nb).unwrap_or(std::cmp::Ordering::Equal)
                        }
                        _ => av.to_ascii_lowercase().cmp(&bv.to_ascii_lowercase()),
                    }
                });
                if grid.sort_desc {
                    idxs.reverse();
                }
                idxs
            }
            None => (0..rows.len()).collect(),
        };

        let n = order.len();
        let start = grid.selected_row.min(n - 1);
        for step in 1..=n {
            let display_idx = if forward {
                (start + step) % n
            } else {
                (start + n - step) % n
            };
            let data_idx = order[display_idx];
            if rows[data_idx]
                .iter()
                .any(|cell| cell.to_ascii_lowercase().contains(&query))
            {
                grid.selected_row = display_idx;
                return;
            }
        }
        self.state
            .set_notification(format!("No match for \"{query}\""));
    }

    /// Serialise the currently visible query result to CSV or JSON and
    /// write it under `./exports/`. Shows the resulting path in the
    /// status bar notification so the user can `cat` / open it.
    fn export_current_result(&mut self, format: crate::ui::export::ExportFormat) {
        let (qr, label) = match self.state.current_tab {
            Tab::Tables => {
                let qr = match self.state.table_browse_result.as_ref() {
                    Some(qr) => qr,
                    None => {
                        self.state.set_notification("Nothing to export".to_string());
                        return;
                    }
                };
                let label = self
                    .state
                    .selected_table()
                    .map(|t| t.table_name.clone())
                    .unwrap_or_else(|| "table".to_string());
                (qr.clone(), label)
            }
            Tab::Sql => {
                let qr = match self.state.query_result.as_ref() {
                    Some(qr) => qr,
                    None => {
                        self.state.set_notification("Nothing to export".to_string());
                        return;
                    }
                };
                (qr.clone(), "query".to_string())
            }
            _ => return,
        };

        match crate::ui::export::write_export(&qr, format, &label) {
            Ok(path) => {
                self.state
                    .set_notification(format!("Exported to {}", path.display()));
            }
            Err(e) => {
                tracing::warn!("export failed: {e:#}");
                self.state.set_error(format!("Export failed: {e:#}"));
            }
        }
    }

    // ── Spreadsheet edit mode (Faz 10) ───────────────────────────────────

    /// Open spreadsheet edit mode on the Tables tab. No-op if the
    /// user hasn't loaded any table data yet (nothing to edit).
    fn enter_edit_mode(&mut self) {
        if self.state.table_browse_result.is_none() {
            self.state
                .set_notification("Nothing to edit — load a table first".to_string());
            return;
        }
        if self.state.selected_table().is_none() {
            self.state.set_notification("No table selected".to_string());
            return;
        }
        self.state.edit_mode = Some(crate::state::edit_mode::EditMode::new());
        self.state
            .set_notification("EDIT MODE — Ctrl+E to exit".to_string());
    }

    /// Leave edit mode. If there are uncommitted edits, pops up a
    /// confirm dialog so the user doesn't lose them by accident.
    fn exit_edit_mode(&mut self) {
        let pending = self
            .state
            .edit_mode
            .as_ref()
            .map(|em| em.pending_count())
            .unwrap_or(0);
        if pending == 0 {
            self.state.edit_mode = None;
            return;
        }
        // Pending changes — ask before discarding. We reuse the
        // existing Confirm modal; its action is ignored because we
        // handle cancel vs confirm inline via a sentinel.
        self.state.modal = Some(crate::state::modal::Modal::confirm(
            "Discard pending edits?",
            format!(
                "You have {pending} uncommitted cell edit(s).\n\
                 Leaving edit mode without saving will drop them.\n\n\
                 Press [y] to discard, [n] to stay in edit mode."
            ),
            crate::state::modal::ModalAction::DiscardPendingEdits,
        ));
    }

    /// Route a key event through the edit-mode key map.
    async fn handle_edit_mode_key(&mut self, key: KeyEvent) {
        // If the inline cell editor is open, keystrokes go into the
        // input buffer instead of the outer key map.
        let editor_open = self
            .state
            .edit_mode
            .as_ref()
            .map(|em| em.editor.is_some())
            .unwrap_or(false);
        if editor_open {
            self.handle_cell_editor_key(key);
            return;
        }

        match key.code {
            KeyCode::Esc => {
                self.exit_edit_mode();
            }
            KeyCode::Enter | KeyCode::Char('i') => {
                self.begin_cell_edit();
            }
            KeyCode::Char('s') => {
                self.save_pending_edits().await;
            }
            KeyCode::Char('u') => {
                self.revert_active_cell();
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.tables_grid.prev_col();
            }
            KeyCode::Right | KeyCode::Char('l') => {
                let cc = self
                    .state
                    .table_browse_result
                    .as_ref()
                    .map(|qr| qr.column_count())
                    .unwrap_or(0);
                self.tables_grid.next_col(cc);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.tables_grid.prev_row();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let rc = self
                    .state
                    .table_browse_result
                    .as_ref()
                    .map(|qr| qr.row_count())
                    .unwrap_or(0);
                self.tables_grid.next_row(rc);
            }
            _ => {}
        }
    }

    /// Keystrokes routed into the inline cell editor's `InputState`.
    fn handle_cell_editor_key(&mut self, key: KeyEvent) {
        let Some(em) = self.state.edit_mode.as_mut() else {
            return;
        };
        let Some(editor) = em.editor.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => {
                em.editor = None;
            }
            KeyCode::Enter => {
                self.commit_cell_edit();
            }
            KeyCode::Left => editor.move_left(),
            KeyCode::Right => editor.move_right(),
            KeyCode::Home => editor.home(),
            KeyCode::End => editor.end(),
            KeyCode::Backspace => editor.backspace(),
            KeyCode::Delete => editor.delete(),
            KeyCode::Char(ch) => editor.insert(ch),
            _ => {}
        }
    }

    /// Open the inline editor over the currently selected cell,
    /// pre-filled with the cell's current display value (or any
    /// existing pending edit value).
    fn begin_cell_edit(&mut self) {
        // Figure out the data row / column, plus whether the user is
        // trying to edit the PK (which we refuse so the WHERE clause
        // stays valid).
        let data_row = match self.active_data_row_index() {
            Some(i) => i,
            None => return,
        };
        let col_idx = self.tables_grid.selected_col;
        let table = match self.state.selected_table().cloned() {
            Some(t) => t,
            None => return,
        };
        if table.columns.is_empty() {
            return;
        }
        let (pk_idx, _) = pick_primary_key(&table);
        if col_idx == pk_idx {
            self.state
                .set_notification("PK column is read-only in edit mode".to_string());
            return;
        }

        // Prefer the in-flight pending value if one exists, otherwise
        // fall back to the original cell string.
        let cell_text = {
            let em = self.state.edit_mode.as_ref();
            let pending = em.and_then(|e| e.find(data_row, col_idx));
            if let Some(pe) = pending {
                pe.new_value.clone()
            } else {
                self.state
                    .table_browse_result
                    .as_ref()
                    .and_then(|qr| qr.rows.get(data_row))
                    .and_then(|row| row.get(col_idx))
                    .map(crate::ui::tabs::tables::value_to_display)
                    .unwrap_or_default()
            }
        };

        let Some(em) = self.state.edit_mode.as_mut() else {
            return;
        };
        let mut input = InputState::new();
        input.set(cell_text);
        em.editor = Some(input);
    }

    /// Commit whatever's in the inline editor into the pending-edits
    /// list and close the editor.
    fn commit_cell_edit(&mut self) {
        let data_row = match self.active_data_row_index() {
            Some(i) => i,
            None => return,
        };
        let col_idx = self.tables_grid.selected_col;

        // Capture the original display value before we touch `edit_mode`.
        let original = self
            .state
            .table_browse_result
            .as_ref()
            .and_then(|qr| qr.rows.get(data_row))
            .and_then(|row| row.get(col_idx))
            .map(crate::ui::tabs::tables::value_to_display)
            .unwrap_or_default();

        let Some(em) = self.state.edit_mode.as_mut() else {
            return;
        };
        let Some(editor) = em.editor.take() else {
            return;
        };
        em.upsert(data_row, col_idx, original, editor.value);
    }

    /// Drop the pending edit (if any) on the active cell.
    fn revert_active_cell(&mut self) {
        let data_row = match self.active_data_row_index() {
            Some(i) => i,
            None => return,
        };
        let col_idx = self.tables_grid.selected_col;
        let Some(em) = self.state.edit_mode.as_mut() else {
            return;
        };
        if em.revert(data_row, col_idx) {
            self.state.set_notification("Reverted".to_string());
        }
    }

    /// Flush every pending edit to the server as a sequence of
    /// `UPDATE <table> SET col=val WHERE pk=pk` statements, one per
    /// edit. On success clears the pending list and leaves edit
    /// mode; on partial failure keeps the failing entries.
    async fn save_pending_edits(&mut self) {
        // Gather everything we need while we still hold the state
        // borrow; then drop the borrow before spawning tasks.
        let (db, table, pk_idx, pk_column, column_types, rows, pending) = {
            let Some(db) = self.state.selected_database() else {
                self.state.set_error("No database selected".to_string());
                return;
            };
            let db = db.to_string();
            let Some(table) = self.state.selected_table().cloned() else {
                return;
            };
            let (pk_idx, pk_column) = pick_primary_key(&table);
            let column_types: Vec<String> = table
                .columns
                .iter()
                .map(|c| type_tag(&c.col_type))
                .collect();
            let rows: Vec<Vec<serde_json::Value>> = self
                .state
                .table_browse_result
                .as_ref()
                .map(|qr| qr.rows.clone())
                .unwrap_or_default();
            let pending: Vec<_> = self
                .state
                .edit_mode
                .as_ref()
                .map(|em| em.pending.clone())
                .unwrap_or_default();
            (
                db,
                table.table_name,
                pk_idx,
                pk_column,
                column_types,
                rows,
                pending,
            )
        };

        if pending.is_empty() {
            self.state.set_notification("No pending edits".to_string());
            return;
        }

        let mut spawned = 0usize;
        let mut skipped_errors: Vec<String> = Vec::new();
        for edit in &pending {
            let Some(row) = rows.get(edit.data_row_idx) else {
                continue;
            };
            let Some(col_type) = column_types.get(edit.col_idx) else {
                continue;
            };
            let col_name = self
                .state
                .selected_table()
                .and_then(|t| t.columns.get(edit.col_idx).map(|c| c.col_name.clone()))
                .unwrap_or_default();
            let new_literal = sql_literal(&edit.new_value, col_type);
            // Build the WHERE literal from the raw JSON value so
            // Identity / ConnectionId / U256 PKs round-trip
            // correctly. A complex-shaped PK (array, nested object
            // we don't recognise) is skipped with a per-edit error
            // so the rest of the batch still goes through.
            let pk_literal = match row.get(pk_idx).map(json_to_sql_literal) {
                Some(Ok(lit)) => lit,
                Some(Err(e)) => {
                    skipped_errors.push(format!("{col_name}: {e}"));
                    continue;
                }
                None => {
                    skipped_errors.push(format!("{col_name}: no PK value"));
                    continue;
                }
            };
            let sql = format!(
                "UPDATE {table} SET {col_name} = {new_literal} WHERE {pk_column} = {pk_literal}"
            );
            let op_label = format!("edit {table}.{col_name}");
            self.spawn_write_sql(db.clone(), sql, op_label);
            spawned += 1;
        }

        if !skipped_errors.is_empty() {
            self.state.set_error(format!(
                "Skipped {} edit(s): {}",
                skipped_errors.len(),
                skipped_errors.join("; ")
            ));
        }

        if spawned > 0 {
            self.state
                .set_notification(format!("Submitted {spawned} UPDATE statement(s)"));
            // Leave edit mode — the background spawn will re-fetch
            // the table data on success, and any failures surface
            // as `WriteOpError` notifications.
            self.state.edit_mode = None;
        }
    }

    // ── Command palette (Faz 6.3) ────────────────────────────────────────

    /// Route a key event into the active command palette overlay.
    /// Mirrors `handle_modal_key`'s "take, mutate, put back" pattern
    /// so we never hold two borrows on `state` at once.
    async fn handle_palette_key(&mut self, key: KeyEvent) {
        let Some(mut palette) = self.state.palette.take() else {
            return;
        };

        match key.code {
            KeyCode::Esc => {
                // Cancel — drop the palette entirely.
                return;
            }
            KeyCode::Enter => {
                if let Some(cmd) = palette.current() {
                    self.dispatch_command(cmd).await;
                }
                return;
            }
            KeyCode::Down | KeyCode::Tab => {
                let len = palette.filter().len();
                palette.next(len);
            }
            KeyCode::Up | KeyCode::BackTab => {
                palette.prev();
            }
            KeyCode::Backspace => {
                palette.query.backspace();
                palette.selected = 0;
            }
            KeyCode::Char(ch) => {
                palette.query.insert(ch);
                palette.selected = 0;
            }
            _ => {}
        }

        self.state.palette = Some(palette);
    }

    /// Run the action behind a [`Command`].
    async fn dispatch_command(&mut self, cmd: crate::state::palette::Command) {
        use crate::state::palette::Command as C;
        match cmd {
            C::GotoTables => self.state.current_tab = Tab::Tables,
            C::GotoSql => self.state.current_tab = Tab::Sql,
            C::GotoLogs => self.state.current_tab = Tab::Logs,
            C::GotoMetrics => self.state.current_tab = Tab::Metrics,
            C::GotoModule => self.state.current_tab = Tab::Module,
            C::GotoLive => self.state.current_tab = Tab::Live,
            C::RefreshCurrentView => self.refresh_current_view().await,
            C::ReconnectWebSocket => {
                self.connect_ws().await;
                self.state
                    .set_notification("Reconnecting WebSocket…".to_string());
            }
            C::ToggleHelp => {
                self.state.show_help = !self.state.show_help;
                self.state.help_scroll = 0;
            }
            C::ExportCsv => {
                self.export_current_result(crate::ui::export::ExportFormat::Csv);
            }
            C::ExportJson => {
                self.export_current_result(crate::ui::export::ExportFormat::Json);
            }
            C::CopyCell => self.copy_selected_cell(),
            C::CopyRow => self.copy_selected_row(),
            C::Quit => self.state.should_quit = true,
        }
    }

    // ── Modal dialogs (Faz 5: write operations) ──────────────────────────

    /// Route a key event into the active modal dialog. Called from
    /// `handle_key` when `state.modal.is_some()`.
    async fn handle_modal_key(&mut self, key: KeyEvent) {
        // Take the modal out of state so we can mutate its fields
        // freely without holding two borrows at the same time. We
        // put it back at the end unless the user accepted / cancelled.
        let Some(mut modal) = self.state.modal.take() else {
            return;
        };

        match &mut modal {
            crate::state::modal::Modal::Confirm { .. } => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.dispatch_modal_action(modal).await;
                    return;
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    // Cancelled — drop the modal entirely.
                    return;
                }
                _ => {}
            },
            crate::state::modal::Modal::Form { fields, focus, .. } => match key.code {
                KeyCode::Esc => {
                    return;
                }
                KeyCode::Enter => {
                    self.dispatch_modal_action(modal).await;
                    return;
                }
                KeyCode::Tab | KeyCode::Down => {
                    if !fields.is_empty() {
                        *focus = (*focus + 1) % fields.len();
                    }
                }
                KeyCode::BackTab | KeyCode::Up => {
                    if !fields.is_empty() {
                        *focus = if *focus == 0 {
                            fields.len() - 1
                        } else {
                            *focus - 1
                        };
                    }
                }
                KeyCode::Left => {
                    if let Some(f) = fields.get_mut(*focus) {
                        f.input.move_left();
                    }
                }
                KeyCode::Right => {
                    if let Some(f) = fields.get_mut(*focus) {
                        f.input.move_right();
                    }
                }
                KeyCode::Home => {
                    if let Some(f) = fields.get_mut(*focus) {
                        f.input.home();
                    }
                }
                KeyCode::End => {
                    if let Some(f) = fields.get_mut(*focus) {
                        f.input.end();
                    }
                }
                KeyCode::Backspace => {
                    if let Some(f) = fields.get_mut(*focus) {
                        f.input.backspace();
                    }
                }
                KeyCode::Delete => {
                    if let Some(f) = fields.get_mut(*focus) {
                        f.input.delete();
                    }
                }
                KeyCode::Char(ch) => {
                    if let Some(f) = fields.get_mut(*focus) {
                        f.input.insert(ch);
                    }
                }
                _ => {}
            },
        }

        // Key didn't trigger accept/cancel — put the modal back.
        self.state.modal = Some(modal);
    }

    /// Open an edit form pre-filled with the currently selected row's
    /// values. Field 0 is the PK (used for the WHERE clause); the
    /// rest are the editable column values. The submit handler builds
    /// an `UPDATE table SET col=val,... WHERE pk=original_pk` SQL.
    fn open_update_form(&mut self) {
        let Some(table) = self.state.selected_table().cloned() else {
            self.state.set_notification("No table selected".to_string());
            return;
        };
        if table.columns.is_empty() {
            self.state
                .set_error(format!("Table '{}' has no columns", table.table_name));
            return;
        }
        let data_idx = match self.active_data_row_index() {
            Some(i) => i,
            None => {
                self.state.set_notification("No row selected".to_string());
                return;
            }
        };
        let row = match self
            .state
            .table_browse_result
            .as_ref()
            .and_then(|qr| qr.rows.get(data_idx))
        {
            Some(r) => r.clone(),
            None => return,
        };

        // Pick the PK column up-front so we can mark it read-only in
        // the form labels and use it in the WHERE clause below.
        let (pk_idx, pk_column) = pick_primary_key(&table);

        // Pre-fill each form field with the row's current display value.
        let fields: Vec<crate::state::modal::FormField> = table
            .columns
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let type_label = type_tag(&c.col_type);
                let pk_marker = if i == pk_idx {
                    " — PK (read-only)"
                } else {
                    ""
                };
                let mut field = crate::state::modal::FormField::new(format!(
                    "{} ({}{pk_marker})",
                    c.col_name, type_label
                ));
                let cell_text = row
                    .get(i)
                    .map(crate::ui::tabs::tables::value_to_display)
                    .unwrap_or_default();
                field.input.set(cell_text);
                field
            })
            .collect();

        let column_types: Vec<String> = table
            .columns
            .iter()
            .map(|c| type_tag(&c.col_type))
            .collect();
        // Build the PK WHERE literal directly from the raw JSON
        // value so Identity / ConnectionId / U256 PKs round-trip
        // correctly. Display strings (`value_to_display`) mangle
        // these into `{__identity__:0xabc}` which the server
        // cannot parse.
        let pk_sql_literal = match row.get(pk_idx).and_then(|v| json_to_sql_literal(v).ok()) {
            Some(lit) => lit,
            None => {
                self.state.set_error(format!(
                    "Cannot edit row: primary key of {} has no SQL literal form",
                    table.table_name
                ));
                return;
            }
        };

        let action = crate::state::modal::ModalAction::UpdateRow {
            table: table.table_name.clone(),
            pk_column,
            column_types,
            pk_sql_literal,
            pk_index: pk_idx,
        };
        self.state.modal = Some(crate::state::modal::Modal::form(
            format!("Update row in {}", table.table_name),
            fields,
            action,
        ));
    }

    /// Open a one-field form to attach a new alias / human name to
    /// the currently selected database. Non-destructive — no typed
    /// confirm required, just a non-empty string.
    fn open_add_alias_form(&mut self) {
        let Some(db_name) = self.state.selected_database().map(str::to_string) else {
            self.state
                .set_notification("No database selected".to_string());
            return;
        };
        let field =
            crate::state::modal::FormField::new("New alias").with_placeholder("e.g. my-app-prod");
        let action = crate::state::modal::ModalAction::AddDatabaseAlias {
            database: db_name.clone(),
        };
        self.state.modal = Some(crate::state::modal::Modal::form(
            format!("Add alias to {db_name}"),
            vec![field],
            action,
        ));
    }

    /// Shared constructor for the "type the name to confirm" modal
    /// flow used by destructive admin ops. The dispatcher checks
    /// `fields[0].input.value == expected` before running `action`.
    fn open_typed_confirm_form(
        &mut self,
        title: impl Into<String>,
        expected: &str,
        verb: &str,
        action: crate::state::modal::ModalAction,
    ) {
        let field =
            crate::state::modal::FormField::new(format!("Type '{expected}' to confirm {verb}"))
                .with_placeholder("required");
        self.state.modal = Some(crate::state::modal::Modal::form(
            title.into(),
            vec![field],
            action,
        ));
    }

    /// Open a typed-confirm form to permanently delete the currently
    /// selected database. The user has to type the database name
    /// verbatim — a plain `[y/n]` would be too easy to trigger by
    /// accident and the operation is irreversible.
    fn open_delete_db_form(&mut self) {
        let Some(db_name) = self.state.selected_database().map(str::to_string) else {
            self.state
                .set_notification("No database selected".to_string());
            return;
        };
        self.open_typed_confirm_form(
            format!("⚠ DELETE DATABASE {db_name}"),
            &db_name.clone(),
            "DELETE",
            crate::state::modal::ModalAction::DeleteDatabase { database: db_name },
        );
    }

    /// Open a typed-confirm form to delete every row from the
    /// currently selected table. SpacetimeDB tables are part of
    /// the published module schema and cannot be `DROP`ped via SQL,
    /// so this is the closest "delete the table" operation we can
    /// safely offer from a TUI session — it issues `DELETE FROM
    /// <table>` once the user has typed the table name back to us.
    fn open_truncate_table_form(&mut self) {
        let Some(table) = self.state.selected_table().map(|t| t.table_name.clone()) else {
            self.state.set_notification("No table selected".to_string());
            return;
        };
        self.open_typed_confirm_form(
            format!("⚠ TRUNCATE TABLE {table}"),
            &table.clone(),
            "TRUNCATE",
            crate::state::modal::ModalAction::TruncateTable { table },
        );
    }

    /// Open a confirm dialog to delete the currently selected row.
    /// Uses the heuristic in [`pick_primary_key`] for the WHERE clause.
    fn open_delete_confirm(&mut self) {
        // Pull the data we need before borrowing mutably.
        let Some(table) = self.state.selected_table().cloned() else {
            self.state.set_notification("No table selected".to_string());
            return;
        };
        if table.columns.is_empty() {
            self.state
                .set_error(format!("Table '{}' has no columns", table.table_name));
            return;
        }

        let data_idx = match self.active_data_row_index() {
            Some(i) => i,
            None => {
                self.state.set_notification("No row selected".to_string());
                return;
            }
        };
        let row = match self
            .state
            .table_browse_result
            .as_ref()
            .and_then(|qr| qr.rows.get(data_idx))
        {
            Some(r) => r.clone(),
            None => return,
        };

        let (pk_idx, pk_name) = pick_primary_key(&table);
        // Build the WHERE literal directly from the raw JSON value
        // so Identity / ConnectionId / U256 PKs are emitted as real
        // SQL literals (`0xdeadbeef`) instead of the mangled display
        // form (`{__identity__:0xdeadbeef}`).
        let pk_literal = match row.get(pk_idx).and_then(|v| json_to_sql_literal(v).ok()) {
            Some(lit) => lit,
            None => {
                self.state.set_error(format!(
                    "Cannot delete row: primary key of {} has no SQL literal form",
                    table.table_name
                ));
                return;
            }
        };
        let where_sql = format!("{pk_name} = {pk_literal}");

        let prompt = format!(
            "DELETE FROM {table_name} WHERE {where_sql}\n\n\
             This will permanently remove one row.\n\
             Press [y] to confirm, [n] to cancel.",
            table_name = table.table_name,
        );
        let action = crate::state::modal::ModalAction::DeleteRow {
            table: table.table_name.clone(),
            where_sql,
        };
        self.state.modal = Some(crate::state::modal::Modal::confirm(
            format!("Delete row from {}", table.table_name),
            prompt,
            action,
        ));
    }

    /// Open an insert form for the currently selected table in the
    /// Tables tab. Each user-visible column gets one form field. The
    /// submit handler builds an `INSERT INTO ... VALUES (...)` SQL
    /// statement and runs it via [`spawn_write_sql`].
    fn open_insert_form(&mut self) {
        let Some(table) = self.state.selected_table().cloned() else {
            self.state.set_notification("No table selected".to_string());
            return;
        };

        if table.columns.is_empty() {
            self.state
                .set_error(format!("Table '{}' has no columns", table.table_name));
            return;
        }

        let fields: Vec<crate::state::modal::FormField> = table
            .columns
            .iter()
            .map(|c| {
                let type_label = type_tag(&c.col_type);
                let auto = if c.is_autoinc { " — auto" } else { "" };
                crate::state::modal::FormField::new(format!(
                    "{} ({}{auto})",
                    c.col_name, type_label
                ))
                .with_placeholder(default_placeholder_for_type(&type_label))
            })
            .collect();

        let column_types: Vec<String> = table
            .columns
            .iter()
            .map(|c| type_tag(&c.col_type))
            .collect();

        let action = crate::state::modal::ModalAction::InsertRow {
            table: table.table_name.clone(),
            column_types,
        };

        self.state.modal = Some(crate::state::modal::Modal::form(
            format!("Insert into {}", table.table_name),
            fields,
            action,
        ));
    }

    /// Open a reducer-call form for the currently selected reducer in
    /// the Module tab. No-op if there is no schema or selection.
    fn open_reducer_form(&mut self) {
        let Some(schema) = self.state.current_schema.as_ref() else {
            return;
        };
        let Some(reducer) = schema.reducers.get(self.state.module_selected_reducer) else {
            return;
        };

        let fields: Vec<crate::state::modal::FormField> = reducer
            .params
            .iter()
            .map(|p| {
                let type_label = type_tag(&p.algebraic_type);
                crate::state::modal::FormField::new(format!("{} ({})", p.name, type_label))
                    .with_placeholder(default_placeholder_for_type(&type_label))
            })
            .collect();

        let action = crate::state::modal::ModalAction::CallReducer {
            reducer: reducer.name.clone(),
            param_types: reducer
                .params
                .iter()
                .map(|p| type_tag(&p.algebraic_type))
                .collect(),
        };

        let title = if reducer.params.is_empty() {
            format!("Call {} (no args — Enter to confirm)", reducer.name)
        } else {
            format!("Call {}", reducer.name)
        };

        self.state.modal = Some(crate::state::modal::Modal::form(title, fields, action));
    }

    /// Dispatch a finished modal action — runs the underlying API
    /// call on a background task and surfaces the result via
    /// `AppEvent::WriteOpSuccess` / `WriteOpError`. The modal is
    /// dropped (the caller already moved it out of `state.modal`).
    async fn dispatch_modal_action(&mut self, modal: crate::state::modal::Modal) {
        use crate::state::modal::{Modal, ModalAction};

        let db = match self.state.selected_database() {
            Some(d) => d.to_string(),
            None => {
                self.state.set_error("No database selected".to_string());
                return;
            }
        };
        let op_label = modal.action().op_label();

        match modal {
            Modal::Form { fields, action, .. } => match action {
                ModalAction::CallReducer {
                    reducer,
                    param_types,
                } => {
                    let args: Vec<serde_json::Value> = fields
                        .iter()
                        .zip(param_types.iter())
                        .map(|(f, t)| coerce_field_to_json(&f.input.value, t))
                        .collect();
                    self.spawn_call_reducer(db, reducer, args, op_label);
                }
                ModalAction::InsertRow {
                    table,
                    column_types,
                } => {
                    let columns: Vec<String> = fields
                        .iter()
                        .map(|f| extract_field_name(&f.label))
                        .collect();
                    let values: Vec<String> = fields
                        .iter()
                        .zip(column_types.iter())
                        .map(|(f, t)| sql_literal(&f.input.value, t))
                        .collect();
                    let sql = format!(
                        "INSERT INTO {table} ({}) VALUES ({})",
                        columns.join(", "),
                        values.join(", ")
                    );
                    self.spawn_write_sql(db, sql, op_label);
                }
                ModalAction::UpdateRow {
                    table,
                    pk_column,
                    column_types,
                    pk_sql_literal,
                    pk_index,
                } => {
                    // Skip the PK field when generating the SET clause
                    // — it's the WHERE column, not an assignment.
                    let assignments: Vec<String> = fields
                        .iter()
                        .zip(column_types.iter())
                        .enumerate()
                        .filter(|(i, _)| *i != pk_index)
                        .map(|(_, (f, t))| {
                            let col = extract_field_name(&f.label);
                            format!("{col} = {}", sql_literal(&f.input.value, t))
                        })
                        .collect();
                    if assignments.is_empty() {
                        self.state.set_notification("Nothing to update".to_string());
                        return;
                    }
                    // `pk_sql_literal` was built at modal-open time
                    // from the row's raw JSON Value, so it already
                    // handles Identity / ConnectionId / U256 PKs
                    // correctly without going through the display-
                    // string round trip.
                    let sql = format!(
                        "UPDATE {table} SET {} WHERE {pk_column} = {pk_sql_literal}",
                        assignments.join(", ")
                    );
                    self.spawn_write_sql(db, sql, op_label);
                }
                ModalAction::DeleteDatabase { database } => {
                    // Typed-confirm: the user must type the database
                    // name verbatim into the single form field.
                    let typed = fields.first().map(|f| f.input.value.trim().to_string());
                    if typed.as_deref() != Some(database.as_str()) {
                        self.state
                            .set_error(format!("Type '{database}' exactly to confirm"));
                        return;
                    }
                    self.spawn_delete_database(database, op_label);
                }
                ModalAction::AddDatabaseAlias { database } => {
                    // Non-destructive: accept any non-empty alias
                    // and forward to the server. Validation (uniqueness,
                    // formatting) is the server's job.
                    let alias = fields
                        .first()
                        .map(|f| f.input.value.trim().to_string())
                        .unwrap_or_default();
                    if alias.is_empty() {
                        self.state
                            .set_notification("Alias cannot be empty".to_string());
                        return;
                    }
                    self.spawn_add_alias(database, alias, op_label);
                }
                ModalAction::TruncateTable { table } => {
                    // Same typed-confirm pattern as DeleteDatabase.
                    let typed = fields.first().map(|f| f.input.value.trim().to_string());
                    if typed.as_deref() != Some(table.as_str()) {
                        self.state
                            .set_error(format!("Type '{table}' exactly to confirm"));
                        return;
                    }
                    let sql = format!("DELETE FROM {table}");
                    self.spawn_write_sql(db, sql, op_label);
                }
                ModalAction::DeleteRow { .. } => {
                    // DeleteRow is always a Confirm, never a Form —
                    // unreachable but handle gracefully.
                    self.state
                        .set_error("Internal: DeleteRow inside a Form".to_string());
                }
                ModalAction::DiscardPendingEdits => {
                    // DiscardPendingEdits is always a Confirm, never
                    // a Form — unreachable.
                }
            },
            Modal::Confirm { action, .. } => match action {
                ModalAction::DeleteRow { table, where_sql } => {
                    let sql = format!("DELETE FROM {table} WHERE {where_sql}");
                    self.spawn_write_sql(db, sql, op_label);
                }
                ModalAction::DiscardPendingEdits => {
                    // User confirmed leaving edit mode — drop the
                    // pending list and exit.
                    self.state.edit_mode = None;
                    self.state
                        .set_notification("Pending edits discarded".to_string());
                }
                _ => {
                    self.state
                        .set_error("Internal: non-DeleteRow inside Confirm".to_string());
                }
            },
        }
    }

    /// Run `client.add_database_alias` on a background task. On
    /// success we re-fetch the database list so any new alias
    /// shows up in the sidebar immediately, and re-pull the name
    /// list for the currently selected DB.
    fn spawn_add_alias(&self, database: String, alias: String, op_label: String) {
        let client = self.client.clone();
        let tx = self.event_tx.clone();
        tokio::spawn(async move {
            match tokio::time::timeout(
                HTTP_REQUEST_TIMEOUT,
                client.add_database_alias(&database, &alias),
            )
            .await
            {
                Ok(Ok(())) => {
                    send_event(
                        &tx,
                        AppEvent::WriteOpSuccess {
                            op: op_label,
                            response: serde_json::json!({
                                "database": database,
                                "new_alias": alias,
                            }),
                        },
                    );
                    // Refresh the sidebar so the new alias can be
                    // discovered without a restart.
                    if let Ok(Ok(dbs)) =
                        tokio::time::timeout(HTTP_REQUEST_TIMEOUT, client.list_databases()).await
                    {
                        send_event(&tx, AppEvent::DatabasesLoaded(dbs));
                    }
                }
                Ok(Err(e)) => send_event(
                    &tx,
                    AppEvent::WriteOpError {
                        op: op_label,
                        error: format!("{e:#}"),
                    },
                ),
                Err(_) => send_event(
                    &tx,
                    AppEvent::WriteOpError {
                        op: op_label,
                        error: "request timed out".to_string(),
                    },
                ),
            }
        });
    }

    /// Run `client.delete_database` on a background task. On success
    /// re-bootstraps the database list so the now-deleted entry
    /// disappears from the sidebar without a manual refresh.
    fn spawn_delete_database(&self, database: String, op_label: String) {
        let client = self.client.clone();
        let tx = self.event_tx.clone();
        tokio::spawn(async move {
            match tokio::time::timeout(HTTP_REQUEST_TIMEOUT, client.delete_database(&database))
                .await
            {
                Ok(Ok(())) => {
                    send_event(
                        &tx,
                        AppEvent::WriteOpSuccess {
                            op: op_label,
                            response: serde_json::json!({"deleted": database}),
                        },
                    );
                    // Re-fetch the database list so the sidebar
                    // updates immediately. The bootstrap helper
                    // already does ping → list_databases under a
                    // timeout.
                    if let Ok(Ok(dbs)) =
                        tokio::time::timeout(HTTP_REQUEST_TIMEOUT, client.list_databases()).await
                    {
                        send_event(&tx, AppEvent::DatabasesLoaded(dbs))
                    }
                }
                Ok(Err(e)) => send_event(
                    &tx,
                    AppEvent::WriteOpError {
                        op: op_label,
                        error: format!("{e:#}"),
                    },
                ),
                Err(_) => send_event(
                    &tx,
                    AppEvent::WriteOpError {
                        op: op_label,
                        error: "request timed out".to_string(),
                    },
                ),
            }
        });
    }

    /// Run `client.call_reducer` on a background task and route the
    /// outcome through `AppEvent::WriteOp{Success,Error}`.
    fn spawn_call_reducer(
        &self,
        db: String,
        reducer: String,
        args: Vec<serde_json::Value>,
        op_label: String,
    ) {
        let client = self.client.clone();
        let tx = self.event_tx.clone();
        tokio::spawn(async move {
            match tokio::time::timeout(
                HTTP_REQUEST_TIMEOUT,
                client.call_reducer(&db, &reducer, &args),
            )
            .await
            {
                Ok(Ok(response)) => send_event(
                    &tx,
                    AppEvent::WriteOpSuccess {
                        op: op_label,
                        response,
                    },
                ),
                Ok(Err(e)) => send_event(
                    &tx,
                    AppEvent::WriteOpError {
                        op: op_label,
                        error: format!("{e:#}"),
                    },
                ),
                Err(_) => send_event(
                    &tx,
                    AppEvent::WriteOpError {
                        op: op_label,
                        error: "request timed out".to_string(),
                    },
                ),
            }
        });
    }

    /// Run a write SQL statement (INSERT/UPDATE/DELETE) on a
    /// background task and route the outcome the same way reducer
    /// calls are.
    fn spawn_write_sql(&self, db: String, sql: String, op_label: String) {
        let client = self.client.clone();
        let tx = self.event_tx.clone();
        tokio::spawn(async move {
            match tokio::time::timeout(HTTP_REQUEST_TIMEOUT, client.query_sql(&db, &sql)).await {
                Ok(Ok(_result)) => send_event(
                    &tx,
                    AppEvent::WriteOpSuccess {
                        op: op_label,
                        response: serde_json::json!({"sql": sql}),
                    },
                ),
                Ok(Err(e)) => send_event(
                    &tx,
                    AppEvent::WriteOpError {
                        op: op_label,
                        error: format!("{e:#}"),
                    },
                ),
                Err(_) => send_event(
                    &tx,
                    AppEvent::WriteOpError {
                        op: op_label,
                        error: "request timed out".to_string(),
                    },
                ),
            }
        });
    }

    /// Tab-complete the SQL input against the current schema.
    ///
    /// Extracts the identifier token immediately to the left of the cursor,
    /// builds a candidate list from SQL keywords plus every table/column
    /// name in the active schema, and then either (a) commits the unique
    /// completion, (b) extends the token to the longest common prefix
    /// shared by multiple matches and surfaces the candidate list as a
    /// notification, or (c) shows a "no match" notification.
    fn complete_sql_input(&mut self) {
        use crate::ui::components::completion::{CompletionResult, build_candidates, complete};

        let (range, word) = self.sql_input.current_word();
        if word.is_empty() {
            return;
        }
        let word = word.to_string();

        let candidates = build_candidates(self.state.tables.iter());
        let refs: Vec<&str> = candidates.iter().map(String::as_str).collect();

        match complete(&word, &refs) {
            CompletionResult::NoMatch => {
                self.state
                    .set_notification(format!("No match for \"{word}\""));
            }
            CompletionResult::Unique(hit) => {
                self.sql_input.replace_range(range, &hit);
            }
            CompletionResult::Multiple {
                common_prefix,
                candidates,
            } => {
                // Extend the input to the longest common prefix (if any),
                // then show the user what's still ambiguous.
                if common_prefix.len() > word.len() {
                    self.sql_input.replace_range(range, &common_prefix);
                }
                let preview: Vec<String> = candidates.into_iter().take(6).collect();
                let more = if preview.len() == 6 { "…" } else { "" };
                self.state.set_notification(format!(
                    "{} matches: {}{more}",
                    preview.len(),
                    preview.join(", ")
                ));
            }
        }
    }

    async fn execute_sql(&mut self) {
        let sql = self.sql_input.as_str().trim().to_string();
        if sql.is_empty() {
            return;
        }

        let db = match self.state.selected_database() {
            Some(d) => d.to_string(),
            None => {
                self.state
                    .set_error("No database selected — pick one from the sidebar".to_string());
                return;
            }
        };

        self.state.query_loading = true;
        self.state.query_result = None;
        self.state.history_cursor = None;

        let client = self.client.clone();
        let tx = self.event_tx.clone();
        let start = Instant::now();
        let sql_clone = sql.clone();

        tokio::spawn(async move {
            let outcome =
                tokio::time::timeout(HTTP_REQUEST_TIMEOUT, client.query_sql(&db, &sql_clone)).await;
            match outcome {
                Ok(Ok(result)) => send_event(
                    &tx,
                    AppEvent::QueryResult {
                        result,
                        duration: start.elapsed(),
                        sql: sql_clone,
                    },
                ),
                Ok(Err(e)) => send_event(
                    &tx,
                    AppEvent::QueryError {
                        sql: sql_clone,
                        error: format!("{e:#}"),
                    },
                ),
                Err(_) => send_event(
                    &tx,
                    AppEvent::QueryError {
                        sql: sql_clone,
                        error: "SQL query timed out".to_string(),
                    },
                ),
            }
        });
    }

    async fn refresh_current_view(&mut self) {
        match self.state.current_tab {
            Tab::Tables => {
                // If the previous schema fetch failed, `r` should
                // retry it instead of running a no-op table load
                // against a schema we don't have.
                if self.state.schema_load_failed || self.state.current_schema.is_none() {
                    self.load_schema().await;
                } else {
                    self.load_table_data().await;
                }
            }
            Tab::Sql => {
                // Re-execute last SQL if any
                if let Some(entry) = self.state.sql_history.back() {
                    let sql = entry.sql.clone();
                    self.sql_input.set(sql);
                    self.execute_sql().await;
                }
            }
            Tab::Logs => {
                self.load_logs().await;
            }
            Tab::Metrics => {
                let client = self.client.clone();
                let tx = self.event_tx.clone();
                tokio::spawn(async move {
                    if let Ok(Ok(text)) =
                        tokio::time::timeout(HTTP_REQUEST_TIMEOUT, client.get_metrics()).await
                    {
                        let snapshot = parse_prometheus_metrics(&text);
                        send_event(&tx, AppEvent::MetricsLoaded(snapshot));
                    }
                    let ok = matches!(
                        tokio::time::timeout(HTTP_REQUEST_TIMEOUT, client.ping()).await,
                        Ok(true)
                    );
                    send_event(&tx, AppEvent::PingResult(ok));
                });
            }
            Tab::Module => {
                self.load_schema().await;
            }
            Tab::Live => {
                // The Live tab is driven entirely by the WebSocket
                // subscription + the client-list polling task, so a
                // manual refresh just re-subscribes.
                self.connect_ws().await;
            }
        }
    }

    async fn load_logs(&mut self) {
        let db = match self.state.selected_database() {
            Some(d) => d.to_string(),
            None => return,
        };
        let client = self.client.clone();
        let tx = self.event_tx.clone();
        tokio::spawn(async move {
            match tokio::time::timeout(HTTP_REQUEST_TIMEOUT, client.get_logs(&db, 500, false)).await
            {
                Ok(Ok(logs)) => send_event(&tx, AppEvent::LogsLoaded(logs)),
                Ok(Err(e)) => send_event(&tx, AppEvent::Error(format!("Logs fetch failed: {e:#}"))),
                Err(_) => send_event(&tx, AppEvent::Error("Logs fetch timed out".to_string())),
            }
        });
    }

    // ── WebSocket integration ─────────────────────────────────────────────

    /// Connect a WebSocket subscription for the currently selected database.
    ///
    /// Closes any existing WebSocket connection before opening a new one and
    /// clears any stale live-data cache from a previous database.
    async fn connect_ws(&mut self) {
        // Close existing connection if any
        if let Some(ref handle) = self.ws_handle {
            handle.close().await;
        }
        self.ws_handle = None;
        self.state.ws_connected = false;
        self.state.live_table_data.clear();

        let db = match self.state.selected_database() {
            Some(d) => d.to_string(),
            None => return,
        };

        let config = WsConfig {
            base_url: self.ws_url.clone(),
            database: db,
            auth_token: self.auth_token.clone(),
            channel_capacity: 256,
        };

        match crate::api::ws::spawn_subscription(config) {
            Ok(handle) => {
                self.ws_handle = Some(handle);
                tracing::info!("WebSocket subscription task spawned");
            }
            Err(e) => {
                tracing::warn!("Failed to spawn WebSocket subscription: {e}");
                send_event(
                    &self.event_tx,
                    AppEvent::Notification(format!("WebSocket unavailable: {e}")),
                );
            }
        }
    }

    /// Drain all pending WebSocket events without blocking.
    async fn drain_ws_events(&mut self) {
        // Collect events first to avoid borrow issues
        let mut events: Vec<WsEvent> = Vec::new();
        if let Some(ref mut handle) = self.ws_handle {
            while let Ok(ev) = handle.event_rx.try_recv() {
                events.push(ev);
            }
        }
        for ev in events {
            self.handle_ws_event(ev).await;
        }
    }

    /// Handle a single WebSocket event.
    async fn handle_ws_event(&mut self, event: WsEvent) {
        match event {
            WsEvent::Connected => {
                tracing::info!("WebSocket connected");
                self.state.ws_connected = true;
                self.state.ws_reconnect_deadline = None;
                self.state.ws_reconnect_attempt = 0;
                // Subscribe to all user tables after connection
                self.ws_subscribe_all_tables().await;
            }
            WsEvent::ServerMessage(msg) => {
                self.handle_ws_server_message(msg);
            }
            WsEvent::LogLine(entry) => {
                send_event(&self.event_tx, AppEvent::LogLine(entry));
            }
            WsEvent::Disconnected { reason } => {
                tracing::warn!("WebSocket disconnected: {reason}");
                self.state.ws_connected = false;
                // If the disconnect was flagged as permanent
                // ("(retries disabled)" marker from subscription_task),
                // clear the countdown so the status bar doesn't keep
                // showing a stale "reconnect in Ns" pill forever.
                if reason.contains("(retries disabled)") {
                    self.state.ws_reconnect_deadline = None;
                    self.state.ws_reconnect_attempt = 0;
                }
                send_event(
                    &self.event_tx,
                    AppEvent::Notification(format!("WebSocket disconnected: {reason}")),
                );
            }
            WsEvent::Reconnecting { attempt, delay_ms } => {
                tracing::info!("WebSocket reconnect attempt {attempt} in {delay_ms}ms");
                self.state.ws_reconnect_attempt = attempt;
                self.state.ws_reconnect_deadline =
                    Some(Instant::now() + Duration::from_millis(delay_ms));
                // No notification here — the status bar renders a live
                // countdown from `ws_reconnect_deadline` so a persistent
                // toast would just duplicate the information.
            }
            WsEvent::Error(e) => {
                tracing::warn!("WebSocket error: {e}");
            }
            WsEvent::RawText(text) => {
                // Raw frames we can't decode as structured messages — log for diagnostics
                tracing::debug!("WebSocket raw text frame ({} bytes)", text.len());
            }
        }
    }

    /// Send subscription queries for all user tables in the current schema.
    async fn ws_subscribe_all_tables(&mut self) {
        let queries: Vec<String> = self
            .state
            .tables
            .iter()
            .filter(|t| t.table_type != "system")
            .map(|t| format!("SELECT * FROM {}", t.table_name))
            .collect();

        if queries.is_empty() {
            return;
        }

        if let Some(ref handle) = self.ws_handle {
            if let Err(e) = handle.subscribe(queries, 1).await {
                tracing::warn!("WS subscribe failed: {e}");
            }
        }
    }

    /// Push a transaction entry onto the Live-tab feed, capping the
    /// buffer so a chatty module can't grow it without bound.
    fn push_tx_log_entry(&mut self, entry: crate::state::TxLogEntry) {
        const MAX: usize = 500;
        if self.state.tx_log.len() >= MAX {
            self.state.tx_log.pop_front();
        }
        self.state.tx_log.push_back(entry);
    }

    /// Apply a decoded WebSocket server message to the application state.
    fn handle_ws_server_message(&mut self, msg: crate::api::types::WsServerMessage) {
        use crate::api::types::WsServerMessage;
        match msg {
            WsServerMessage::InitialSubscription(payload) => {
                // Initial snapshot — replace any existing live data for each table.
                let mut total_rows = 0usize;
                for table_update in payload.database_update.tables {
                    total_rows += table_update.inserts.len();
                    self.state
                        .live_table_data
                        .insert(table_update.table_name, table_update.inserts);
                }
                send_event(
                    &self.event_tx,
                    AppEvent::Notification(format!("Live subscription active — {total_rows} rows")),
                );
            }
            WsServerMessage::TransactionUpdate(payload) => {
                // Incremental update — apply inserts/deletes to the cached
                // live data. Deletes are matched by exact JSON value equality
                // (the server's row identity model isn't exposed in the JSON
                // protocol, so this is a best-effort match).
                let mut total_changes = 0usize;
                let mut per_table: Vec<(String, usize, usize)> = Vec::new();
                for table_update in payload.database_update.tables {
                    let inserts_n = table_update.inserts.len();
                    let deletes_n = table_update.deletes.len();
                    total_changes += inserts_n + deletes_n;
                    per_table.push((table_update.table_name.clone(), inserts_n, deletes_n));

                    let entry = self
                        .state
                        .live_table_data
                        .entry(table_update.table_name)
                        .or_default();
                    if !table_update.deletes.is_empty() {
                        entry.retain(|row| !table_update.deletes.contains(row));
                    }
                    entry.extend(table_update.inserts);
                }
                if total_changes > 0 {
                    tracing::debug!("Transaction update: {total_changes} row changes");
                }

                // Push a summary row into the Live tab's transaction feed.
                // Extract caller identity + status from the payload's
                // free-form `extra` map (which preserves fields the
                // server added after we wrote this code).
                let caller = payload
                    .extra
                    .get("caller_identity")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                let committed = payload
                    .status
                    .as_ref()
                    .map(|s| matches!(s, crate::api::types::TransactionStatus::Committed));
                self.push_tx_log_entry(crate::state::TxLogEntry {
                    observed_at: chrono::Utc::now(),
                    caller,
                    tables: per_table,
                    committed,
                });
            }
            WsServerMessage::IdentityToken(payload) => {
                tracing::info!("WebSocket identity confirmed: {:?}", payload.identity);
            }
        }
    }

    // ── Async event handler ───────────────────────────────────────────────

    async fn handle_app_event(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::PingResult(ok) => {
                if ok {
                    self.state.connection.status = ConnectionStatus::Connected;
                } else {
                    self.state.connection.status =
                        ConnectionStatus::Error("Server unreachable".to_string());
                }
            }

            AppEvent::DatabasesLoaded(dbs) => {
                self.state.connection.status = ConnectionStatus::Connected;
                // The list call returns names only; rebuild the database
                // list from them while carrying over any status we'd
                // already discovered for a name.
                let existing: Vec<crate::state::Database> =
                    self.state.databases.drain(..).collect();
                let prior_status = |name: &str| {
                    existing
                        .iter()
                        .find(|d| d.name == name)
                        .map(|d| d.status)
                        .unwrap_or_default()
                };
                self.state.databases = dbs
                    .into_iter()
                    .map(|name| crate::state::Database {
                        status: prior_status(&name),
                        name,
                    })
                    .collect();
                // Preserve any pre-selected DB not present in the new list.
                for db in existing {
                    if !self.state.databases.iter().any(|d| d.name == db.name) {
                        self.state.databases.insert(0, db);
                    }
                }
                // If a previous session left a "last database" hint
                // and we still have no selection, try to land on it.
                if self.state.selected_database_idx.is_none() {
                    if let Some(session) = self.pending_session.as_ref() {
                        if let Some(ref last_db) = session.last_database {
                            if let Some(idx) =
                                self.state.databases.iter().position(|d| &d.name == last_db)
                            {
                                self.state.select_database(idx);
                                if let Some(tab_idx) = session.last_tab {
                                    self.state.current_tab = index_to_tab(tab_idx);
                                }
                                self.load_schema().await;
                            }
                        }
                    }
                }
                if !self.state.databases.is_empty() && self.state.selected_database_idx.is_none() {
                    self.state.select_database(0);
                    self.load_schema().await;
                }
            }

            AppEvent::SchemaLoaded(schema) => {
                self.state.schema_loading = false;
                self.state.schema_load_failed = false;
                // The schema came back, so this database is responding —
                // clear any stale paused flag.
                if let Some(name) = self.state.selected_database().map(str::to_string) {
                    self.state
                        .set_database_status(&name, crate::state::DatabaseStatus::Active);
                }
                self.state.tables = schema.tables.clone();
                // If we restored a session and the user was looking at
                // a specific table, jump to it instead of defaulting
                // to row 0.
                if !self.state.tables.is_empty() && self.state.selected_table_idx.is_none() {
                    let restored = self
                        .pending_session
                        .as_ref()
                        .and_then(|s| s.last_table.as_deref())
                        .and_then(|name| {
                            self.state.tables.iter().position(|t| t.table_name == name)
                        });
                    self.state.selected_table_idx = Some(restored.unwrap_or(0));
                }
                // Session restore is one-shot — don't keep firing it
                // every time the user navigates to a new database.
                self.pending_session = None;
                self.state.current_schema = Some(schema);
                let table_count = self.state.tables.len();
                send_event(
                    &self.event_tx,
                    AppEvent::Notification(format!("Schema loaded — {table_count} tables")),
                );
                // Establish WebSocket subscription for live data
                self.connect_ws().await;
            }

            AppEvent::SchemaError(msg) => {
                // Clear the in-flight flag so the sidebar drops its
                // "(loading…)" placeholder, then flip the terminal
                // "failed" flag so it can show an error hint instead.
                self.state.schema_loading = false;
                self.state.schema_load_failed = true;
                self.state.set_error(msg);
            }

            AppEvent::SchemaPaused { database, message } => {
                self.state.schema_loading = false;
                self.state.schema_load_failed = true;
                self.state
                    .set_database_status(&database, crate::state::DatabaseStatus::Paused);
                self.state.set_error(message);
            }

            AppEvent::QueryResult {
                result,
                duration,
                sql,
            } => {
                self.state.query_loading = false;
                let row_count = result.row_count();
                self.state.query_result = Some(result);
                // Reset grid scroll on new results
                self.tables_grid = TableGridState::new();
                self.sql_grid = TableGridState::new();

                // Push to history
                self.state.push_sql_history(SqlHistoryEntry {
                    sql,
                    executed_at: chrono::Utc::now(),
                    duration,
                    row_count: Some(row_count),
                    error: None,
                });
                self.state
                    .set_notification(format!("{row_count} rows returned"));
            }

            AppEvent::QueryError { sql, error } => {
                self.state.query_loading = false;
                self.state.push_sql_history(SqlHistoryEntry {
                    sql,
                    executed_at: chrono::Utc::now(),
                    duration: Duration::ZERO,
                    row_count: None,
                    error: Some(error.clone()),
                });
                self.state.set_error(error);
            }

            AppEvent::TableBrowseResult { result } => {
                self.state.query_loading = false;
                let row_count = result.row_count();
                self.state.table_browse_result = Some(result);
                // Reset the Tables grid scroll/selection on fresh data.
                self.tables_grid = TableGridState::new();
                self.state
                    .set_notification(format!("{row_count} rows loaded"));
            }

            AppEvent::TableBrowseError { error } => {
                self.state.query_loading = false;
                self.state.set_error(error);
            }

            AppEvent::LogsLoaded(logs) => {
                self.state.extend_logs(logs);
                self.state.set_notification("Logs refreshed".to_string());
            }

            AppEvent::MetricsLoaded(snapshot) => {
                self.state.update_metrics(snapshot);
            }

            AppEvent::LiveClientsLoaded(clients) => {
                self.state.live_clients = clients;
            }

            AppEvent::WriteOpSuccess { op, response } => {
                self.state.query_loading = false;
                let summary = if response.is_null() {
                    op.clone()
                } else {
                    let s = response.to_string();
                    let preview: String = s.chars().take(60).collect();
                    format!("{op} → {preview}")
                };
                self.state.set_notification(format!("✓ {summary}"));
                // Many writes invalidate the table-browse view, so a
                // gentle refresh is useful — but only when the user
                // is still looking at the Tables tab.
                if self.state.current_tab == Tab::Tables && self.state.selected_table().is_some() {
                    self.load_table_data().await;
                }
            }

            AppEvent::WriteOpError { op, error } => {
                self.state.query_loading = false;
                self.state.set_error(format!("{op} failed: {error}"));
            }

            AppEvent::LogLine(entry) => {
                self.state.push_log(entry);
            }

            AppEvent::Notification(msg) => {
                self.state.set_notification(msg);
            }

            AppEvent::Error(msg) => {
                self.state.set_error(msg);
            }
        }
    }
}

// ── PK detection helper ──────────────────────────────────────────────────────

/// Pick the most likely primary-key column for a table.
///
/// SpacetimeDB's v9 schema JSON doesn't expose a reliable
/// "primary key" flag, so this is a heuristic with two fallbacks:
///   1. The first column flagged `is_autoinc` — autoinc columns are
///      almost always the table's identity in real schemas.
///   2. The first column whose name is `id`, `pk`, or ends in `_id`
///      and matches the table's bare name (e.g. `user_id` for `users`).
///   3. Column 0 — last-resort default.
///
/// Returns `(index, column_name)`. Used by row update / delete so the
/// WHERE clause targets the right column instead of always assuming
/// it's column zero.
fn pick_primary_key(table: &crate::api::types::TableInfo) -> (usize, String) {
    // 0. Server-declared primary key wins (populated by the v9
    //    schema parser from the table's `primary_key: [u16, ...]`
    //    field). Composite PKs aren't supported by the write-op
    //    SQL builders yet — we take the first column in that case
    //    so at least single-column lookups work.
    if let Some(&col_id) = table.primary_key_cols.first() {
        if let Some((i, c)) = table
            .columns
            .iter()
            .enumerate()
            .find(|(_, c)| c.col_id as u16 == col_id)
        {
            return (i, c.col_name.clone());
        }
    }
    // 1. autoinc wins (now that `is_autoinc` is actually set by the
    //    parser from the `sequences` array).
    if let Some((i, c)) = table.columns.iter().enumerate().find(|(_, c)| c.is_autoinc) {
        return (i, c.col_name.clone());
    }
    // 2. naming convention.
    let lower = table.table_name.to_ascii_lowercase();
    let stem = lower.trim_end_matches('s');
    for (i, c) in table.columns.iter().enumerate() {
        let n = c.col_name.to_ascii_lowercase();
        if n == "id" || n == "pk" || n == format!("{stem}_id") {
            return (i, c.col_name.clone());
        }
    }
    // 3. fallback.
    (
        0,
        table
            .columns
            .first()
            .map(|c| c.col_name.clone())
            .unwrap_or_default(),
    )
}

// ── Session restore helpers (Faz 6) ──────────────────────────────────────────

/// Encode a [`Tab`] as the same `0..6` index used in
/// `Tab::ALL`. Used when persisting / restoring `SessionState`.
fn tab_to_index(tab: Tab) -> u8 {
    Tab::ALL.iter().position(|t| *t == tab).unwrap_or(0) as u8
}

/// Inverse of [`tab_to_index`].
fn index_to_tab(idx: u8) -> Tab {
    Tab::ALL.get(idx as usize).copied().unwrap_or(Tab::Tables)
}

// ── Modal helpers (Faz 5) ────────────────────────────────────────────────────

/// Extract the SpacetimeDB type "tag" from an algebraic-type JSON
/// value. The schema encodes types as either `"String"` or
/// `{"String": []}`-style objects, so we tolerate both.
fn type_tag(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Object(o) => o
            .keys()
            .next()
            .cloned()
            .unwrap_or_else(|| "Unknown".to_string()),
        _ => "Unknown".to_string(),
    }
}

/// Suggest a placeholder string for a form field based on its type
/// tag. Just guidance — the user can type whatever they want.
fn default_placeholder_for_type(t: &str) -> String {
    match t {
        "String" => "text".to_string(),
        "Bool" => "true / false".to_string(),
        s if s.starts_with('U') || s.starts_with('I') => "0".to_string(),
        s if s.starts_with('F') => "0.0".to_string(),
        _ => "".to_string(),
    }
}

/// Extract the bare column / parameter name from a form field label
/// like `"name (String)"` → `"name"`.
fn extract_field_name(label: &str) -> String {
    label.split_whitespace().next().unwrap_or("").to_string()
}

/// Coerce a raw input string into a JSON value suitable for the
/// SpacetimeDB reducer-call wire format. We try to be helpful but
/// not magical: numerics parse, booleans parse, everything else stays
/// a string. JSON-shaped input (`[1,2]`, `{"k":"v"}`) is preserved
/// verbatim by attempting a `serde_json::from_str` first.
fn coerce_field_to_json(raw: &str, type_tag: &str) -> serde_json::Value {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return serde_json::Value::Null;
    }
    // Numeric / bool inference based on the declared type tag.
    if type_tag.starts_with('U') || type_tag.starts_with('I') {
        if let Ok(n) = trimmed.parse::<i64>() {
            return serde_json::json!(n);
        }
    }
    if type_tag.starts_with('F') {
        if let Ok(f) = trimmed.parse::<f64>() {
            return serde_json::json!(f);
        }
    }
    if type_tag == "Bool" {
        if let Ok(b) = trimmed.parse::<bool>() {
            return serde_json::json!(b);
        }
    }
    // If the input *looks* like JSON, accept it as-is so users can
    // pass arrays / objects to complex param types.
    if matches!(trimmed.chars().next(), Some('[' | '{' | '"')) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
            return v;
        }
    }
    serde_json::Value::String(trimmed.to_string())
}

/// Build a SQL literal from a raw input string and a type tag.
/// Numerics are emitted bare, booleans become `TRUE`/`FALSE`, and
/// everything else is single-quoted with embedded quotes doubled.
///
/// `Identity` / `ConnectionId` / `Address` columns get a hex-literal
/// path: if the user typed `"0xabc…"` or a bare `"abc…"` the emitted
/// literal is `0xabc…` (valid SpacetimeDB hex syntax) rather than a
/// single-quoted string — which the server would reject.
fn sql_literal(raw: &str, type_tag: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "NULL".to_string();
    }
    // Identity / ConnectionId / Address want a hex literal, not a
    // quoted string. Accept both "0xabc…" and bare "abc…" and always
    // emit the `0x`-prefixed form. Checked *before* the numeric
    // prefix branch below because `"Identity"` starts with `I` and
    // would otherwise get mis-classified as an integer type.
    if type_tag == "Identity" || type_tag == "ConnectionId" || type_tag.contains("Address") {
        let body = trimmed.strip_prefix("0x").unwrap_or(trimmed);
        if !body.is_empty() && body.chars().all(|c| c.is_ascii_hexdigit()) {
            return format!("0x{body}");
        }
        // Fall through to quoted-string form if the input doesn't
        // look like hex — surfaces the problem as a SQL error
        // rather than sending a silently-wrong literal.
        let escaped = trimmed.replace('\'', "''");
        return format!("'{escaped}'");
    }
    if type_tag.starts_with('U') || type_tag.starts_with('I') || type_tag.starts_with('F') {
        return trimmed.to_string();
    }
    if type_tag == "Bool" {
        return match trimmed.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => "TRUE".to_string(),
            _ => "FALSE".to_string(),
        };
    }
    let escaped = trimmed.replace('\'', "''");
    format!("'{escaped}'")
}

/// Convert a raw JSON `Value` directly into a SpacetimeDB SQL literal.
///
/// Goes further than [`sql_literal`]: instead of flattening the value
/// to a display string first (which corrupts `Identity` / `ConnectionId` /
/// `U256` into garbage like `{__identity__:0xabc}`), this inspects the
/// value's shape and emits the correct SQL form:
///
/// | JSON shape                           | Emits              |
/// |--------------------------------------|--------------------|
/// | `null`                               | `NULL`             |
/// | `true` / `false`                     | `TRUE` / `FALSE`   |
/// | integer / float                      | bare number        |
/// | `"0xdead"` hex-looking string        | `0xdead`           |
/// | regular string                       | `'…'` (escaped)    |
/// | `{"__identity__": "0xabc"}`          | `0xabc`            |
/// | `{"__connection_id__": "0xabc"}`     | `0xabc`            |
/// | `{"U256": "12345"}` / `{"I128": …}`  | bare number        |
/// | array / unknown object               | Err(…) with reason |
///
/// Used by the row-level write helpers (delete / update / edit-mode
/// save) so their WHERE clauses actually match the row on the server
/// instead of getting rejected for a bogus string literal.
fn json_to_sql_literal(v: &serde_json::Value) -> Result<String, String> {
    use serde_json::Value;
    match v {
        Value::Null => Ok("NULL".to_string()),
        Value::Bool(true) => Ok("TRUE".to_string()),
        Value::Bool(false) => Ok("FALSE".to_string()),
        Value::Number(n) => Ok(n.to_string()),
        Value::String(s) => {
            // Accept hex-looking strings verbatim so `Identity` /
            // `ConnectionId` values survive round-tripping without
            // being wrapped in single quotes.
            if let Some(hex) = s.strip_prefix("0x") {
                if !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Ok(s.clone());
                }
            }
            // Bare-hex heuristic (no `0x` prefix): some server versions
            // serialise `Identity` / `ConnectionId` as a bare hex string
            // like `"c2005e40a5b1..."`. Re-add the prefix so the literal
            // is valid SpacetimeDB hex syntax.
            //
            // Require ≥8 chars and even length so short ambiguous
            // strings (`"ab"`, `"cafe"`, `"deadbeef"` as a nickname)
            // still fall through to the quoted-string branch unless
            // they're long enough to be obviously an identity hash.
            if s.len() >= 16 && s.len() % 2 == 0 && s.chars().all(|c| c.is_ascii_hexdigit()) {
                return Ok(format!("0x{s}"));
            }
            let escaped = s.replace('\'', "''");
            Ok(format!("'{escaped}'"))
        }
        Value::Object(o) => {
            // SpacetimeDB's JSON encoding for the scalar SATS types
            // that need special-casing. The key tells us the wire tag.
            if let Some(key) = o.keys().next() {
                let inner = &o[key];
                match key.as_str() {
                    // Identity / ConnectionId come through in a
                    // couple of shapes depending on the server
                    // version: `{"__identity__": "0xabc"}`, the
                    // tagged form `{"Identity": "0xabc"}`, or a
                    // bare object with a single hex string value.
                    "__identity__" | "Identity" | "__connection_id__" | "ConnectionId" => {
                        if let Some(s) = inner.as_str() {
                            // Ensure the leading `0x` for the literal.
                            if s.starts_with("0x") {
                                return Ok(s.to_string());
                            }
                            return Ok(format!("0x{s}"));
                        }
                    }
                    // Large integer SATS types sometimes serialise
                    // as `{"U256": "12345"}` to preserve precision.
                    "U128" | "U256" | "I128" | "I256" => {
                        if let Some(s) = inner.as_str() {
                            return Ok(s.to_string());
                        }
                        if let Some(n) = inner.as_u64() {
                            return Ok(n.to_string());
                        }
                        if let Some(n) = inner.as_i64() {
                            return Ok(n.to_string());
                        }
                    }
                    _ => {}
                }
            }
            Err(format!(
                "complex value {v} has no SQL literal form — edit via the SQL console instead"
            ))
        }
        Value::Array(_) => Err(format!(
            "array value {v} has no SQL literal form — SpacetimeDB SQL doesn't support array literals"
        )),
    }
}

// ── Metrics Parser ────────────────────────────────────────────────────────────

fn parse_prometheus_metrics(text: &str) -> crate::state::MetricsSnapshot {
    let mut snapshot = crate::state::MetricsSnapshot {
        sampled_at: Some(chrono::Utc::now()),
        ..Default::default()
    };

    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 2 {
            continue;
        }
        let key = parts[0];
        let val: f64 = parts[1].parse().unwrap_or(0.0);

        if key.contains("connected_clients") {
            snapshot.connected_clients = val as u64;
        } else if key.contains("reducer_calls_total") {
            snapshot.total_reducer_calls = val as u64;
        } else if key.contains("energy_used_total") {
            snapshot.total_energy_used = val as u64;
        } else if key.contains("memory_bytes") {
            snapshot.memory_bytes = val as u64;
        } else {
            snapshot
                .extra
                .insert(key.to_string(), serde_json::json!(val));
        }
    }
    snapshot
}

// ── Tests for modal helpers ──────────────────────────────────────────────────
//
// Kept inline (rather than in a separate file) so the tests sit
// next to the `json_to_sql_literal` / `sql_literal` / `pick_primary_key`
// free functions they exercise. `draw_frame` follows this block on
// purpose — it's the last item in the file and logically closes out
// the app module, so we suppress clippy's "items after test module"
// lint here rather than fragmenting the code with a second file.
#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod modal_helper_tests {
    use super::*;

    use crate::api::types::{ColumnInfo, TableInfo};

    fn make_col(name: &str, autoinc: bool) -> ColumnInfo {
        make_col_at(0, name, autoinc)
    }

    /// Build a test column with an explicit `col_id`. Needed for
    /// the declared-PK path — `pick_primary_key` matches the PK
    /// entry against `ColumnInfo.col_id`, so every column in a
    /// PK-test fixture needs a distinct id.
    fn make_col_at(col_id: u32, name: &str, autoinc: bool) -> ColumnInfo {
        ColumnInfo {
            col_id,
            col_name: name.to_string(),
            col_type: serde_json::json!("U64"),
            is_autoinc: autoinc,
        }
    }

    fn make_table(name: &str, cols: Vec<ColumnInfo>) -> TableInfo {
        TableInfo {
            table_name: name.to_string(),
            product_type_ref: 0,
            table_type: "user".to_string(),
            table_access: "public".to_string(),
            columns: cols,
            primary_key_cols: vec![],
            indexes: vec![],
            constraints: vec![],
        }
    }

    /// Same as `make_table` but pre-populated with a declared
    /// server-side primary key (the `primary_key: [col_id, ...]`
    /// array that v9 schemas carry). Used by the "prefers declared
    /// PK" regression test.
    fn make_table_with_pk(name: &str, cols: Vec<ColumnInfo>, pk_col_ids: Vec<u16>) -> TableInfo {
        let mut t = make_table(name, cols);
        t.primary_key_cols = pk_col_ids;
        t
    }

    #[test]
    fn pick_primary_key_prefers_declared_server_pk() {
        // When the v9 schema carries a `primary_key: [col_id]`
        // entry, that wins over every heuristic — including
        // autoinc matches on a different column.
        let t = make_table_with_pk(
            "sessions",
            vec![
                make_col_at(0, "created_at", false),
                make_col_at(1, "auto_id", true), // autoinc, but NOT the declared PK
                make_col_at(2, "session_token", false),
            ],
            vec![2], // server says col_id 2 is the primary key
        );
        let (idx, name) = pick_primary_key(&t);
        assert_eq!(idx, 2);
        assert_eq!(name, "session_token");
    }

    #[test]
    fn pick_primary_key_prefers_autoinc() {
        let t = make_table(
            "users",
            vec![
                make_col("name", false),
                make_col("user_id", true), // autoinc — should win
                make_col("email", false),
            ],
        );
        let (idx, name) = pick_primary_key(&t);
        assert_eq!(idx, 1);
        assert_eq!(name, "user_id");
    }

    #[test]
    fn pick_primary_key_falls_back_to_id_naming() {
        let t = make_table(
            "users",
            vec![
                make_col("name", false),
                make_col("id", false), // naming convention
                make_col("email", false),
            ],
        );
        let (idx, name) = pick_primary_key(&t);
        assert_eq!(idx, 1);
        assert_eq!(name, "id");
    }

    #[test]
    fn pick_primary_key_falls_back_to_user_id_for_users_table() {
        let t = make_table(
            "users",
            vec![
                make_col("name", false),
                make_col("email", false),
                make_col("user_id", false),
            ],
        );
        let (idx, name) = pick_primary_key(&t);
        assert_eq!(idx, 2);
        assert_eq!(name, "user_id");
    }

    #[test]
    fn pick_primary_key_last_resort_is_first_column() {
        let t = make_table(
            "logs",
            vec![
                make_col("timestamp", false),
                make_col("level", false),
                make_col("message", false),
            ],
        );
        let (idx, name) = pick_primary_key(&t);
        assert_eq!(idx, 0);
        assert_eq!(name, "timestamp");
    }

    #[test]
    fn type_tag_handles_string_and_object_forms() {
        assert_eq!(type_tag(&serde_json::json!("String")), "String");
        assert_eq!(type_tag(&serde_json::json!({"U64": []})), "U64");
        assert_eq!(type_tag(&serde_json::json!(null)), "Unknown");
    }

    #[test]
    fn extract_field_name_strips_type_suffix() {
        assert_eq!(extract_field_name("name (String)"), "name");
        assert_eq!(extract_field_name("user_id (U64 — auto)"), "user_id");
        assert_eq!(extract_field_name(""), "");
    }

    #[test]
    fn coerce_field_to_json_numeric_types() {
        assert_eq!(coerce_field_to_json("42", "U64"), serde_json::json!(42));
        assert_eq!(coerce_field_to_json("-7", "I32"), serde_json::json!(-7));
        assert_eq!(coerce_field_to_json("1.5", "F64"), serde_json::json!(1.5));
    }

    #[test]
    fn coerce_field_to_json_bool_and_string() {
        assert_eq!(
            coerce_field_to_json("true", "Bool"),
            serde_json::json!(true)
        );
        assert_eq!(
            coerce_field_to_json("hello", "String"),
            serde_json::json!("hello")
        );
    }

    #[test]
    fn coerce_field_to_json_passes_through_json_arrays() {
        assert_eq!(
            coerce_field_to_json("[1,2,3]", "Array"),
            serde_json::json!([1, 2, 3])
        );
    }

    #[test]
    fn coerce_field_to_json_empty_is_null() {
        assert_eq!(
            coerce_field_to_json("   ", "String"),
            serde_json::Value::Null
        );
    }

    #[test]
    fn sql_literal_quotes_strings_and_escapes_quotes() {
        assert_eq!(sql_literal("alice", "String"), "'alice'");
        assert_eq!(sql_literal("O'Brien", "String"), "'O''Brien'");
    }

    #[test]
    fn sql_literal_emits_numbers_bare() {
        assert_eq!(sql_literal("42", "U64"), "42");
        assert_eq!(sql_literal("-1.5", "F32"), "-1.5");
    }

    #[test]
    fn sql_literal_bool_to_keyword() {
        assert_eq!(sql_literal("true", "Bool"), "TRUE");
        assert_eq!(sql_literal("false", "Bool"), "FALSE");
        assert_eq!(sql_literal("1", "Bool"), "TRUE");
        assert_eq!(sql_literal("nope", "Bool"), "FALSE");
    }

    #[test]
    fn sql_literal_empty_is_null() {
        assert_eq!(sql_literal("  ", "String"), "NULL");
    }

    // ── json_to_sql_literal ────────────────────────────────────────────

    #[test]
    fn json_literal_scalars() {
        assert_eq!(
            json_to_sql_literal(&serde_json::json!(null)).unwrap(),
            "NULL"
        );
        assert_eq!(
            json_to_sql_literal(&serde_json::json!(true)).unwrap(),
            "TRUE"
        );
        assert_eq!(
            json_to_sql_literal(&serde_json::json!(false)).unwrap(),
            "FALSE"
        );
        assert_eq!(json_to_sql_literal(&serde_json::json!(42)).unwrap(), "42");
        assert_eq!(json_to_sql_literal(&serde_json::json!(-17)).unwrap(), "-17");
        assert_eq!(json_to_sql_literal(&serde_json::json!(1.5)).unwrap(), "1.5");
    }

    #[test]
    fn json_literal_plain_string_is_single_quoted() {
        let lit = json_to_sql_literal(&serde_json::json!("alice")).unwrap();
        assert_eq!(lit, "'alice'");
    }

    #[test]
    fn json_literal_string_escapes_embedded_quotes() {
        let lit = json_to_sql_literal(&serde_json::json!("O'Brien")).unwrap();
        assert_eq!(lit, "'O''Brien'");
    }

    #[test]
    fn json_literal_hex_string_passes_through() {
        // Identity / ConnectionId serialised as a bare hex string
        // must round-trip as a raw `0x…` literal — *not* as a
        // single-quoted string.
        let lit = json_to_sql_literal(&serde_json::json!("0xdeadbeef")).unwrap();
        assert_eq!(lit, "0xdeadbeef");
    }

    #[test]
    fn json_literal_identity_object_tagged_form() {
        // `{"Identity": "0xabc"}` → `0xabc`
        let v = serde_json::json!({"Identity": "0xdeadbeef"});
        assert_eq!(json_to_sql_literal(&v).unwrap(), "0xdeadbeef");
    }

    #[test]
    fn json_literal_identity_object_dunder_form() {
        // `{"__identity__": "0xabc"}` → `0xabc`
        let v = serde_json::json!({"__identity__": "0xc2005e40a5b1"});
        assert_eq!(json_to_sql_literal(&v).unwrap(), "0xc2005e40a5b1");
    }

    #[test]
    fn json_literal_identity_object_without_0x_prefix() {
        // If the server drops the `0x` prefix, re-add it so the
        // literal is still valid SpacetimeDB hex syntax.
        let v = serde_json::json!({"__identity__": "deadbeef"});
        assert_eq!(json_to_sql_literal(&v).unwrap(), "0xdeadbeef");
    }

    #[test]
    fn json_literal_connection_id_object() {
        let v = serde_json::json!({"ConnectionId": "0xfeed"});
        assert_eq!(json_to_sql_literal(&v).unwrap(), "0xfeed");
    }

    #[test]
    fn json_literal_u256_string_form() {
        // Large integers come back as strings to preserve precision.
        let v = serde_json::json!({"U256": "123456789012345678901234567890"});
        assert_eq!(
            json_to_sql_literal(&v).unwrap(),
            "123456789012345678901234567890"
        );
    }

    #[test]
    fn json_literal_array_is_error() {
        let v = serde_json::json!([1, 2, 3]);
        let err = json_to_sql_literal(&v).unwrap_err();
        assert!(err.contains("array"));
    }

    #[test]
    fn json_literal_unknown_object_is_error() {
        let v = serde_json::json!({"WeirdCustomType": {"foo": 1}});
        let err = json_to_sql_literal(&v).unwrap_err();
        assert!(err.contains("complex value"));
    }

    #[test]
    fn json_literal_bare_hex_long_string_gets_prefix() {
        // Some server versions emit Identity as a bare hex string
        // without the `0x` prefix. A 64-char all-hex string must
        // round-trip as a hex literal, not a quoted one.
        let v = serde_json::json!("c2005e40a5b1576e629a78ae0deef2fbbd6449ba1f150a8dcf76d312c47e2f");
        let lit = json_to_sql_literal(&v).unwrap();
        assert!(lit.starts_with("0x"));
    }

    #[test]
    fn json_literal_short_hex_like_string_stays_quoted() {
        // Short strings that *happen* to be hex-only (`"cafe"`,
        // `"deadbeef"` as a nickname) must still be treated as
        // regular strings — the heuristic is only safe for lengths
        // that are clearly identity hashes.
        for short in ["ab", "cafe", "deadbeef"] {
            let v = serde_json::json!(short);
            let lit = json_to_sql_literal(&v).unwrap();
            assert!(
                lit.starts_with('\''),
                "{short:?} → {lit:?} should be quoted, not hex"
            );
        }
    }

    #[test]
    fn sql_literal_identity_type_emits_hex() {
        // A user typing a 0x-prefixed hex value into an Identity
        // form field must not end up single-quoted.
        assert_eq!(sql_literal("0xdeadbeef", "Identity"), "0xdeadbeef");
        assert_eq!(sql_literal("0xfeed", "ConnectionId"), "0xfeed");
    }

    #[test]
    fn sql_literal_identity_type_without_prefix_adds_0x() {
        // Same thing but without the `0x` the user omitted.
        assert_eq!(sql_literal("deadbeef", "Identity"), "0xdeadbeef");
    }

    #[test]
    fn sql_literal_identity_type_non_hex_falls_through_to_quoted() {
        // Garbage input for an Identity column shouldn't silently
        // produce a bad hex literal — let the server reject a
        // quoted string so the user sees an error.
        let lit = sql_literal("not-hex!", "Identity");
        assert_eq!(lit, "'not-hex!'");
    }
}

// ── Frame renderer ────────────────────────────────────────────────────────────

/// Draw the complete UI frame.
pub fn draw_frame(
    frame: &mut ratatui::Frame,
    state: &mut AppState,
    sql_input: &InputState,
    tables_grid: &mut TableGridState,
    sql_grid: &mut TableGridState,
) {
    use crate::ui::{
        components::{
            error::render_error, help::HelpOverlay, modal::render_modal, palette::render_palette,
            status_bar::StatusBar,
        },
        layout::render_layout,
        sidebar::render_sidebar,
        tabs::{
            live::render_live, logs::render_logs, metrics::render_metrics, module::render_module,
            sql::render_sql, tables::render_tables,
        },
    };
    use ratatui::layout::{Constraint, Direction, Layout};

    let area = frame.area();

    // ── Outer layout: content + status bar ───────────────────────────────
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);

    let main_area = outer[0];
    let status_area = outer[1];

    // ── Render chrome (title bar, tab bar, sidebar border) ────────────────
    let content_areas = render_layout(main_area, frame.buffer_mut(), state);

    // ── Sidebar ───────────────────────────────────────────────────────────
    render_sidebar(content_areas.sidebar, frame.buffer_mut(), state);

    // ── Tab content ───────────────────────────────────────────────────────
    match state.current_tab {
        crate::state::Tab::Tables => {
            render_tables(
                content_areas.content,
                frame.buffer_mut(),
                state,
                tables_grid,
            );
        }
        crate::state::Tab::Sql => {
            render_sql(
                content_areas.content,
                frame.buffer_mut(),
                state,
                sql_input,
                sql_grid,
            );
        }
        crate::state::Tab::Logs => {
            render_logs(content_areas.content, frame.buffer_mut(), state);
        }
        crate::state::Tab::Metrics => {
            render_metrics(content_areas.content, frame.buffer_mut(), state);
        }
        crate::state::Tab::Module => {
            let selected = state.module_selected_reducer;
            render_module(content_areas.content, frame.buffer_mut(), state, selected);
        }
        crate::state::Tab::Live => {
            render_live(content_areas.content, frame.buffer_mut(), state);
        }
    }

    // ── Status bar ────────────────────────────────────────────────────────
    StatusBar::new(state).render(status_area, frame.buffer_mut());

    // ── Error overlay ─────────────────────────────────────────────────────
    // Rendered above the tab content but below help/modal/palette, mirroring
    // the input precedence in `handle_key` (those overlays capture keys
    // before the error does). While an error is shown every key except
    // Esc/Enter is swallowed, so it must be plainly visible — not a 40-char
    // sliver in the status-bar corner.
    if let Some(ref err) = state.error_message {
        render_error(area, frame.buffer_mut(), err);
    }

    // ── Help overlay (drawn on top of everything) ─────────────────────────
    if state.show_help {
        HelpOverlay::new(state.help_scroll).render(area, frame.buffer_mut());
    }

    // ── Modal dialog (drawn last so it's always on top) ──────────────────
    if let Some(ref modal) = state.modal {
        render_modal(area, frame.buffer_mut(), modal);
    }

    // ── Command palette (always on top, even above modals) ──────────────
    if let Some(ref palette) = state.palette {
        render_palette(area, frame.buffer_mut(), palette);
    }
}
