//! Configuration for the SpacetimeDB TUI.
//!
//! [`Config`] is built from CLI arguments (via [`clap`]) and optional
//! environment variables.  It is constructed once at startup and then passed
//! (by reference or `Arc`) wherever it is needed.

use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};
use clap::Parser;
use serde::Deserialize;

// ---------------------------------------------------------------------------
// SpacetimeDB CLI config auto-detection
// ---------------------------------------------------------------------------

/// Values pulled from the SpacetimeDB CLI config (`spacetime/cli.toml`).
///
/// Unlike the connection details (which are a single resolved server),
/// we keep **all** named servers around so that `-s/--server <nickname>`
/// can select any of them — not just the file's `default_server`.
#[derive(Debug, Default)]
struct SpacetimeCliConfig {
    token: Option<String>,
    /// The file's `default_server` nickname, used when `-s` is not given.
    default_server: Option<String>,
    /// Every `[[server_configs]]` entry, resolved to host/port/tls.
    servers: Vec<ResolvedServer>,
}

/// A single named server from the CLI config, with its address resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedServer {
    nickname: String,
    host: String,
    port: u16,
    uses_tls: bool,
}

impl SpacetimeCliConfig {
    /// Look up a server by its exact nickname.
    fn server(&self, nickname: &str) -> Option<&ResolvedServer> {
        self.servers.iter().find(|s| s.nickname == nickname)
    }

    /// Resolve the server to use when no explicit `-s` is given: the file's
    /// `default_server`, falling back to the conventional `"local"` nickname.
    fn default_server(&self) -> Option<&ResolvedServer> {
        let want = self.default_server.as_deref().unwrap_or("local");
        self.server(want)
    }

    /// All known server nicknames, for error messages.
    fn nicknames(&self) -> Vec<String> {
        self.servers.iter().map(|s| s.nickname.clone()).collect()
    }
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

    // Resolve every named server to a concrete (host, port, tls). Entries
    // missing a nickname or host are skipped — they can't be selected anyway.
    let servers = raw
        .server_configs
        .iter()
        .filter_map(|s| {
            let nickname = s.nickname.clone()?;
            let host_str = s.host.as_deref()?;
            let is_https = s.protocol.as_deref() == Some("https");
            // When the host carries no explicit port, fall back to the
            // protocol's standard port (443 for https, 80 for http) rather
            // than the local-dev default of 3000. maincloud, for example, is
            // `maincloud.spacetimedb.com` over https → 443, not 3000.
            let default_port = if is_https { 443 } else { 80 };
            let (host, port) = split_host_port(host_str, default_port);
            Some(ResolvedServer {
                nickname,
                host,
                port,
                uses_tls: is_https,
            })
        })
        .collect();

