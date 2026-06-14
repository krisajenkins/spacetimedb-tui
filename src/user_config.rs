//! User-level config + session state for the SpacetimeDB TUI.
//!
//! Two files live under the platform config directory
//! (`~/.config/spacetimedb-tui/` on Linux, `~/Library/Application
//! Support/spacetimedb-tui/` on macOS, `%APPDATA%\spacetimedb-tui\` on
//! Windows — see [`config_dir`]):
//!
//! - **`config.toml`** — persistent user preferences (default theme,
//!   default database, optional `themes_dir`). Hand-written and never
//!   touched by the app.
//! - **`session.toml`** — last-known UI state (selected db, selected
//!   tab, last selected table). Written automatically on quit so the
//!   next launch can drop the user back where they left off.
//!
//! Both files are optional. Missing or unparseable files fall back to
//! defaults silently — `--host`, `--database`, `--theme` etc. on the
//! CLI always take precedence.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Filesystem helpers
// ---------------------------------------------------------------------------

/// Return the platform-specific config directory for the TUI:
///
/// - **Linux**:   `~/.config/spacetimedb-tui/`
/// - **macOS**:   `~/Library/Application Support/spacetimedb-tui/`
/// - **Windows**: `%APPDATA%\spacetimedb-tui\`
///
/// Returns `None` on the rare systems where `dirs` cannot locate a
/// config root (e.g. a stripped-down container with no `$HOME`).
pub fn config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("spacetimedb-tui"))
}

fn config_path() -> Option<PathBuf> {
    Some(config_dir()?.join("config.toml"))
}

fn session_path() -> Option<PathBuf> {
    Some(config_dir()?.join("session.toml"))
}

// ---------------------------------------------------------------------------
// User config (config.toml)
// ---------------------------------------------------------------------------

/// User preferences read from `~/.config/spacetimedb-tui/config.toml`.
///
/// Every field is optional so that a brand-new file with just one
/// setting still parses correctly. CLI args override anything we
/// pull out of here.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct UserConfig {
    /// Default theme to use when `--theme` is not passed.
    /// Accepted values: `"dark"`, `"light"`, `"high-contrast"`, or the
    /// stem of a TOML file under `themes_dir`.
    #[serde(default)]
    pub theme: Option<String>,
    /// Default database name / identity to open at startup when
    /// `--database` is not passed.
    #[serde(default)]
    pub default_database: Option<String>,
    /// Optional directory to look for `*.toml` theme files in. When
    /// unset, falls back to `~/.config/spacetimedb-tui/themes/`.
    #[serde(default)]
    pub themes_dir: Option<PathBuf>,
    /// Whether to restore the last selected db / tab / table on launch
    /// (default: `true`). Disable with `restore_session = false`.
    #[serde(default = "default_true")]
    pub restore_session: bool,
}

fn default_true() -> bool {
    true
}

impl UserConfig {
    /// Read the user config file. Missing or invalid → returns
    /// `UserConfig::default()`. Never panics, never returns an error.
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        let Ok(content) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        match toml::from_str::<UserConfig>(&content) {
            Ok(cfg) => cfg,
            Err(e) => {
                tracing::warn!(
                    "Could not parse {}: {e}; ignoring user config",
                    path.display()
                );
                Self::default()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Session state (session.toml)
// ---------------------------------------------------------------------------

/// Snapshot of the last UI state, written on quit and reloaded on
/// next launch (when `UserConfig.restore_session` is enabled).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct SessionState {
    /// Last selected database (name or hex identity).
    #[serde(default)]
    pub last_database: Option<String>,
    /// Last selected table name within `last_database`.
    #[serde(default)]
    pub last_table: Option<String>,
    /// Last visible tab index (0..=5).
    #[serde(default)]
    pub last_tab: Option<u8>,
}

impl SessionState {
    /// Read the session file. Missing or invalid → empty session.
    pub fn load() -> Self {
        let Some(path) = session_path() else {
            return Self::default();
        };
        let Ok(content) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        toml::from_str(&content).unwrap_or_default()
    }

    /// Best-effort write to disk. Logs a warning on failure but
    /// never returns an error — losing session state is cosmetic.
    pub fn save(&self) {
        let Some(dir) = config_dir() else { return };
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!("Failed to create config dir {}: {e}", dir.display());
            return;
        }
        let Some(path) = session_path() else { return };
        let serialised = match toml::to_string_pretty(self) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("Failed to serialise session: {e}");
                return;
            }
        };
        if let Err(e) = std::fs::write(&path, serialised) {
            tracing::warn!("Failed to write {}: {e}", path.display());
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_config_parses_full_file() {
        let toml = r#"
            theme = "dracula"
            default_database = "alice-state"
            themes_dir = "/tmp/themes"
            restore_session = false
        "#;
        let cfg: UserConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.theme.as_deref(), Some("dracula"));
        assert_eq!(cfg.default_database.as_deref(), Some("alice-state"));
        assert_eq!(cfg.themes_dir, Some(PathBuf::from("/tmp/themes")));
        assert!(!cfg.restore_session);
    }

    #[test]
    fn user_config_parses_partial_file_with_defaults() {
        let toml = r#"theme = "light""#;
        let cfg: UserConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.theme.as_deref(), Some("light"));
        assert!(cfg.default_database.is_none());
        // restore_session defaults to true via #[serde(default = "default_true")]
        assert!(cfg.restore_session);
    }

    #[test]
    fn user_config_parses_empty_file() {
        let cfg: UserConfig = toml::from_str("").unwrap();
        assert!(cfg.theme.is_none());
        assert!(cfg.default_database.is_none());
        assert!(cfg.restore_session); // default
    }

    #[test]
    fn session_state_round_trip() {
        let s = SessionState {
            last_database: Some("mydb".to_string()),
            last_table: Some("users".to_string()),
            last_tab: Some(2),
        };
        let serialised = toml::to_string_pretty(&s).unwrap();
        let parsed: SessionState = toml::from_str(&serialised).unwrap();
        assert_eq!(parsed.last_database.as_deref(), Some("mydb"));
        assert_eq!(parsed.last_table.as_deref(), Some("users"));
        assert_eq!(parsed.last_tab, Some(2));
    }

    #[test]
    fn session_state_partial_file_fills_gaps_with_none() {
        let toml = r#"last_database = "only-this""#;
        let s: SessionState = toml::from_str(toml).unwrap();
        assert_eq!(s.last_database.as_deref(), Some("only-this"));
        assert!(s.last_table.is_none());
        assert!(s.last_tab.is_none());
    }
}
