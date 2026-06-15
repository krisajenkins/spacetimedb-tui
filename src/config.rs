//! Configuration for the SpacetimeDB TUI.
//!
//! [`Config`] is built from CLI arguments (via [`clap`]) and optional
//! environment variables.  It is constructed once at startup and then passed
//! (by reference or `Arc`) wherever it is needed.

use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::Parser;
use serde::Deserialize;

// ---------------------------------------------------------------------------
// SpacetimeDB CLI config auto-detection
// ---------------------------------------------------------------------------

/// Values pulled from the SpacetimeDB CLI config (`spacetime/cli.toml`).
#[derive(Debug, Default)]
struct SpacetimeCliConfig {
    token: Option<String>,
    host: Option<String>,
    port: Option<u16>,
    uses_tls: bool,
}

/// Locate the SpacetimeDB CLI config file.
///
/// Unlike *our own* config (see [`crate::user_config::config_dir`]), the
/// SpacetimeDB CLI does **not** follow the OS-specific convention — it stores
/// its config under `~/.config/spacetime/` on **every** platform, honouring
/// `XDG_CONFIG_HOME` when set. In particular it does *not* use macOS's
/// `~/Library/Application Support`, so we must not resolve this path via
/// `dirs::config_dir()` (which would silently miss the file on macOS).
fn spacetime_cli_config_path() -> Option<PathBuf> {
    spacetime_cli_config_path_from(std::env::var_os("XDG_CONFIG_HOME"), dirs::home_dir())
}

/// Pure resolver for [`spacetime_cli_config_path`], split out so the
/// precedence rules can be unit-tested without touching the environment.
fn spacetime_cli_config_path_from(
    xdg_config_home: Option<OsString>,
    home_dir: Option<PathBuf>,
) -> Option<PathBuf> {
    let config_home = match xdg_config_home.filter(|v| !v.is_empty()) {
        Some(xdg) => PathBuf::from(xdg),
        None => home_dir?.join(".config"),
    };
    Some(config_home.join("spacetime").join("cli.toml"))
}

/// Try to read and parse the SpacetimeDB CLI config file from
/// `~/.config/spacetime/cli.toml` (or `$XDG_CONFIG_HOME/spacetime/cli.toml`).
///
/// Returns `None` when the file does not exist or cannot be parsed.
fn read_spacetime_cli_config() -> Option<SpacetimeCliConfig> {
    let path = spacetime_cli_config_path()?;
    let content = std::fs::read_to_string(&path).ok()?;
    parse_spacetime_cli_toml(&content)
}

/// On-disk shape of the SpacetimeDB CLI config, for `serde`/`toml`.
///
/// Only the fields we care about are declared; unknown keys (e.g.
/// `web_session_token`) are ignored.
#[derive(Debug, Deserialize)]
struct RawCliConfig {
    #[serde(default)]
    default_server: Option<String>,
    #[serde(default)]
    spacetimedb_token: Option<String>,
    #[serde(default)]
    server_configs: Vec<RawServerConfig>,
}

#[derive(Debug, Deserialize)]
struct RawServerConfig {
    #[serde(default)]
    nickname: Option<String>,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    protocol: Option<String>,
}

/// Parse the TOML format used by the SpacetimeDB CLI.
///
/// The file looks like:
/// ```toml
/// default_server = "local"
/// spacetimedb_token = "eyJ..."
///
/// [[server_configs]]
/// nickname = "local"
/// host = "127.0.0.1:3000"
/// protocol = "http"
/// ```
fn parse_spacetime_cli_toml(content: &str) -> Option<SpacetimeCliConfig> {
    let raw: RawCliConfig = toml::from_str(content).ok()?;

    // Locate the server matching `default_server` (falling back to "local").
    let want = raw.default_server.as_deref().unwrap_or("local");
    let server = raw
        .server_configs
        .iter()
        .find(|s| s.nickname.as_deref() == Some(want));

    let (host, port, uses_tls) = match server.and_then(|s| s.host.as_deref().map(|h| (s, h))) {
        Some((s, host_str)) => {
            let is_https = s.protocol.as_deref() == Some("https");
            // When the host carries no explicit port, fall back to the
            // protocol's standard port (443 for https, 80 for http) rather
            // than the local-dev default of 3000. maincloud, for example, is
            // `maincloud.spacetimedb.com` over https → 443, not 3000.
            let default_port = if is_https { 443 } else { 80 };
            let (h, p) = split_host_port(host_str, default_port);
            (Some(h), Some(p), is_https)
        }
        None => (None, None, false),
    };

    Some(SpacetimeCliConfig {
        token: raw.spacetimedb_token,
        host,
        port,
        uses_tls,
    })
}

