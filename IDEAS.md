# Ideas

Backlog of features worth considering. Ranked roughly by value.

## Inspired by Stargate (stargate-client.com)

A competing SpacetimeDB GUI client. We're at parity or ahead on most of its
advertised features (SQL console, reducer console, live WebSocket view, log
streaming, CSV/JSON export, scheduled-task marking) — plus extras it doesn't
advertise (metrics dashboard, spreadsheet batch-edit, full CRUD, command
palette, theming, session restore, clipboard). The genuine gaps:

1. **Table pagination + WHERE filtering** — _biggest gap, hits every user._
   We currently hardcode `SELECT * FROM table LIMIT 200` (`app.rs:1437`) and
   Ctrl+F is text-highlight, not row filtering. Need a real filter expression
   (or at least offset paging past 200 rows). Stargate advertises
   "paginating through rows with filtering."

2. **Display indexes & constraints** — _cheap win, data already parsed._
   `TableInfo.indexes` and `TableInfo.constraints` are populated from the
   schema but never rendered in the Module Inspector. Just needs UI.

3. **Read-only mode toggle** — _safety polish, low effort._
   We have strong per-operation safety (typed-confirm for truncate/delete, PK
   protection) but no session-wide read-only flag. A global "I'm just looking"
   lock is a different guarantee than per-op confirms. Stargate makes this a
   headline (and ties its monetization to the read/write split).
