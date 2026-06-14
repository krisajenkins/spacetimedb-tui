# 🛸 spacetimedb-tui

> **A blazing-fast, keyboard-driven terminal UI for managing, querying, editing, and monitoring SpacetimeDB 2.0 — right from your shell.**

Browse databases, run SQL, stream live transactions, edit rows in a spreadsheet, call reducers, and manage aliases — all with Vim-style key bindings and a command palette.

[![CI](https://github.com/RazieLDG/spacetimedb-tui/actions/workflows/ci.yml/badge.svg)](https://github.com/RazieLDG/spacetimedb-tui/actions/workflows/ci.yml)
[![Release](https://github.com/RazieLDG/spacetimedb-tui/actions/workflows/release.yml/badge.svg)](https://github.com/RazieLDG/spacetimedb-tui/actions/workflows/release.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](./LICENSE)
[![SpacetimeDB](https://img.shields.io/badge/SpacetimeDB-2.0-blueviolet)](https://spacetimedb.com)
[![Rust](https://img.shields.io/badge/Rust-1.78%2B-orange)](https://www.rust-lang.org)
[![Tests](https://img.shields.io/badge/tests-127%20passing-brightgreen)](#)

---

## 📸 Screenshots

| Database Browser | SQL Console |
|:---:|:---:|
| ![Database Browser](images/stdb1.jpg) | ![Table Viewer](images/stdb2.jpg) |

| Metrics Viewer | Module Inspector |
|:---:|:---:|
| ![SQL Console](images/stdb3.jpg) | ![Module Inspector](images/stdb4.jpg) |

---

## ✨ Features

### Read

| Feature | Description |
|---|---|
| 🗄️ **Database Browser** | Tree-style sidebar with databases and their tables. Step-up navigation (`Esc`/`h`), search-as-you-type (`/`), and automatic schema fallback states (`loading…` / `schema unavailable — press r to retry`). |
| 📊 **Tables Tab** | Browse any table's rows in a scrollable grid with a real two-dimensional cell cursor. Horizontal scroll, sort (off → asc → desc), and client-side search highlighting (`Ctrl+F`, `n`/`N`). |
| 🖥️ **SQL Console** | Syntax-highlighted input with **autocomplete** (`Tab`) for keywords, table names, and column names. Query history (`↑`/`↓`), `Ctrl+L/K/U/W` line editing, and a scrollable results grid. |
| 📜 **Log Viewer** | Tail structured logs with level filtering (`f` cycles `Trace` → `Panic`), pause/resume (`Space`), and a live `visible / total` counter. Tolerates both RFC 3339 and u64-microsecond timestamps. |
| 📈 **Metrics Dashboard** | Per-tick **delta sparklines** (not cumulative ramps) for reducer calls and energy use, plus stat cards for clients / tables / reducer count / memory. Auto-refresh every 10s while visible. |
| 🔬 **Module Inspector** | Browse reducers with full parameter signatures, user tables, system tables, and columns. |
| ⚡ **Live Tab** | Real-time transaction feed driven by the WebSocket subscription, split 2/3 : 1/3 with a connected-client list polled from `st_client` every 10s. Status bar shows `● LIVE` / `◌ reconnect in Ns`. |

### Write

| Feature | Description |
|---|---|
| 📞 **Reducer Calls** | `Enter` on a reducer in the Module tab opens a param-typed form. Values are type-coerced to JSON (numerics, bools, strings, raw JSON arrays/objects) and sent via `POST /v1/database/<db>/call/<reducer>`. |
| ➕ **Row Insert** | `i` on the Tables tab opens a column-typed form; submit issues `INSERT INTO <table> (…) VALUES (…)`. |
| ✏️ **Row Update (form)** | `Shift+U` pre-fills every column of the selected row into an edit form with the PK marked read-only. Submit builds a correct `UPDATE … SET col=val WHERE pk=original_pk`, even if the PK is an `Identity` / `ConnectionId` / `U256`. |
| 🗑️ **Row Delete** | `d` opens a y/n confirm showing the exact `DELETE FROM … WHERE pk = …` statement that will run. |
| 📝 **Spreadsheet Edit Mode** | `Ctrl+E` enters a cell-by-cell editor on the Tables tab. Move with `h`/`j`/`k`/`l`, `Enter` to open an inline input, `Enter` to stage, `s` to flush all pending edits as batched `UPDATE`s, `u` to revert, `Esc` to exit (with a discard prompt if pending > 0). |
| 💾 **Clipboard + Export** | `y` copies the cell, `Y` copies the row as TSV (via OSC 52, no external dep). `e` exports to CSV, `E` exports to JSON under `./exports/`. |

### Admin

| Feature | Description |
|---|---|
| 🆕 **Add Alias** | `a` on a database opens a form to attach a new human-readable name via `POST /v1/database/<db>/names`. |
| ❌ **Delete Database** | `Shift+D` on a database opens a **typed-confirm** form (the user has to type the database name verbatim) before firing `DELETE /v1/database/<db>`. |
| 🧹 **Truncate Table** | `Shift+D` on a table opens the same typed-confirm pattern and issues `DELETE FROM <table>`. |

### UX

| Feature | Description |
|---|---|
| 🎨 **Theming** | Built-in `dark`, `light`, `high-contrast` themes plus user-defined palettes in `~/.config/spacetimedb-tui/themes/<name>.toml`. Select via `--theme` or the user config. |
| 💾 **Session Restore** | On quit, the last selected database, table, and tab are written to `~/.config/spacetimedb-tui/session.toml` and reloaded on next launch. Disable with `restore_session = false`. |
| 🎛️ **Command Palette** | `Ctrl+P` opens a fuzzy-search overlay over every registered command (tab jumps, refresh, reconnect, export, copy, quit, …). |
| 🧭 **Sidebar Search** | `/` filters the database / table list as you type. |
| 🔁 **Auto-Reconnect** | WebSocket drops trigger exponential backoff reconnects with live countdown. HTTP 4xx/5xx are classified as **permanent** and abort the retry loop so the logs don't flood. |
| ⌨️ **Keyboard-First UX** | Every action is reachable without a mouse. Vim-style navigation, modal forms, help overlay with section-grouped bindings. |
| 🔐 **Auto Auth** | Reads credentials from `~/.config/spacetime/cli.toml` — zero config in the common case. |

---

## 🚀 Installation

### Prerequisites

- A running **SpacetimeDB 2.0** instance (local or remote)
- **SpacetimeDB CLI** configured (`spacetime login` or local server)
- For source builds only: **Rust 1.78+** via [rustup](https://rustup.rs)

### Option 1 — One-line installer (recommended)

The repo ships ready-to-pipe installer scripts that detect your OS/architecture, download the matching archive from the latest GitHub release, and drop the binary onto a directory on your `PATH`.

```bash
# Linux / macOS — installs to ~/.local/bin by default
curl -fsSL https://raw.githubusercontent.com/RazieLDG/spacetimedb-tui/main/scripts/install.sh | bash

# Pin a version or override the install dir
curl -fsSL https://raw.githubusercontent.com/RazieLDG/spacetimedb-tui/main/scripts/install.sh \
    | bash -s -- --version v0.1.0 --dir /usr/local/bin
```

```powershell
# Windows (PowerShell 5.1+ / 7+)
irm https://raw.githubusercontent.com/RazieLDG/spacetimedb-tui/main/scripts/install.ps1 | iex

# Pin a version and auto-append the install dir to user PATH
iex "& { $(irm https://raw.githubusercontent.com/RazieLDG/spacetimedb-tui/main/scripts/install.ps1) } -Version v0.1.0 -AddToPath"
```

Both scripts are idempotent — re-running upgrades the binary in place.

### Option 2 — Pre-built binaries (manual)

Every tagged release ships pre-built archives for the three tier-1 desktop platforms. Grab the right archive for your machine from the [latest release](https://github.com/RazieLDG/spacetimedb-tui/releases/latest):

| Platform | Archive |
|---|---|
| Linux x86_64 (glibc) | `spacetimedb-tui-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz` |
| macOS Intel | `spacetimedb-tui-vX.Y.Z-x86_64-apple-darwin.tar.gz` |
| macOS Apple Silicon | `spacetimedb-tui-vX.Y.Z-aarch64-apple-darwin.tar.gz` |
| Windows x86_64 | `spacetimedb-tui-vX.Y.Z-x86_64-pc-windows-msvc.zip` |

Unpack and drop `spacetimedb-tui` (or `spacetimedb-tui.exe` on Windows) anywhere on your `PATH`:

```bash
# Linux / macOS
tar xzf spacetimedb-tui-v0.1.0-x86_64-unknown-linux-gnu.tar.gz
sudo mv spacetimedb-tui-v0.1.0-x86_64-unknown-linux-gnu/spacetimedb-tui /usr/local/bin/
```

```powershell
# Windows (PowerShell)
Expand-Archive spacetimedb-tui-v0.1.0-x86_64-pc-windows-msvc.zip
Move-Item .\spacetimedb-tui-v0.1.0-x86_64-pc-windows-msvc\spacetimedb-tui.exe `
          "$env:USERPROFILE\bin\spacetimedb-tui.exe"
```

### Option 3 — Build from source

```bash
# 1. Clone the repository
git clone https://github.com/RazieLDG/spacetimedb-tui.git
cd spacetimedb-tui

# 2. Build an optimised release binary
cargo build --release

# 3. (Optional) copy to a directory on your PATH
cp target/release/spacetimedb-tui ~/.local/bin/
```

---

## 🖥️ Usage

### Quick Start

`spacetimedb-tui` automatically reads your SpacetimeDB CLI config from `~/.config/spacetime/cli.toml`, so in most cases you can just run:

```bash
# Connect using your existing SpacetimeDB CLI credentials
spacetimedb-tui

# Specify a database to open on startup
spacetimedb-tui --database my_game_db

# Connect to a specific host
spacetimedb-tui --host db.example.com --port 3000

# Provide a token explicitly
spacetimedb-tui --host localhost --port 3000 --token $STDB_TOKEN

# Pick a theme
spacetimedb-tui --theme high-contrast
```

### CLI Reference

| Flag | Short | Default | Description |
|---|---|---|---|
| `--host <HOST>` | `-H` | *from cli.toml* | SpacetimeDB server hostname or IP |
| `--port <PORT>` | `-p` | `3000` | SpacetimeDB server port |
| `--database <DB>` | `-d` | *(none)* | Database to select on startup |
| `--token <TOKEN>` | `-t` | *from cli.toml* | SpacetimeDB identity/auth token |
| `--theme <NAME>` |  | `dark` | `dark`, `light`, or `high-contrast` |
| `--tls` |  | `false` | Use `https` / `wss` instead of `http` / `ws` |
| `--log-level <LVL>` |  | `warn` | `error`, `warn`, `info`, `debug`, `trace` |
| `--version` | `-V` | | Print version and exit |
| `--help` | `-h` | | Print help and exit |

### Auto-Configuration

The TUI reads credentials from SpacetimeDB's CLI config file:

```
~/.config/spacetime/cli.toml
```

This file is created when you run `spacetime login` or `spacetime start`. It contains:
- Your **auth token** (JWT with embedded identity)
- Your **default server** hostname and port

If this file exists, you don't need to pass `--host`, `--port`, or `--token` manually.

### User Config

Persistent preferences live at the platform-specific config directory:

| Platform | Path |
|---|---|
| **Linux** | `~/.config/spacetimedb-tui/config.toml` |
| **macOS** | `~/Library/Application Support/spacetimedb-tui/config.toml` |
| **Windows** | `%APPDATA%\spacetimedb-tui\config.toml` |

The SpacetimeDB CLI config is **not** looked up in this root. The `spacetime` CLI stores its own config under `~/.config/spacetime/cli.toml` on _every_ platform (honouring `XDG_CONFIG_HOME`; it does not use `~/Library` on macOS), so that is where the TUI reads credentials created by `spacetime login` from — see [Auto-Configuration](#auto-configuration) above.

Example contents:

```toml
# Default theme — built-in name (dark / light / high-contrast)
# or the stem of a file under themes_dir/.
theme = "dark"

# Open this database automatically when no --database flag is passed.
default_database = "my_game_db"

# Where to look for user-defined theme files.
# Defaults to ~/.config/spacetimedb-tui/themes/
# themes_dir = "/path/to/themes"

# Restore last selected db / table / tab on next launch.
restore_session = true
```

A user theme file is a flat table of RGB triples, placed under `<config_dir>/themes/<name>.toml`:

```toml
# e.g. ~/.config/spacetimedb-tui/themes/dracula.toml (Linux)
bg_primary    = [40, 42, 54]
bg_secondary  = [68, 71, 90]
bg_selected   = [68, 71, 90]
fg_primary    = [248, 248, 242]
fg_secondary  = [189, 147, 249]
fg_muted      = [98, 114, 164]
accent        = [189, 147, 249]
highlight     = [241, 250, 140]
success       = [80, 250, 123]
warning       = [255, 184, 108]
error         = [255, 85, 85]
info          = [139, 233, 253]
border_normal = [68, 71, 90]
border_focused = [189, 147, 249]
```

---

## ⌨️ Key Bindings

Press `?` at any time to open a scrollable help overlay listing every binding. Use `j`/`k` or `g`/`G` to navigate, `Esc`/`q` to close.

### Global

| Key | Action |
|---|---|
| `1`–`6` | Jump to tab (Tables / SQL / Logs / Metrics / Module / Live) |
| `Tab` / `Shift+Tab` | Cycle tabs |
| `q` / `Ctrl+C` | Quit |
| `?` | Toggle help overlay |
| `Ctrl+P` | Command palette (fuzzy search) |
| `Ctrl+R` | Force WebSocket reconnect |
| `r` | Refresh current view |
| `:` | Jump into the SQL console input |

### Navigation

| Key | Action |
|---|---|
| `j` / `↓` | Move down |
| `k` / `↑` | Move up |
| `h` / `←` | Sidebar: step up (Tables → Databases) / focus sidebar from main |
| `l` / `→` | Focus main pane (or move cell cursor right in Tables / SQL) |
| `g` / `Home` | First item |
| `G` / `End` | Last item |
| `Enter` | Select / open / confirm |
| `Esc` / `Backspace` | Step back up the sidebar tree |
| `/` | Sidebar search |

### Data Grid (Tables / SQL Results)

| Key | Action |
|---|---|
| `h` / `j` / `k` / `l` | Move cell cursor |
| `y` | Copy selected cell to clipboard (OSC 52) |
| `Y` | Copy selected row as TSV |
| `e` / `E` | Export results to `./exports/` as CSV / JSON |
| `Ctrl+F` | Open grid search prompt |
| `n` / `N` | Next / previous search match |
| `s` | Cycle sort on selected column (off → asc → desc) |
| `n` / `p` | Next / previous page (when no search is active) |

### Write Ops (Tables tab)

| Key | Action |
|---|---|
| `i` | Insert new row (opens form) |
| `Shift+U` | Update selected row (opens edit form with PK read-only) |
| `d` | Delete selected row (y/n confirm) |
| `Shift+D` | Truncate table (**typed-confirm**: type the table name) |
| `Ctrl+E` | Enter spreadsheet edit mode |

### Spreadsheet Edit Mode

| Key | Action |
|---|---|
| `h` / `j` / `k` / `l` | Move cell cursor |
| `Enter` / `i` | Open inline editor on selected cell |
| `Enter` (in editor) | Commit value to pending list |
| `Esc` (in editor) | Cancel inline edit |
| `s` | Save all pending edits (spawns batched `UPDATE`s) |
| `u` | Revert pending edit on active cell |
| `Ctrl+E` / `Esc` | Exit edit mode (asks to discard if pending > 0) |

### Admin (Sidebar — Databases)

| Key | Action |
|---|---|
| `a` | Add alias / human name to the selected database |
| `Shift+D` | **DELETE database** (typed-confirm: type the database name) |

### Module Tab

| Key | Action |
|---|---|
| `j` / `k` | Move between reducers |
| `Enter` | Open the reducer call form |

### SQL Console

| Key | Action |
|---|---|
| `:` | Jump into the SQL input |
| `Enter` | Execute query |
| `Tab` | Autocomplete keyword / table / column name |
| `↑` / `↓` | Browse query history |
| `Ctrl+L` | Clear entire input |
| `Ctrl+K` | Kill to end of line |
| `Ctrl+U` | Kill to start of line |
| `Ctrl+W` | Delete previous word |
| `Ctrl+A` / `Home` | Move cursor to start |
| `Ctrl+E` / `End` | Move cursor to end |

### Logs Tab

| Key | Action |
|---|---|
| `Space` | Pause / resume auto-scroll |
| `f` | Cycle minimum log level filter |
| `r` | Refresh logs |
| `c` | Clear log buffer |

### Modal Dialogs

| Key | Action |
|---|---|
| `Tab` / `↓` | Next field |
| `Shift+Tab` / `↑` | Previous field |
| `Enter` | Submit form / confirm |
| `y` | Confirm (yes/no prompts) |
| `n` / `Esc` | Cancel |

---

## 🪐 SpacetimeDB 2.0 Compatibility

Built and tested against **SpacetimeDB 2.0** HTTP + WebSocket APIs.

| SpacetimeDB Version | Supported | Notes |
|---|---|---|
| **2.0.x** | ✅ Full support | Primary target |
| **1.x** | ❌ Not supported | Breaking API differences |

### API Endpoints Used

| Feature | Endpoint |
|---|---|
| Schema inspection | `GET /v1/database/{db}/schema?version=9` |
| SQL execution | `POST /v1/database/{db}/sql` |
| Reducer calls | `POST /v1/database/{db}/call/{reducer}` |
| Database listing | `GET /v1/identity/{id}/databases` |
| Database names | `GET /v1/database/{db}/names` |
| Add alias | `POST /v1/database/{db}/names` |
| Delete database | `DELETE /v1/database/{db}` |
| Log streaming | `GET /v1/database/{db}/logs` |
| Live subscription | `GET /v1/database/{db}/subscribe` (WebSocket) |
| Metrics | `GET /metrics` (Prometheus format) |

### WebSocket Subprotocol

The TUI requests `v1.json.spacetimedb` and decodes `InitialSubscription`, `TransactionUpdate`, and `IdentityToken` messages in their externally-tagged SATS JSON form. Unknown variants are safely ignored.

---

## 🏗️ Architecture

```
src/
├── api/            HTTP + WebSocket client, SATS JSON types
├── state/          AppState, modal/palette/edit-mode state machines
├── ui/             ratatui renderers (tabs, sidebar, overlays)
├── config.rs       CLI args + cli.toml + user config merge
├── user_config.rs  ~/.config/spacetimedb-tui/{config,session}.toml
├── app.rs          Event loop, key dispatch, write pipeline
└── main.rs         Terminal setup + tokio runtime
```

Key design choices:

- **WebSocket subprotocol is `v1.json.spacetimedb`** so all messages are decoded with `serde_json`. No BSATN decoder required.
- **Write operations produce SQL literals directly from raw JSON values** via `json_to_sql_literal`, so `Identity` / `ConnectionId` / `U256` PKs round-trip correctly (`0xdeadbeef`, not `{__identity__:0xdeadbeef}`).
- **Primary key detection** prefers the server's declared PK (`primary_key: [u16]` in the v9 schema) with a heuristic fallback chain (autoinc → naming convention → column 0).
- **Permanent HTTP errors abort WebSocket retries**, so a bad database doesn't spam the log with reconnect attempts.
- **Modal dialogs and the command palette share the same "take + mutate + put back" borrow pattern** so input routing stays lint-clean under NLL.

---

## 🧪 Testing

```bash
cargo test
```

Current: **122 tests passing** across schema / SQL / WebSocket / edit-mode / modal / palette / export / clipboard / completion / syntax / config / theme / session / PK detection / json_to_sql_literal.

---

## 🤝 Contributing

Contributions are welcome! Whether it's a bug fix, new feature, or documentation improvement.

### Reporting Bugs

Please [open an issue](https://github.com/RazieLDG/spacetimedb-tui/issues/new) and include:
- Your OS and terminal emulator
- `spacetimedb-tui --version` output
- SpacetimeDB server version (`spacetime version`)
- Steps to reproduce

### Development

```bash
# Run with info logging
RUST_LOG=spacetimedb_tui=info cargo run -- --database my_db

# Run tests
cargo test

# Check warnings + formatting
cargo build
cargo fmt --check
```

---

## 📄 License

MIT License — Copyright © 2026 **Beyond Horizons Industries**

See [LICENSE](./LICENSE) for the full text.

---

<div align="center">

Built by [Alice AI](https://aliceos.ai) · Beyond Horizons Industries

*Exploring the SpacetimeDB universe, one terminal at a time.* 🛸

</div>