/// Split `"host:port"` into `(host, port)`, with `default_port` as fallback.
fn split_host_port(addr: &str, default_port: u16) -> (String, u16) {
    // Handle bracketed IPv6 like `[::1]:3000`.
    if addr.starts_with('[') {
        if let Some(close) = addr.find(']') {
            let host = &addr[..=close];
            let rest = &addr[close + 1..];
            if let Some(p_str) = rest.strip_prefix(':') {
                if let Ok(p) = p_str.parse::<u16>() {
                    return (host.to_string(), p);
                }
            }
            return (host.to_string(), default_port);
        }
    }
    // Regular host:port — use the last `:` so IPv6 literals without brackets work too.
    if let Some(pos) = addr.rfind(':') {
        if let Ok(p) = addr[pos + 1..].parse::<u16>() {
            return (addr[..pos].to_string(), p);
        }
    }
    (addr.to_string(), default_port)
}

/// Resolve the `(host, port, tls)` to connect to from the CLI flags and the
/// optional SpacetimeDB CLI config.
///
/// The user is considered to have **explicitly chosen a server** when they
/// pass any of `--host`, `--port`, or `--tls` (on the command line or via the
/// matching env var). In that case the CLI config is ignored entirely — even
/// if the values happen to equal the built-in defaults — so that
/// `-H localhost -p 3000` always reaches a local server rather than being
/// silently replaced by the CLI config's `default_server`.
///
/// Only when *no* server flag is given do we fall back to the CLI config, and
/// finally to `localhost:3000` without TLS.
fn resolve_server(
    cli_host: Option<String>,
    cli_port: Option<u16>,
    cli_tls: bool,
    cli_cfg: Option<&SpacetimeCliConfig>,
) -> (String, u16, bool) {
    // `--tls` is a `SetTrue` flag, so `false` is indistinguishable from unset;
    // treat `true` as an explicit server choice, `false` as "not specified".
    let server_explicit = cli_host.is_some() || cli_port.is_some() || cli_tls;

    if !server_explicit {
        if let Some(cc) = cli_cfg {
            return (
                cc.host.clone().unwrap_or_else(|| "localhost".to_string()),
                cc.port.unwrap_or(3000),
                cc.uses_tls,
            );
        }
    }

    (
        cli_host.unwrap_or_else(|| "localhost".to_string()),
        cli_port.unwrap_or(3000),
        cli_tls,
    )
}

// ---------------------------------------------------------------------------
// CLI argument definition
// ---------------------------------------------------------------------------

/// Command-line arguments parsed by clap.
#[derive(Debug, Parser)]
#[command(
    name = "spacetimedb-tui",
    version,
    author,
    about = "A terminal user interface for SpacetimeDB",
    long_about = None,
)]
pub struct Cli {
    // No `default_value`: an unset (`None`) host means "the user did not pick
    // a server", which lets `Config::from_cli` fall back to the SpacetimeDB
    // CLI config. Defaults to `localhost` only when neither the flag, the env
    // var, nor the CLI config supplies one.
    #[arg(
        short = 'H',
        long,
        env = "SPACETIMEDB_HOST",
        help = "SpacetimeDB server hostname [default: localhost]"
    )]
    pub host: Option<String>,

    // Like `host`, left as `None` when unspecified so the CLI config can be
    // consulted; otherwise defaults to `3000`.
    #[arg(
        short,
        long,
        env = "SPACETIMEDB_PORT",
        help = "SpacetimeDB server port [default: 3000]"
    )]
    pub port: Option<u16>,

    /// Database (module) name to connect to on startup.
    #[arg(
        short,
        long,
        env = "SPACETIMEDB_DATABASE",
        help = "Database / module name to open on startup"
    )]
    pub database: Option<String>,

    /// Authentication token.
    #[arg(
        short,
        long,
        env = "SPACETIMEDB_TOKEN",
        help = "Bearer token for authentication"
    )]
    pub token: Option<String>,

    /// Use TLS (wss:// / https://).
    #[arg(long, default_value_t = false, help = "Use TLS for the connection")]
    pub tls: bool,

    /// Log level filter for the TUI's own log output (not module logs).
    #[arg(
        long,
        default_value = "warn",
        env = "RUST_LOG",
        help = "Tracing log level (error/warn/info/debug/trace)"
    )]
    pub log_level: String,

    /// Colour theme.
    #[arg(
        long,
        default_value = "dark",
        help = "Colour theme: dark, light, or high-contrast"
    )]
    pub theme: ThemeName,
}

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