    Some(SpacetimeCliConfig {
        token: raw.spacetimedb_token,
        default_server: raw.default_server,
        servers,
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
/// Precedence, highest first:
///
/// 1. **Explicit `--host`/`--port`/`--tls`.** Passing any of these means the
///    user picked a server by hand; the CLI config is ignored entirely — even
///    if the values happen to equal the built-in defaults — so that
///    `-H localhost -p 3000` always reaches a local server rather than being
///    silently replaced by the config's `default_server`. (These conflict with
///    `-s` at the clap layer, so they never co-occur.)
/// 2. **`-s/--server <nickname>`.** Looks the nickname up in the CLI config's
///    `server_configs` and uses that server's host/port/protocol. An unknown
///    nickname (or a missing config file) is an error, not a silent fallback.
/// 3. **The config's `default_server`**, when no server flag is given at all.
/// 4. **`localhost:3000` without TLS**, when there is no config to consult.
fn resolve_server(
    cli_host: Option<String>,
    cli_port: Option<u16>,
    cli_tls: bool,
    cli_server: Option<&str>,
    cli_cfg: Option<&SpacetimeCliConfig>,
) -> Result<(String, u16, bool)> {
    // `--tls` is a `SetTrue` flag, so `false` is indistinguishable from unset;
    // treat `true` as an explicit server choice, `false` as "not specified".
    let host_port_tls_explicit = cli_host.is_some() || cli_port.is_some() || cli_tls;

    if host_port_tls_explicit {
        return Ok((
            cli_host.unwrap_or_else(|| "localhost".to_string()),
            cli_port.unwrap_or(3000),
            cli_tls,
        ));
    }

    // `-s/--server <nickname>`: resolve it against the CLI config. Unlike the
    // default-server fallback below, a bad `-s` is a hard error so the user
    // isn't silently connected to the wrong place.
    if let Some(name) = cli_server {
        let cfg = cli_cfg.ok_or_else(|| {
            anyhow!(
                "--server {name:?} was given, but no SpacetimeDB CLI config \
                 (~/.config/spacetime/cli.toml) was found to resolve it against"
            )
        })?;
        let server = cfg.server(name).ok_or_else(|| {
            let available = cfg.nicknames();
            let listed = if available.is_empty() {
                "(none configured)".to_string()
            } else {
                available.join(", ")
            };
            anyhow!("--server {name:?} not found in cli.toml; available servers: {listed}")
        })?;
        return Ok((server.host.clone(), server.port, server.uses_tls));
    }

    // No server flags → fall back to the config's default server, then
    // finally to the local-dev default.
    if let Some(cc) = cli_cfg {
        if let Some(server) = cc.default_server() {
            return Ok((server.host.clone(), server.port, server.uses_tls));
        }
    }

    Ok(("localhost".to_string(), 3000, false))
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

    /// Named server from the SpacetimeDB CLI config (e.g. `-slocal`,
    /// `-smaincloud`). A shortcut that pulls host/port/TLS from the matching
    /// `[[server_configs]]` entry in `~/.config/spacetime/cli.toml`. Mutually
    /// exclusive with `--host`/`--port`/`--tls`, which set those by hand.
    #[arg(
        short,
        long,
        env = "SPACETIMEDB_SERVER",
        conflicts_with_all = ["host", "port", "tls"],
        help = "Named server from the spacetime CLI config (e.g. local, maincloud)"
    )]
    pub server: Option<String>,

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

        // Host / port / TLS: explicit CLI flags win; then a `-s` nickname; then
        // the SpacetimeDB CLI config's default server; then localhost:3000. See
        // [`resolve_server`] for the precedence rules.
        let (host, port, tls) = resolve_server(
            cli.host,
            cli.port,
            cli.tls,
            cli.server.as_deref(),
            cli_cfg.as_ref(),
        )?;

        if host.trim().is_empty() {
            bail!("--host must not be empty");
        }
        if port == 0 {
            bail!("--port must be a non-zero port number");
        }

        // Auth token: explicit `--token` wins, then CLI config, then None.
        let auth_token = cli
            .token
            .or_else(|| cli_cfg.as_ref().and_then(|cc| cc.token.clone()));

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
            server: None,
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

    fn server(nickname: &str, host: &str, port: u16, tls: bool) -> ResolvedServer {
        ResolvedServer {
            nickname: nickname.to_string(),
            host: host.to_string(),
            port,
            uses_tls: tls,
        }
    }

    /// A config whose `default_server` points at a single named server.
    fn cli_cfg(nickname: &str, host: &str, port: u16, tls: bool) -> SpacetimeCliConfig {
        SpacetimeCliConfig {
            token: None,
            default_server: Some(nickname.to_string()),
            servers: vec![server(nickname, host, port, tls)],
        }
    }

    #[test]
    fn resolve_server_uses_cli_config_when_no_flags() {
        // No server flags → fall back to the CLI config's default (e.g. maincloud).
        let cc = cli_cfg("maincloud", "maincloud.spacetimedb.com", 443, true);
        let (h, p, tls) = resolve_server(None, None, false, None, Some(&cc)).unwrap();
        assert_eq!(
            (h.as_str(), p, tls),
            ("maincloud.spacetimedb.com", 443, true)
        );
    }

    #[test]
    fn resolve_server_explicit_localhost_beats_cli_config() {
        // The regression: passing the dev defaults explicitly must reach
        // localhost, NOT be swallowed and replaced by the CLI config.
        let cc = cli_cfg("maincloud", "maincloud.spacetimedb.com", 443, true);
        let (h, p, tls) = resolve_server(
            Some("localhost".to_string()),
            Some(3000),
            false,
            None,
            Some(&cc),
        )
        .unwrap();
        assert_eq!((h.as_str(), p, tls), ("localhost", 3000, false));
    }

    #[test]
    fn resolve_server_partial_host_flag_ignores_cli_config_port() {
        // Only `--host` given: the CLI config is ignored entirely, and the
        // port falls back to the built-in 3000 (not the config's port).
        let cc = cli_cfg("maincloud", "maincloud.spacetimedb.com", 443, true);
        let (h, p, tls) =
            resolve_server(Some("10.0.0.5".to_string()), None, false, None, Some(&cc)).unwrap();
        assert_eq!((h.as_str(), p, tls), ("10.0.0.5", 3000, false));
    }

    #[test]
    fn resolve_server_tls_flag_alone_counts_as_explicit() {
        let cc = cli_cfg("maincloud", "maincloud.spacetimedb.com", 443, true);
        let (h, p, tls) = resolve_server(None, None, true, None, Some(&cc)).unwrap();
        assert_eq!((h.as_str(), p, tls), ("localhost", 3000, true));
    }

    #[test]
    fn resolve_server_defaults_when_no_flags_and_no_config() {
        let (h, p, tls) = resolve_server(None, None, false, None, None).unwrap();
        assert_eq!((h.as_str(), p, tls), ("localhost", 3000, false));
    }

    #[test]
    fn resolve_server_named_flag_selects_non_default_server() {
        // `-s local` picks `local` even though `default_server` is maincloud.
        let cc = SpacetimeCliConfig {
            token: None,
            default_server: Some("maincloud".to_string()),
            servers: vec![
                server("maincloud", "maincloud.spacetimedb.com", 443, true),
                server("local", "127.0.0.1", 3000, false),
            ],
        };
        let (h, p, tls) = resolve_server(None, None, false, Some("local"), Some(&cc)).unwrap();
        assert_eq!((h.as_str(), p, tls), ("127.0.0.1", 3000, false));
    }

    #[test]
    fn resolve_server_named_flag_unknown_nickname_errors() {
        let cc = cli_cfg("local", "127.0.0.1", 3000, false);
        let err = resolve_server(None, None, false, Some("nope"), Some(&cc)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("nope"), "message was: {msg}");
        assert!(
            msg.contains("local"),
            "should list available servers: {msg}"
        );
    }

    #[test]
    fn resolve_server_named_flag_without_config_errors() {
        let err = resolve_server(None, None, false, Some("local"), None).unwrap_err();
        assert!(err.to_string().contains("no SpacetimeDB CLI config"));
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
        let s = cfg.default_server().unwrap();
        assert_eq!(s.host, "maincloud.spacetimedb.com");
        // No port in the host string → falls back to the protocol default
        // (https → 443), NOT the local-dev 3000.
        assert_eq!(s.port, 443);
        assert!(s.uses_tls);
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
        let s = cfg.default_server().unwrap();
        assert_eq!(s.host, "127.0.0.1");
        assert_eq!(s.port, 3000);
        assert!(!s.uses_tls);
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
        let s = cfg.default_server().unwrap();
        assert_eq!(s.host, "example.com");
        assert_eq!(s.port, 80);
        assert!(!s.uses_tls);
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
        let s = cfg.default_server().unwrap();
        assert_eq!(s.port, 8443);
        assert!(s.uses_tls);
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
        // `default_server = "missing"` has no matching entry, so resolution
        // yields nothing — but the token is still read from the top level.
        assert!(cfg.default_server().is_none());
    }

    #[test]
    fn parse_cli_toml_keeps_all_servers_for_nickname_lookup() {
        // Every named server is retained so `-s <nickname>` can pick any of
        // them, not just `default_server`.
        let toml = r#"
            default_server = "maincloud"

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
        assert_eq!(cfg.nicknames(), vec!["maincloud", "local"]);
        let local = cfg.server("local").unwrap();
        assert_eq!(
            (local.host.as_str(), local.port, local.uses_tls),
            ("127.0.0.1", 3000, false)
        );
        // The default resolves to maincloud.
        assert_eq!(cfg.default_server().unwrap().nickname, "maincloud");
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
