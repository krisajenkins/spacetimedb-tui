//! Application state module.
//!
//! The [`app_state`] sub-module contains [`AppState`], the single source of
//! truth for all TUI state.  Import from here rather than from the sub-module
//! directly.

pub mod app_state;
pub mod edit_mode;
pub mod modal;
pub mod palette;

// Re-export the most commonly used items.
pub use app_state::{
    AppState, ConnectionStatus, Database, DatabaseStatus, FocusPanel, HistoryAdvance,
    MetricsSnapshot, SidebarFocus, SqlHistoryEntry, Tab, TxLogEntry,
};
