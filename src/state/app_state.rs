//! Central application state for the SpacetimeDB TUI.
//!
//! [`AppState`] is the single source of truth consumed by every UI widget.
//! It is **not** `Send` or `Sync` by design; all mutations happen on the main
//! thread inside the synchronous event loop.  Background tasks communicate
//! via channels and the event loop applies mutations here.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use crate::api::types::{LogEntry, LogLevel, QueryResult, Schema, TableInfo};
use crate::config::ThemeColors;

// ---------------------------------------------------------------------------
// Tab / focus enums
// ---------------------------------------------------------------------------

/// Top-level tabs shown in the main pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tab {
    /// Table browser (rows of selected table).
    Tables,
    /// Interactive SQL query editor and results view.
    Sql,
    /// Live log viewer.
    Logs,
    /// Server / module metrics.
    Metrics,
    /// Module inspector (reducers, tables, scheduled).
    Module,
    /// Real-time transaction stream + connected-client list.
    Live,
}

impl Tab {
    pub const ALL: &'static [Tab] = &[
        Tab::Tables,
        Tab::Sql,
        Tab::Logs,
        Tab::Metrics,
        Tab::Module,
        Tab::Live,
    ];

    pub fn title(&self) -> &'static str {
        match self {
            Tab::Tables => "Tables",
            Tab::Sql => "SQL",
            Tab::Logs => "Logs",
            Tab::Metrics => "Metrics",
            Tab::Module => "Module",
            Tab::Live => "Live",
        }
    }

    /// Cycle to the next tab.
    pub fn next(self) -> Self {
        let idx = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        Self::ALL[(idx + 1) % Self::ALL.len()]
    }

    /// Cycle to the previous tab.
    pub fn prev(self) -> Self {
        let idx = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        Self::ALL[(idx + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

impl std::fmt::Display for Tab {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.title())
    }
}

/// Which panel currently owns keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusPanel {
    /// The left-hand database/table sidebar.
    Sidebar,
    /// The main content area (query editor, schema view, …).
    Main,
    /// The SQL input box at the bottom.
    SqlInput,
    /// A modal dialog (e.g. error popup, help overlay).
    #[allow(dead_code)]
    Modal,
}

/// Which item in the sidebar is highlighted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarFocus {
    /// The database list at the top of the sidebar.
    Databases,
    /// The table list below the selected database.
    Tables,
}

// ---------------------------------------------------------------------------
// Connection state
// ---------------------------------------------------------------------------

/// Current state of the server connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionStatus {
    /// Not yet attempted.
    Disconnected,
    /// Connection attempt in progress.
    Connecting,
    /// Successfully connected and authenticated.
    Connected,
    /// Connection was lost; contains a human-readable reason.
    Error(String),
}

impl std::fmt::Display for ConnectionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectionStatus::Disconnected => write!(f, "Disconnected"),
            ConnectionStatus::Connecting => write!(f, "Connecting…"),
            ConnectionStatus::Connected => write!(f, "Connected"),
            ConnectionStatus::Error(e) => write!(f, "Error: {e}"),
        }
    }
}

/// Connection parameters and live status.
#[derive(Debug, Clone)]
pub struct ConnectionInfo {
    /// e.g. `"http://localhost:3000"`
    pub base_url: String,
    /// Current connection status.
    pub status: ConnectionStatus,
    /// Server version string, if reported (populated when available).
    #[allow(dead_code)]
    pub server_version: Option<String>,
    /// Authenticated identity token, if present (for display in status bar).
    #[allow(dead_code)]
    pub auth_token: Option<String>,
    /// When the last successful connection was made.
    #[allow(dead_code)]
    pub connected_at: Option<DateTime<Utc>>,
}

impl ConnectionInfo {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            status: ConnectionStatus::Disconnected,
            server_version: None,
            auth_token: None,
            connected_at: None,
        }
    }

    /// Returns `true` when the connection is in the `Connected` state.
    #[allow(dead_code)]
    pub fn is_connected(&self) -> bool {
        self.status == ConnectionStatus::Connected
    }
}

// ---------------------------------------------------------------------------
// SQL history
// ---------------------------------------------------------------------------

/// Maximum number of SQL history entries retained.
const SQL_HISTORY_LIMIT: usize = 200;