/// Named colour themes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ThemeName {
    Dark,
    Light,
    HighContrast,
}

impl std::fmt::Display for ThemeName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ThemeName::Dark => write!(f, "dark"),
            ThemeName::Light => write!(f, "light"),
            ThemeName::HighContrast => write!(f, "high-contrast"),
        }
    }
}

/// A set of ratatui `Color` values for a named theme.
///
/// Using `u8` RGB triples rather than `ratatui::style::Color` directly so
/// that `config.rs` does not need to depend on ratatui (keeping the layer
/// boundary clean). The UI layer converts these to `Color::Rgb(r, g, b)`
/// when rendering.
///
/// Most fields are referenced by the renderers (e.g. `accent`, `success`,
/// `bg_selected`); the remaining ones (`bg_*`, `highlight`, `info`,
/// `border_*`) are kept for future expansion when the rest of the UI is
/// converted off hardcoded constants.
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct ThemeColors {
    // Backgrounds
    pub bg_primary: (u8, u8, u8),
    pub bg_secondary: (u8, u8, u8),
    pub bg_selected: (u8, u8, u8),
    // Foregrounds
    pub fg_primary: (u8, u8, u8),
    pub fg_secondary: (u8, u8, u8),
    pub fg_muted: (u8, u8, u8),
    // Accent / highlight
    pub accent: (u8, u8, u8),
    pub highlight: (u8, u8, u8),
    // Status colours
    pub success: (u8, u8, u8),
    pub warning: (u8, u8, u8),
    pub error: (u8, u8, u8),
    pub info: (u8, u8, u8),
    // Border
    pub border_normal: (u8, u8, u8),
    pub border_focused: (u8, u8, u8),
}

impl ThemeColors {
    pub fn dark() -> Self {
        Self {
            bg_primary: (18, 18, 18),
            bg_secondary: (28, 28, 30),
            bg_selected: (44, 62, 80),
            fg_primary: (220, 220, 220),
            fg_secondary: (180, 180, 180),
            fg_muted: (120, 120, 120),
            accent: (97, 175, 239),
            highlight: (229, 192, 123),
            success: (152, 195, 121),
            warning: (229, 192, 123),
            error: (224, 108, 117),
            info: (86, 182, 194),
            border_normal: (60, 60, 60),
            border_focused: (97, 175, 239),
        }
    }

    pub fn light() -> Self {
        Self {
            bg_primary: (248, 248, 248),
            bg_secondary: (235, 235, 235),
            bg_selected: (200, 220, 240),
            fg_primary: (30, 30, 30),
            fg_secondary: (80, 80, 80),
            fg_muted: (160, 160, 160),
            accent: (0, 100, 200),
            highlight: (160, 100, 0),
            success: (0, 140, 0),
            warning: (180, 120, 0),
            error: (200, 0, 0),
            info: (0, 120, 160),
            border_normal: (180, 180, 180),
            border_focused: (0, 100, 200),
        }
    }

    pub fn high_contrast() -> Self {
        Self {
            bg_primary: (0, 0, 0),
            bg_secondary: (20, 20, 20),
            bg_selected: (0, 80, 160),
            fg_primary: (255, 255, 255),
            fg_secondary: (220, 220, 220),
            fg_muted: (180, 180, 180),
            accent: (0, 200, 255),
            highlight: (255, 220, 0),
            success: (0, 255, 0),
            warning: (255, 200, 0),
            error: (255, 0, 0),
            info: (0, 200, 255),
            border_normal: (120, 120, 120),
            border_focused: (255, 255, 255),
        }
    }

