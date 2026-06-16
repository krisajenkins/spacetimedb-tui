# ☑ Timestamps show as numbers in the Tables view

# ☑ Timestamps show as 'Product' in the Modules->Tables view

# ☑ We want UI feedback in the left panel lift when a table is private

The module inspector uses a globe or lock prefix, so let's reuse that.

# ☑ I want to take a thorough look at data loading and caching.

- [x] When I view a database, it lists the tables. I go down to the next database, it lists those tables. I go back to the previous one and it says, "(no tables)" until I press refresh. The table list should've been in the cache.
- [x] When I select a database it highlights the first table, but it doesn't load that table's data.
- [x] When I scroll through tables it doesn't load the data immediately, because of debouncing. That's good if the data needs fetching, but if it's in cache there's no reason not to show it immediately. Debouncing should only affect remote calls, not UI responsiveness.

# ☑ Host/port flags have stopped working.

No matter what I pass, it just connects to maincloud.

# ☑ `-s/--server` flag support.

The official `spacetime` CLI tool supports a flag that lets you say `-slocal` or `-smaincloud` as a shortcut to the right host/port/tls setting. That's really helpful. Let's do that too.

# ☑ We should show views

Both in the table browser, and the module browser. Shown with an 👁️ icon?

# [ ] `-d/--database` doesn't work very well.

If the supplied database doesn't exist, it shows up as the selection in the GUI, but the first data load obviously fails.
If it _does_ exist it doesn't get selected, so the flag was pointless.