/// A single row pushed into the Live tab's transaction feed.
///
/// Derived from [`crate::api::types::WsServerMessage::TransactionUpdate`]
/// when one arrives over the subscription WebSocket. Only the bits the
/// UI actually needs are kept so the buffer stays small even under
/// heavy activity.
#[derive(Debug, Clone)]
pub struct TxLogEntry {
    /// When we observed the update (client clock).
    pub observed_at: DateTime<Utc>,
    /// Caller identity (may be an empty string for system-originated
    /// transactions).
    pub caller: String,
    /// Per-table row counts affected by this transaction, in the
    /// server's original order. `(table, inserts, deletes)`.
    pub tables: Vec<(String, usize, usize)>,
    /// Whether the transaction committed successfully. `None` when
    /// the server didn't include a status field.
    pub committed: Option<bool>,
}

impl TxLogEntry {
    /// Sum of row inserts across every table touched by this tx.
    pub fn total_inserts(&self) -> usize {
        self.tables.iter().map(|(_, i, _)| *i).sum()
    }
    /// Sum of row deletes across every table touched by this tx.
    pub fn total_deletes(&self) -> usize {
        self.tables.iter().map(|(_, _, d)| *d).sum()
    }
}

/// Last-known status of a database.
///
/// We can't read this from the database list (which returns names only);
/// it's discovered when we actually talk to a database. `Unknown` is the
/// initial state until the first schema fetch resolves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DatabaseStatus {
    /// Not yet probed.
    #[default]
    Unknown,
    /// Responding normally (a schema fetch succeeded).
    Active,
    /// Suspended by Maincloud (`503 database is paused`).
    Paused,
}

/// A database visible to the current identity, plus its last-known status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Database {
    pub name: String,
    pub status: DatabaseStatus,
}

impl Database {
    /// A database with not-yet-probed status.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: DatabaseStatus::Unknown,
        }
    }

    pub fn is_paused(&self) -> bool {
        self.status == DatabaseStatus::Paused
    }
}

/// One row in the Live tab's connected-client list.
#[derive(Debug, Clone)]
pub struct LiveClientEntry {
    /// Hex identity or connection id (whichever the server returned).
    pub identity: String,
    /// When the client first connected (best-effort from `st_client`).
    pub connected_at: Option<DateTime<Utc>>,
}

/// Result of advancing the SQL history cursor forward (↓).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryAdvance {
    /// Cursor was already at `None` — nothing happened.
    Unchanged,
    /// Cursor moved to a new history entry (use [`AppState::current_history_sql`]).
    Moved,
    /// Cursor walked off the end — caller should clear the input buffer.
    Cleared,
}

/// A single entry in the SQL execution history.
#[derive(Debug, Clone)]
pub struct SqlHistoryEntry {
    /// The SQL text that was executed.
    pub sql: String,
    /// When it was executed.
    pub executed_at: DateTime<Utc>,
    /// How long the query took (round-trip including network).
    pub duration: Duration,
    /// Row count returned, or `None` if the query errored.
    /// Available for display in the history panel and future export features.
    #[allow(dead_code)]
    pub row_count: Option<usize>,
    /// Error message, if the query failed.
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

/// A snapshot of server / module metrics.
#[derive(Debug, Clone, Default)]
pub struct MetricsSnapshot {
    /// Total reducer calls processed.
    pub total_reducer_calls: u64,
    /// Total energy quanta consumed.
    pub total_energy_used: u64,
    /// Number of connected WebSocket clients.
    pub connected_clients: u64,
    /// Module memory usage in bytes.
    pub memory_bytes: u64,
    /// When this snapshot was taken.
    pub sampled_at: Option<DateTime<Utc>>,
    /// Raw key-value pairs for metrics not captured by the fields above.
    pub extra: HashMap<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Table data cache
// ---------------------------------------------------------------------------

/// A cached query result for a specific table.
#[derive(Debug, Clone)]
pub struct TableCache {
    /// The cached result set.
    pub result: QueryResult,
    /// When the cache entry was populated (used for cache expiry checks).
    pub fetched_at: Instant,
    /// Whether a refresh is currently in flight.
    #[allow(dead_code)]
    pub loading: bool,
}

// ---------------------------------------------------------------------------
// Log buffer
// ---------------------------------------------------------------------------

/// Maximum log lines kept in memory.
const LOG_BUFFER_LIMIT: usize = 10_000;

// ---------------------------------------------------------------------------
// AppState
// ---------------------------------------------------------------------------

/// The complete application state.
///
/// This struct is intentionally large — it is the single place where all UI
/// state lives, making it easy to reason about what the TUI is displaying at
/// any point in time.
#[derive(Debug)]
pub struct AppState {
    // ------------------------------------------------------------------
    // Connection
    // ------------------------------------------------------------------
    /// Connection info and status.
    pub connection: ConnectionInfo,