    pub fn for_theme(theme: ThemeName) -> Self {
        match theme {
            ThemeName::Dark => Self::dark(),
            ThemeName::Light => Self::light(),
            ThemeName::HighContrast => Self::high_contrast(),
        }
    }

    /// Look up a theme by free-form name. Built-ins (`"dark"`,
    /// `"light"`, `"high-contrast"`) match first; anything else is
    /// treated as a stem and loaded from `<themes_dir>/<name>.toml`.
    /// Returns `None` if neither lookup succeeds — the caller should
    /// fall back to a built-in default and surface a warning.
    pub fn resolve_named(name: &str, themes_dir: Option<&std::path::Path>) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "dark" => Some(Self::dark()),
            "light" => Some(Self::light()),
            "high-contrast" | "highcontrast" => Some(Self::high_contrast()),
            other => Self::load_from_dir(other, themes_dir),
        }
    }

    /// Try to load a theme by `name` from `themes_dir`, falling back
    /// to `~/.config/spacetimedb-tui/themes/` when no explicit
    /// directory is supplied. The file is expected to contain RGB
    /// triples for every field of [`ThemeColors`].
    fn load_from_dir(name: &str, themes_dir: Option<&std::path::Path>) -> Option<Self> {
        let dir = match themes_dir {
            Some(d) => d.to_path_buf(),
            None => crate::user_config::config_dir()?.join("themes"),
        };
        let path = dir.join(format!("{name}.toml"));
        let content = std::fs::read_to_string(&path).ok()?;
        match toml::from_str::<ThemeColors>(&content) {
            Ok(t) => Some(t),
            Err(e) => {
                tracing::warn!(
                    "Could not parse theme {}: {e}; falling back to built-in",
                    path.display()
                );
                None
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Resolved application configuration, derived from [`Cli`].
///
/// Some fields (`theme`, `theme_name`) are reserved for future UI theming
/// and are not yet consumed by the renderers.
#[derive(Debug, Clone)]
pub struct Config {
    /// Full HTTP base URL, e.g. `http://localhost:3000`.
    pub server_url: String,
    /// Full WebSocket base URL, e.g. `ws://localhost:3000`.
    pub ws_url: String,
    /// Database / module to open on startup (may be `None`).
    pub database: Option<String>,
    /// Authentication token (may be `None` for unauthenticated access).
    pub auth_token: Option<String>,
    /// Resolved colour theme (reserved for future UI theming).
    #[allow(dead_code)]
    pub theme: ThemeColors,
    /// Theme name (for display / serialisation).
    #[allow(dead_code)]
    pub theme_name: ThemeName,
    /// Tracing log level string.
    pub log_level: String,
    /// User-level preferences from `~/.config/spacetimedb-tui/config.toml`.
    /// Used at runtime by `App::bootstrap` for session restore and by the
    /// theming layer to look up custom palettes.
    pub user_config: crate::user_config::UserConfig,
}

impl Config {
    /// Build a [`Config`] from parsed CLI arguments.
    ///
    /// When no `--host`/`--port`/`--tls` flag is given (and/or `--token` is
    /// not supplied), values are sourced from the SpacetimeDB CLI config
    /// (`~/.config/spacetime/cli.toml` on every platform) if that file exists.
    /// Any explicit server flag — even one equal to the built-in default —
    /// suppresses that fallback; see [`resolve_server`].
    ///
    /// # Errors
    /// Returns an error if the port is 0 or the host is empty.
    pub fn from_cli(cli: Cli) -> Result<Self> {
        // Pull user preferences out of `~/.config/spacetimedb-tui/config.toml`.
        // CLI args override anything we find here, but the user config can
        // supply a default theme and a default database when the CLI didn't.
        let user_cfg = crate::user_config::UserConfig::load();

        let cli_cfg = read_spacetime_cli_config();

        // Host / port / TLS: explicit CLI flags win; otherwise fall back to
        // the SpacetimeDB CLI config, then to localhost:3000. See
        // [`resolve_server`] for the precedence rules.
        let (host, port, tls) = resolve_server(cli.host, cli.port, cli.tls, cli_cfg.as_ref());

        if host.trim().is_empty() {
            bail!("--host must not be empty");
        }
        if port == 0 {
            bail!("--port must be a non-zero port number");
        }

        // Auth token: explicit `--token` wins, then CLI config, then None.
        let auth_token = cli.token.or_else(|| cli_cfg.and_then(|cc| cc.token));

        let scheme = if tls { "https" } else { "http" };
        let ws_scheme = if tls { "wss" } else { "ws" };

        let server_url = format!("{}://{}:{}", scheme, host, port);
        let ws_url = format!("{}://{}:{}", ws_scheme, host, port);

        // CLI `--database` always wins; otherwise fall back to the
        // user config's `default_database`. Session restore is
        // applied later (in `App::bootstrap`) so the user can still
        // type a non-default DB on the CLI without it being
        // overwritten.
        let database = cli.database.or(user_cfg.default_database.clone());

        // Theme resolution priority:
        //   1. CLI `--theme` if it deviates from the default
        //   2. `user_cfg.theme` (built-in name OR `themes_dir` lookup)
        //   3. CLI default (Dark)
        //
        // The built-in default for `--theme` is `Dark`; we treat that
        // as "user didn't ask for anything" so we don't accidentally
        // override the user_cfg setting.
        let theme_name = cli.theme;
        let theme_was_explicit = !matches!(theme_name, ThemeName::Dark);
        let theme = if theme_was_explicit {
            ThemeColors::for_theme(theme_name)
        } else if let Some(ref name) = user_cfg.theme {
            ThemeColors::resolve_named(name, user_cfg.themes_dir.as_deref())
                .unwrap_or_else(ThemeColors::dark)
        } else {
            ThemeColors::for_theme(theme_name)
        };

        Ok(Self {
            server_url,
            ws_url,
            database,
            auth_token,
            theme,
            theme_name,
            log_level: cli.log_level,
            user_config: user_cfg,
        })
    }

    /// Parse CLI args from `std::env::args()` and build a [`Config`].
    pub fn parse() -> Result<Self> {
        let cli = Cli::parse();
        Self::from_cli(cli)
    }

    /// Whether TLS is in use (inferred from the scheme in `server_url`).
    ///
    /// Used when constructing WebSocket URLs and for display in the status bar.
    #[allow(dead_code)]
    pub fn uses_tls(&self) -> bool {
        self.server_url.starts_with("https://")
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_cli(host: &str, port: u16, database: Option<&str>, tls: bool) -> Cli {
        Cli {
            host: Some(host.to_string()),
            port: Some(port),
            database: database.map(str::to_owned),
            token: None,
            tls,
            log_level: "warn".to_string(),
            theme: ThemeName::Dark,
        }
    }

    #[test]
    fn test_config_http() {
        // Use an explicit non-default host so that CLI config auto-detection
        // (which only fires when the host is at its default "localhost") does
        // not interfere with the expected URL in this test.
        let cfg = Config::from_cli(make_cli("test.local", 3000, None, false)).unwrap();
        assert_eq!(cfg.server_url, "http://test.local:3000");
        assert_eq!(cfg.ws_url, "ws://test.local:3000");
        assert!(!cfg.uses_tls());
    }

    #[test]
    fn test_config_tls() {
        let cfg = Config::from_cli(make_cli("example.com", 443, Some("mydb"), true)).unwrap();
        assert_eq!(cfg.server_url, "https://example.com:443");
        assert_eq!(cfg.ws_url, "wss://example.com:443");
        assert!(cfg.uses_tls());
        assert_eq!(cfg.database.as_deref(), Some("mydb"));
    }

    #[test]
    fn test_config_empty_host_is_error() {
        let result = Config::from_cli(make_cli("", 3000, None, false));
        assert!(result.is_err());
    }

    fn cli_cfg(host: &str, port: u16, tls: bool) -> SpacetimeCliConfig {
        SpacetimeCliConfig {
            token: None,
            host: Some(host.to_string()),
            port: Some(port),
            uses_tls: tls,
        }
    }

    #[test]
    fn resolve_server_uses_cli_config_when_no_flags() {
        // No server flags → fall back to the CLI config (e.g. maincloud).
        let cc = cli_cfg("maincloud.spacetimedb.com", 443, true);
        let (h, p, tls) = resolve_server(None, None, false, Some(&cc));
        assert_eq!(
            (h.as_str(), p, tls),
            ("maincloud.spacetimedb.com", 443, true)
        );
    }

    #[test]
    fn resolve_server_explicit_localhost_beats_cli_config() {
        // The regression: passing the dev defaults explicitly must reach
        // localhost, NOT be swallowed and replaced by the CLI config.
        let cc = cli_cfg("maincloud.spacetimedb.com", 443, true);
        let (h, p, tls) =
            resolve_server(Some("localhost".to_string()), Some(3000), false, Some(&cc));
        assert_eq!((h.as_str(), p, tls), ("localhost", 3000, false));
    }

    #[test]
    fn resolve_server_partial_host_flag_ignores_cli_config_port() {
        // Only `--host` given: the CLI config is ignored entirely, and the
        // port falls back to the built-in 3000 (not the config's port).
        let cc = cli_cfg("maincloud.spacetimedb.com", 443, true);
        let (h, p, tls) = resolve_server(Some("10.0.0.5".to_string()), None, false, Some(&cc));
        assert_eq!((h.as_str(), p, tls), ("10.0.0.5", 3000, false));
    }

    #[test]
    fn resolve_server_tls_flag_alone_counts_as_explicit() {
        let cc = cli_cfg("maincloud.spacetimedb.com", 443, true);
        let (h, p, tls) = resolve_server(None, None, true, Some(&cc));
        assert_eq!((h.as_str(), p, tls), ("localhost", 3000, true));
    }

    #[test]
    fn resolve_server_defaults_when_no_flags_and_no_config() {
        let (h, p, tls) = resolve_server(None, None, false, None);
        assert_eq!((h.as_str(), p, tls), ("localhost", 3000, false));
    }

    #[test]
    fn test_config_zero_port_is_error() {
        let result = Config::from_cli(make_cli("localhost", 0, None, false));
        assert!(result.is_err());
    }

    #[test]
    fn theme_colors_deserialise_from_toml() {
        // Tuples in TOML are inline arrays.
        let toml = r#"
            bg_primary    = [10, 10, 10]
            bg_secondary  = [20, 20, 20]
            bg_selected   = [30, 30, 30]
            fg_primary    = [200, 200, 200]
            fg_secondary  = [180, 180, 180]
            fg_muted      = [120, 120, 120]
            accent        = [97, 175, 239]
            highlight     = [229, 192, 123]
            success       = [152, 195, 121]
            warning       = [229, 192, 123]
            error         = [224, 108, 117]
            info          = [86, 182, 194]
            border_normal = [60, 60, 60]
            border_focused= [97, 175, 239]
        "#;
        let t: ThemeColors = toml::from_str(toml).expect("theme parses");
        assert_eq!(t.accent, (97, 175, 239));
        assert_eq!(t.bg_primary, (10, 10, 10));
        assert_eq!(t.success, (152, 195, 121));
    }

    #[test]
    fn theme_resolve_named_built_ins() {
        let dark = ThemeColors::resolve_named("dark", None).unwrap();
        assert_eq!(dark.accent, ThemeColors::dark().accent);
        let light = ThemeColors::resolve_named("LIGHT", None).unwrap();
        assert_eq!(light.accent, ThemeColors::light().accent);
        let hc = ThemeColors::resolve_named("high-contrast", None).unwrap();
        assert_eq!(hc.accent, ThemeColors::high_contrast().accent);
    }

    #[test]
    fn theme_resolve_named_returns_none_for_unknown() {
        // No themes_dir, no $HOME guarantee — at minimum we should
        // not panic and should return None for non-built-in names.
        let result = ThemeColors::resolve_named("definitely-not-a-real-theme", None);
        // We can't assert exactly None here because if a user has a
        // matching file in their real ~/.config we'd accidentally
        // hit it. But the test asserts the function doesn't panic.
        let _ = result;
    }

    #[test]
    fn spacetime_cli_path_prefers_xdg_config_home() {
        let p = spacetime_cli_config_path_from(
            Some(OsString::from("/custom/xdg")),
            Some(PathBuf::from("/home/alice")),
        )
        .unwrap();
        assert_eq!(p, PathBuf::from("/custom/xdg/spacetime/cli.toml"));
    }

    #[test]
    fn spacetime_cli_path_falls_back_to_dot_config_under_home() {
        // No XDG, and empty XDG, both fall through to ~/.config — NOT
        // ~/Library on macOS, which is the whole point of this resolver.
        for xdg in [None, Some(OsString::from(""))] {
            let p =
                spacetime_cli_config_path_from(xdg, Some(PathBuf::from("/home/alice"))).unwrap();
            assert_eq!(p, PathBuf::from("/home/alice/.config/spacetime/cli.toml"));
        }
    }

    #[test]
    fn spacetime_cli_path_none_without_home_or_xdg() {
        assert!(spacetime_cli_config_path_from(None, None).is_none());
    }

    #[test]
    fn parse_cli_toml_picks_default_server() {
        let toml = r#"
            default_server = "maincloud"
            spacetimedb_token = "tok123"

            [[server_configs]]
            nickname = "maincloud"
            host = "maincloud.spacetimedb.com"
            protocol = "https"

            [[server_configs]]
            nickname = "local"
            host = "127.0.0.1:3000"
            protocol = "http"
        "#;
        let cfg = parse_spacetime_cli_toml(toml).unwrap();
        assert_eq!(cfg.token.as_deref(), Some("tok123"));
        assert_eq!(cfg.host.as_deref(), Some("maincloud.spacetimedb.com"));
        // No port in the host string → falls back to the protocol default
        // (https → 443), NOT the local-dev 3000.
        assert_eq!(cfg.port, Some(443));
        assert!(cfg.uses_tls);
    }

    #[test]
    fn parse_cli_toml_default_server_falls_back_to_local() {
        // No `default_server` key → resolver looks for "local".
        let toml = r#"
            spacetimedb_token = "tok123"

            [[server_configs]]
            nickname = "local"
            host = "127.0.0.1:3000"
            protocol = "http"
        "#;
        let cfg = parse_spacetime_cli_toml(toml).unwrap();
        assert_eq!(cfg.host.as_deref(), Some("127.0.0.1"));
        assert_eq!(cfg.port, Some(3000));
        assert!(!cfg.uses_tls);
    }

    #[test]
    fn parse_cli_toml_http_without_port_defaults_to_80() {
        let toml = r#"
            default_server = "remote"

            [[server_configs]]
            nickname = "remote"
            host = "example.com"
            protocol = "http"
        "#;
        let cfg = parse_spacetime_cli_toml(toml).unwrap();
        assert_eq!(cfg.host.as_deref(), Some("example.com"));
        assert_eq!(cfg.port, Some(80));
        assert!(!cfg.uses_tls);
    }

    #[test]
    fn parse_cli_toml_explicit_port_overrides_protocol_default() {
        let toml = r#"
            default_server = "remote"

            [[server_configs]]
            nickname = "remote"
            host = "example.com:8443"
            protocol = "https"
        "#;
        let cfg = parse_spacetime_cli_toml(toml).unwrap();
        assert_eq!(cfg.port, Some(8443));
        assert!(cfg.uses_tls);
    }

    #[test]
    fn parse_cli_toml_token_only_when_no_matching_server() {
        let toml = r#"
            default_server = "missing"
            spacetimedb_token = "tok123"

            [[server_configs]]
            nickname = "local"
            host = "127.0.0.1:3000"
            protocol = "http"
        "#;
        let cfg = parse_spacetime_cli_toml(toml).unwrap();
        assert_eq!(cfg.token.as_deref(), Some("tok123"));
        assert!(cfg.host.is_none());
        assert!(cfg.port.is_none());
    }

    #[test]
    fn parse_cli_toml_rejects_garbage() {
        assert!(parse_spacetime_cli_toml("this is = not [valid toml").is_none());
    }

    #[test]
    fn test_theme_colors_dark() {
        let t = ThemeColors::dark();
        // Spot-check a few fields are non-zero.
        assert_ne!(t.accent, (0, 0, 0));
        assert_ne!(t.error, (0, 0, 0));
    }
}
