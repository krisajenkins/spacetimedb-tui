# spacetimedb-tui — project notes

A terminal UI for SpacetimeDB. Targets **SpacetimeDB 2.x** (verified against
CLI/lib 2.5.0). Single **binary** crate — run tests with
`cargo test --bin spacetimedb-tui` (there is no lib target).

## SpacetimeDB API gotchas

- **Schema endpoint is `GET /v1/database/<db>/schema?version=9`.** `version=9`
  is the _current_ module-def format for 2.5.0 — active databases return 200.
  A schema failure is almost never a version/protocol mismatch; check the
  status code and body before assuming the client is out of date.

- **Maincloud pauses inactive databases.** A paused database returns
  `503 database is paused` on _every_ endpoint — HTTP schema, HTTP SQL, and
  even the WebSocket `/subscribe` upgrade. The client **cannot** resume it
  (HTTP reads, SQL, and WS connects all 503); the official `spacetime` CLI
  hits the same wall. Resume only from the dashboard at
  https://spacetimedb.com or by republishing. This is server-side state, so
  it affects _some_ of a user's databases and not others.

## Reproducing against the real server

Auth/server come from `~/.config/spacetime/cli.toml` (XDG path on macOS too,
not `~/Library`). The bearer token is the top-level `spacetimedb_token` key
(not `web_session_token`). To hit an endpoint directly:

```bash
TOKEN=$(grep '^spacetimedb_token' ~/.config/spacetime/cli.toml | sed 's/.*= *//;s/"//g')
curl -s -H "Authorization: Bearer $TOKEN" \
  "https://maincloud.spacetimedb.com/v1/database/<db>/schema?version=9"
spacetime list   # enumerate the identity's databases
```

## State model

- Databases are stored as `state.databases: Vec<Database>`, where each
  `Database` carries a `name` and a `status` (`DatabaseStatus::Unknown` /
  `Active` / `Paused`). Defined in `state/app_state.rs`, re-exported from
  `state`.
- **Status can't be read from the database list** (it returns names only),
  so it's discovered reactively, not on startup. `get_schema` returns a
  typed `DatabasePaused` error on a `503 ... paused` response; the app turns
  that into `AppEvent::SchemaPaused`, sets the database's status, and the
  sidebar renders a `⏸ paused` marker. A successful schema load flips it
  back to `Active`. A startup probe (one schema check per DB) was considered
  and deliberately not added — kept reactive to avoid N requests on launch.

## UI conventions

- `state.error_message` is modal: while set, `handle_key` swallows every key
  except Esc/Enter. It must therefore be rendered as a visible overlay
  (`ui/components/error.rs`), not just the truncated status-bar indicator —
  otherwise the UI looks frozen.