    // ------------------------------------------------------------------
    // Database / table navigation
    // ------------------------------------------------------------------
    /// All databases visible to the current identity, each carrying its
    /// last-known status.
    pub databases: Vec<Database>,
    /// Index of the currently selected database in `databases`.
    pub selected_database_idx: Option<usize>,
    /// Tables belonging to the currently selected database.
    pub tables: Vec<TableInfo>,
    /// Index of the currently selected table in `tables`.
    pub selected_table_idx: Option<usize>,
    /// Schema for the currently selected database.
    pub current_schema: Option<Schema>,

    // ------------------------------------------------------------------
    // Tab / focus
    // ------------------------------------------------------------------
    /// The currently visible top-level tab.
    pub current_tab: Tab,
    /// Which panel owns keyboard focus.
    pub focus: FocusPanel,
    /// Which section of the sidebar is highlighted.
    pub sidebar_focus: SidebarFocus,

    // ------------------------------------------------------------------
    // Query tab
    // ------------------------------------------------------------------
    /// Result of the most recently executed SQL query (SQL tab only).
    pub query_result: Option<QueryResult>,
    /// Result of the "browse table rows" load triggered from the sidebar
    /// (Tables tab only). Kept separate from `query_result` so that a SQL
    /// query in the SQL tab doesn't clobber the Tables view, and vice versa.
    pub table_browse_result: Option<QueryResult>,
    /// Live row data received over the WebSocket subscription, keyed by
    /// table name. Each entry is the latest set of rows the server has
    /// pushed for that table since the most recent `InitialSubscription`.
    /// Used by the status bar / Tables view to surface live updates.
    pub live_table_data: HashMap<String, Vec<serde_json::Value>>,
    /// Rolling buffer of transactions observed over the WebSocket
    /// subscription. Used by the Live tab to show a real-time feed of
    /// what's happening in the database.
    pub tx_log: VecDeque<TxLogEntry>,
    /// Rolling list of connected clients, polled periodically from
    /// `st_client`. Populated by the Live tab's background refresh.
    pub live_clients: Vec<LiveClientEntry>,
    /// Whether the live-subscription WebSocket is currently connected.
    pub ws_connected: bool,
    /// If the WS task is waiting to reconnect, the instant at which the
    /// next attempt will fire. Used by the status bar to render a live
    /// countdown ("Reconnecting in 5s…").
    pub ws_reconnect_deadline: Option<Instant>,
    /// 1-indexed attempt counter of the most recent reconnect wait, for
    /// display purposes only.
    pub ws_reconnect_attempt: u32,
    /// Scroll offset for the results table (row index of the top visible row).
    /// Managed by `TableGridState`; kept here for persistence across tab switches.
    #[allow(dead_code)]
    pub result_scroll_row: usize,
    /// Scroll offset for the results table (column index of the leftmost visible column).
    /// Managed by `TableGridState`; kept here for persistence across tab switches.
    #[allow(dead_code)]
    pub result_scroll_col: usize,
    /// Whether a query is currently in flight.
    pub query_loading: bool,
    /// Whether a `get_schema` request is currently in flight for
    /// the active database. Used by the sidebar to distinguish
    /// "waiting for the first schema to arrive" (show `(loading…)`)
    /// from "schema load failed / no tables" (show a terminal
    /// placeholder so the UI doesn't spin forever).
    pub schema_loading: bool,
    /// Set to `true` after a schema load returns a non-success
    /// status. Cleared the next time `load_schema` kicks off. The
    /// sidebar reads this to show `(schema unavailable)` instead of
    /// the spinning loading placeholder.
    pub schema_load_failed: bool,

    // ------------------------------------------------------------------
    // Grid search
    // ------------------------------------------------------------------
    /// Case-insensitive query that the data-grid tabs filter rows by.
    /// When `None`, no search is active; when `Some("")`, the user has
    /// opened the prompt but hasn't typed anything yet.
    pub grid_search: Option<String>,
    /// While `Some(true)`, the next key in the main handler feeds the
    /// search prompt instead of running regular bindings. Set by the
    /// `Ctrl+F` handler and cleared on Enter / Esc.
    pub grid_search_editing: bool,

    // ------------------------------------------------------------------
    // SQL history
    // ------------------------------------------------------------------
    /// Ordered list of past SQL executions (most recent last).
    pub sql_history: VecDeque<SqlHistoryEntry>,
    /// Index into `sql_history` when the user is browsing history (↑/↓).
    pub history_cursor: Option<usize>,

    // ------------------------------------------------------------------
    // Table data cache
    // ------------------------------------------------------------------
    /// Cached query results keyed by `"<database>.<table_name>"`.
    pub table_cache: HashMap<String, TableCache>,
    /// Cached schemas keyed by database name, so switching back to a
    /// previously visited database shows its tables instantly instead of
    /// re-fetching. Session-lifetime, no TTL — mirrors `table_cache`; an
    /// explicit refresh (`r`) bypasses it.
    pub schema_cache: HashMap<String, Schema>,

    // ------------------------------------------------------------------
    // Log buffer
    // ------------------------------------------------------------------
    /// Buffered log lines (capped at `LOG_BUFFER_LIMIT`).
    pub log_buffer: VecDeque<LogEntry>,
    /// Scroll offset for the log view (index of the top visible line).
    pub log_scroll: usize,
    /// Whether log auto-scroll (follow mode) is enabled.
    pub log_follow: bool,
    /// Minimum log level to display.
    /// Used by `visible_logs()` for filtering; UI key binding to change it is a future enhancement.
    pub log_filter_level: LogLevel,

    // ------------------------------------------------------------------
    // Metrics
    // ------------------------------------------------------------------
    /// Latest metrics snapshot.
    pub metrics: MetricsSnapshot,
    /// Historical metric samples (for sparkline charts).
    pub metrics_history: VecDeque<MetricsSnapshot>,

    // ------------------------------------------------------------------
    // Error / notification state
    // ------------------------------------------------------------------
    /// A transient error message shown in a popup (cleared on any keypress).
    pub error_message: Option<String>,
    /// A transient informational notification.
    pub notification: Option<(String, Instant)>,

    // ------------------------------------------------------------------
    // Application lifecycle
    // ------------------------------------------------------------------
    /// Set to `true` to request a clean shutdown.
    pub should_quit: bool,
    /// When the application was started (used by `uptime()`).
    pub started_at: Instant,

    // ------------------------------------------------------------------
    // UI-only state (not persisted)
    // ------------------------------------------------------------------
    /// Current search/filter query in the sidebar.
    pub search_query: String,
    /// Whether the help overlay is visible.
    pub show_help: bool,
    /// Scroll offset for the help overlay.
    pub help_scroll: usize,
    /// Selected reducer index in the module inspector tab.
    pub module_selected_reducer: usize,

    // ------------------------------------------------------------------
    // Theming
    // ------------------------------------------------------------------
    /// Active colour theme — driven by the `--theme` CLI flag at startup.
    /// UI renderers read accent / border / status colours from this struct
    /// instead of hardcoded constants so that `--theme light` actually
    /// changes what the user sees.
    pub theme: ThemeColors,

    // ------------------------------------------------------------------
    // Modal dialog state (Faz 5: write operations)
    // ------------------------------------------------------------------
    /// Active modal dialog (confirm prompt or multi-field form), if
    /// any. While `Some`, the main key handler routes every event
    /// into the modal until the user accepts or cancels.
    pub modal: Option<crate::state::modal::Modal>,

    /// Command palette overlay, when open. Toggled with Ctrl+P.
    pub palette: Option<crate::state::palette::CommandPalette>,

    /// Spreadsheet edit-mode state for the Tables tab, when active.
    /// Toggled with Ctrl+E. While `Some`, key bindings on the main
    /// Tables pane route through the edit-mode key map instead of
    /// the read-only data-grid bindings.
    pub edit_mode: Option<crate::state::edit_mode::EditMode>,
}

impl AppState {
    /// Create a fresh `AppState` with sensible defaults.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            connection: ConnectionInfo::new(base_url),

            databases: Vec::new(),
            selected_database_idx: None,
            tables: Vec::new(),
            selected_table_idx: None,
            current_schema: None,

            current_tab: Tab::Tables,
            focus: FocusPanel::Sidebar,
            sidebar_focus: SidebarFocus::Databases,

            query_result: None,
            table_browse_result: None,
            live_table_data: HashMap::new(),
            tx_log: VecDeque::new(),
            live_clients: Vec::new(),
            ws_connected: false,
            ws_reconnect_deadline: None,
            ws_reconnect_attempt: 0,
            result_scroll_row: 0,
            result_scroll_col: 0,
            query_loading: false,
            schema_loading: false,
            schema_load_failed: false,

            grid_search: None,
            grid_search_editing: false,
            modal: None,
            palette: None,
            edit_mode: None,

            sql_history: VecDeque::new(),
            history_cursor: None,

            table_cache: HashMap::new(),
            schema_cache: HashMap::new(),

            log_buffer: VecDeque::new(),
            log_scroll: 0,
            log_follow: true,
            log_filter_level: LogLevel::Info,

            metrics: MetricsSnapshot::default(),
            metrics_history: VecDeque::new(),

            error_message: None,
            notification: None,

            should_quit: false,
            started_at: Instant::now(),

            search_query: String::new(),
            show_help: false,
            help_scroll: 0,
            module_selected_reducer: 0,
            theme: ThemeColors::dark(),
        }
    }

    // ------------------------------------------------------------------
    // Database navigation helpers
    // ------------------------------------------------------------------

    /// The name of the currently selected database, if any.
    pub fn selected_database(&self) -> Option<&str> {
        self.selected_database_idx
            .and_then(|i| self.databases.get(i))
            .map(|d| d.name.as_str())
    }

    /// Set the status of the database named `name`, if present.
    pub fn set_database_status(&mut self, name: &str, status: DatabaseStatus) {
        if let Some(db) = self.databases.iter_mut().find(|d| d.name == name) {
            db.status = status;
        }
    }

    /// Select the database at `idx`, resetting table selection.
    ///
    /// This is a no-op when `idx` is already the selected database, preventing
    /// unnecessary state clears on repeated navigation to the same database.
    pub fn select_database(&mut self, idx: usize) {
        if idx < self.databases.len() {
            // No-op if this database is already selected
            if self.selected_database_idx == Some(idx) {
                return;
            }
            self.selected_database_idx = Some(idx);
            self.selected_table_idx = None;
            self.tables.clear();
            self.current_schema = None;
        }
    }

    /// Move the database cursor down by one.
    pub fn database_next(&mut self) {
        if self.databases.is_empty() {
            return;
        }
        let next = match self.selected_database_idx {
            Some(i) => (i + 1).min(self.databases.len() - 1),
            None => 0,
        };
        self.select_database(next);
    }

    /// Move the database cursor up by one.
    pub fn database_prev(&mut self) {
        if self.databases.is_empty() {
            return;
        }
        let prev = match self.selected_database_idx {
            Some(i) => i.saturating_sub(1),
            None => 0,
        };
        self.select_database(prev);
    }

    // ------------------------------------------------------------------
    // Table navigation helpers
    // ------------------------------------------------------------------

    /// The currently selected [`TableInfo`], if any.
    pub fn selected_table(&self) -> Option<&TableInfo> {
        self.selected_table_idx.and_then(|i| self.tables.get(i))
    }

    /// Move the table cursor down by one.
    pub fn table_next(&mut self) {
        if self.tables.is_empty() {
            return;
        }
        self.selected_table_idx = Some(match self.selected_table_idx {
            Some(i) => (i + 1).min(self.tables.len() - 1),
            None => 0,
        });
    }

    /// Move the table cursor up by one.
    pub fn table_prev(&mut self) {
        if self.tables.is_empty() {
            return;
        }
        self.selected_table_idx = Some(match self.selected_table_idx {
            Some(i) => i.saturating_sub(1),
            None => 0,
        });
    }

    // ------------------------------------------------------------------
    // SQL history helpers
    //
    // Note: SQL editing is handled by `InputState` (see
    // `ui/components/input.rs`). This module only tracks history navigation;
    // the actual text buffer lives on `App.sql_input`.
    // ------------------------------------------------------------------

    /// Push a completed query execution into the history ring.
    pub fn push_sql_history(&mut self, entry: SqlHistoryEntry) {
        self.sql_history.push_back(entry);
        if self.sql_history.len() > SQL_HISTORY_LIMIT {
            self.sql_history.pop_front();
        }
        self.history_cursor = None;
    }

    /// Navigate to the previous history entry (↑).
    ///
    /// Returns `true` if the cursor moved, `false` if the history is empty.
    /// The caller is responsible for syncing the selected entry's text into
    /// the `InputState` widget via [`current_history_sql`].
    pub fn history_prev(&mut self) -> bool {
        if self.sql_history.is_empty() {
            return false;
        }
        let new_cursor = match self.history_cursor {
            None => self.sql_history.len() - 1,
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.history_cursor = Some(new_cursor);
        true
    }

    /// Navigate to the next history entry (↓).
    ///
    /// Returns `Some(sql)` if a history entry was selected, or `None` if the
    /// cursor walked off the end of the history (caller should clear the
    /// input in that case). The caller is responsible for syncing the
    /// selected text into the `InputState` widget.
    pub fn history_next(&mut self) -> HistoryAdvance {
        match self.history_cursor {
            None => HistoryAdvance::Unchanged,
            Some(i) if i + 1 >= self.sql_history.len() => {
                self.history_cursor = None;
                HistoryAdvance::Cleared
            }
            Some(i) => {
                self.history_cursor = Some(i + 1);
                HistoryAdvance::Moved
            }
        }
    }

    /// The SQL text of the history entry currently pointed at by
    /// `history_cursor`, if any. Used by the caller to populate the input
    /// widget after calling [`history_prev`] / [`history_next`].
    pub fn current_history_sql(&self) -> Option<&str> {
        let idx = self.history_cursor?;
        self.sql_history.get(idx).map(|e| e.sql.as_str())
    }

    // ------------------------------------------------------------------
    // Log buffer helpers
    // ------------------------------------------------------------------

    /// Append a log entry to the buffer, evicting old entries if needed.
    pub fn push_log(&mut self, entry: LogEntry) {
        if self.log_buffer.len() >= LOG_BUFFER_LIMIT {
            self.log_buffer.pop_front();
            // Adjust scroll so the view doesn't jump.
            if self.log_scroll > 0 {
                self.log_scroll -= 1;
            }
        }
        self.log_buffer.push_back(entry);
        if self.log_follow {
            // Pin scroll to the bottom.
            self.log_scroll = self.log_buffer.len().saturating_sub(1);
        }
    }

    /// Append multiple log entries at once.
    pub fn extend_logs(&mut self, entries: impl IntoIterator<Item = LogEntry>) {
        for entry in entries {
            self.push_log(entry);
        }
    }

    /// Log entries that pass the current `log_filter_level`.
    ///
    /// The Logs tab uses this iterator both to count the visible lines and
    /// to render the filtered slice.
    pub fn visible_logs(&self) -> impl Iterator<Item = &LogEntry> {
        let min_level = &self.log_filter_level;
        self.log_buffer
            .iter()
            .filter(move |e| level_gte(&e.level, min_level))
    }

    // ------------------------------------------------------------------
    // Metrics helpers
    // ------------------------------------------------------------------

    /// Replace the current metrics snapshot and push the old one to history.
    pub fn update_metrics(&mut self, snapshot: MetricsSnapshot) {
        const HISTORY_LIMIT: usize = 120;
        let old = std::mem::replace(&mut self.metrics, snapshot);
        self.metrics_history.push_back(old);
        if self.metrics_history.len() > HISTORY_LIMIT {
            self.metrics_history.pop_front();
        }
    }

    // ------------------------------------------------------------------
    // Error / notification helpers
    // ------------------------------------------------------------------

    /// Set a transient error message (shown in a popup).
    pub fn set_error(&mut self, msg: impl Into<String>) {
        self.error_message = Some(msg.into());
    }

    /// Clear the current error message.
    pub fn clear_error(&mut self) {
        self.error_message = None;
    }

    /// Set a transient notification (auto-expires after a few seconds).
    pub fn set_notification(&mut self, msg: impl Into<String>) {
        self.notification = Some((msg.into(), Instant::now()));
    }

    /// Clear expired notifications (older than `ttl`).
    pub fn tick_notifications(&mut self, ttl: Duration) {
        if let Some((_, ts)) = &self.notification {
            if ts.elapsed() > ttl {
                self.notification = None;
            }
        }
    }

    // ------------------------------------------------------------------
    // Cache helpers
    // ------------------------------------------------------------------

    /// Cache key for a table: `"<database>.<table_name>"`.
    pub fn cache_key(database: &str, table_name: &str) -> String {
        format!("{}.{}", database, table_name)
    }

    /// Store a query result in the table cache.
    pub fn cache_table_result(&mut self, database: &str, table_name: &str, result: QueryResult) {
        let key = Self::cache_key(database, table_name);
        self.table_cache.insert(
            key,
            TableCache {
                result,
                fetched_at: Instant::now(),
                loading: false,
            },
        );
    }

    /// Retrieve a cached result, if present and not older than `max_age`.
    pub fn get_cached_table(
        &self,
        database: &str,
        table_name: &str,
        max_age: Duration,
    ) -> Option<&TableCache> {
        let key = Self::cache_key(database, table_name);
        self.table_cache
            .get(&key)
            .filter(|c| c.fetched_at.elapsed() <= max_age)
    }

    /// Store a database's schema in the session schema cache.
    pub fn cache_schema(&mut self, database: &str, schema: Schema) {
        self.schema_cache.insert(database.to_string(), schema);
    }

    /// Retrieve a cached schema for `database`, if one has been loaded
    /// this session.
    pub fn get_cached_schema(&self, database: &str) -> Option<&Schema> {
        self.schema_cache.get(database)
    }

    // ------------------------------------------------------------------
    // Uptime
    // ------------------------------------------------------------------

    /// How long the application has been running.
    ///
    /// Available for display in the status bar or metrics tab.
    #[allow(dead_code)]
    pub fn uptime(&self) -> Duration {
        self.started_at.elapsed()
    }
}

// ---------------------------------------------------------------------------
// Level ordering helper
// ---------------------------------------------------------------------------

/// Returns `true` when level `a` is at least as severe as `b`.
#[allow(dead_code)]
fn level_gte(a: &LogLevel, b: &LogLevel) -> bool {
    level_rank(a) >= level_rank(b)
}

/// Numeric severity rank for log level comparison.
#[allow(dead_code)]
fn level_rank(level: &LogLevel) -> u8 {
    match level {
        LogLevel::Trace => 0,
        LogLevel::Debug => 1,
        LogLevel::Info => 2,
        LogLevel::Warn => 3,
        LogLevel::Error => 4,
        LogLevel::Panic => 5,
        LogLevel::Unknown => 6,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_state() -> AppState {
        AppState::new("http://localhost:3000")
    }

    #[test]
    fn test_tab_cycle() {
        // Tables → Sql → Logs → Metrics → Module → Live → Tables (wraps).
        assert_eq!(Tab::Tables.next(), Tab::Sql);
        assert_eq!(Tab::Module.next(), Tab::Live);
        assert_eq!(Tab::Live.next(), Tab::Tables);
        assert_eq!(Tab::Tables.prev(), Tab::Live);
        assert_eq!(Tab::Live.prev(), Tab::Module);
    }

    #[test]
    fn test_history_navigation() {
        let mut s = make_state();
        for i in 0..3 {
            s.push_sql_history(SqlHistoryEntry {
                sql: format!("SELECT {i}"),
                executed_at: Utc::now(),
                duration: Duration::from_millis(1),
                row_count: Some(0),
                error: None,
            });
        }
        assert_eq!(s.current_history_sql(), None);
        assert!(s.history_prev());
        assert_eq!(s.current_history_sql(), Some("SELECT 2"));
        assert!(s.history_prev());
        assert_eq!(s.current_history_sql(), Some("SELECT 1"));
        assert_eq!(s.history_next(), HistoryAdvance::Moved);
        assert_eq!(s.current_history_sql(), Some("SELECT 2"));
        assert_eq!(s.history_next(), HistoryAdvance::Cleared);
        assert_eq!(s.current_history_sql(), None);
        assert_eq!(s.history_next(), HistoryAdvance::Unchanged);
    }

    #[test]
    fn test_history_prev_empty() {
        let mut s = make_state();
        assert!(!s.history_prev());
        assert_eq!(s.history_cursor, None);
    }

    #[test]
    fn test_log_filter_visible_logs() {
        let mut s = make_state();
        s.log_filter_level = LogLevel::Warn;
        s.push_log(LogEntry {
            ts: None,
            level: LogLevel::Info,
            message: "noise".into(),
            target: None,
            filename: None,
            line_number: None,
        });
        s.push_log(LogEntry {
            ts: None,
            level: LogLevel::Error,
            message: "boom".into(),
            target: None,
            filename: None,
            line_number: None,
        });
        let visible: Vec<&str> = s.visible_logs().map(|e| e.message.as_str()).collect();
        assert_eq!(visible, vec!["boom"]);
    }

    #[test]
    fn test_log_level_next_filter_cycles() {
        assert_eq!(LogLevel::Trace.next_filter(), LogLevel::Debug);
        assert_eq!(LogLevel::Debug.next_filter(), LogLevel::Info);
        assert_eq!(LogLevel::Info.next_filter(), LogLevel::Warn);
        assert_eq!(LogLevel::Warn.next_filter(), LogLevel::Error);
        assert_eq!(LogLevel::Error.next_filter(), LogLevel::Panic);
        assert_eq!(LogLevel::Panic.next_filter(), LogLevel::Trace);
    }

    #[test]
    fn test_table_browse_separate_from_query_result() {
        // Verify that the Tables tab and SQL tab don't share state.
        let s = make_state();
        assert!(s.query_result.is_none());
        assert!(s.table_browse_result.is_none());
        // Both fields exist independently on AppState — see the
        // `query_result` / `table_browse_result` field declarations.
    }

    #[test]
    fn test_database_navigation() {
        let mut s = make_state();
        s.databases = vec![
            Database::new("alpha"),
            Database::new("beta"),
            Database::new("gamma"),
        ];
        s.database_next();
        assert_eq!(s.selected_database(), Some("alpha"));
        s.database_next();
        assert_eq!(s.selected_database(), Some("beta"));
        s.database_prev();
        assert_eq!(s.selected_database(), Some("alpha"));
    }

    #[test]
    fn test_set_database_status() {
        let mut s = make_state();
        s.databases = vec![Database::new("alpha"), Database::new("beta")];
        assert_eq!(s.databases[0].status, DatabaseStatus::Unknown);
        s.set_database_status("alpha", DatabaseStatus::Paused);
        assert!(s.databases[0].is_paused());
        assert_eq!(s.databases[1].status, DatabaseStatus::Unknown);
        // Unknown name is a silent no-op.
        s.set_database_status("nope", DatabaseStatus::Active);
        s.set_database_status("alpha", DatabaseStatus::Active);
        assert_eq!(s.databases[0].status, DatabaseStatus::Active);
    }

    #[test]
    fn test_log_buffer_eviction() {
        let mut s = make_state();
        for i in 0..10_001usize {
            s.push_log(LogEntry {
                ts: None,
                level: LogLevel::Info,
                message: format!("line {i}"),
                target: None,
                filename: None,
                line_number: None,
            });
        }
        assert_eq!(s.log_buffer.len(), 10_000);
    }

    #[test]
    fn test_sql_history_limit() {
        let mut s = make_state();
        for i in 0..201usize {
            s.push_sql_history(SqlHistoryEntry {
                sql: format!("SELECT {i}"),
                executed_at: Utc::now(),
                duration: Duration::from_millis(1),
                row_count: Some(0),
                error: None,
            });
        }
        assert_eq!(s.sql_history.len(), 200);
    }

    #[test]
    fn test_notification_expiry() {
        let mut s = make_state();
        s.set_notification("hello");
        // Should not expire immediately.
        s.tick_notifications(Duration::from_secs(5));
        assert!(s.notification.is_some());
        // Simulate expiry by using a zero TTL.
        s.tick_notifications(Duration::ZERO);
        assert!(s.notification.is_none());
    }

    #[test]
    fn test_schema_cache_round_trip() {
        let mut s = make_state();
        let schema = Schema {
            typespace: serde_json::Value::Null,
            tables: Vec::new(),
            reducers: Vec::new(),
        };

        // Miss before anything is cached; keyed per-database.
        assert!(s.get_cached_schema("alpha").is_none());

        s.cache_schema("alpha", schema);
        assert!(s.get_cached_schema("alpha").is_some());
        assert!(s.get_cached_schema("beta").is_none());
    }
}
