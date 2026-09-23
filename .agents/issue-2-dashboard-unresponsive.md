# Issue 2 — OS "dashboard app is unresponsive" dialog (Omarchy/Wayland)

Status: diagnosed to hypothesis stage, NOT fixed. Issue 1 (agy/Gemini quota
wrong host) is fixed separately and is unrelated.

## Symptom (user-reported)

On Omarchy, the OS shows a "not responding" dialog for `llm-usage-dashboard`
even though the dashboard window itself is interactive and responsive.
User's hypothesis: "there is a health response that is not being handled."

## Finding: there is no health-check protocol to handle

Searched all crates, `packaging/`, `config.example.toml`, `README.md` for
`health / heartbeat / ping / watchdog / unresponsive / responsive`:
no app-level health endpoint, heartbeat, or readiness probe exists anywhere
in the repo. The only `ready` is the `ready: bool` field in the Omarchy
JSON export (`crates/core/src/omarchy_export.rs`), which is file output,
not a live handshake.

On Wayland (Omarchy uses Hyprland), an OS "not responding" dialog means the
compositor's shell ping timed out. Ping/pong is answered inside winit's
event loop, not by app code — so the cause is almost certainly something
intermittently blocking the dashboard's main thread, not a missing handler.

## What was ruled out (read, not guessed)

- `DashboardApp::update` per-frame work is cheap: one
  `request_repaint_after(2s)`, mutex clones of the snapshot map
  (`crates/dashboard/src/main.rs`, `impl eframe::App`, ~line 318).
- Snapshot reloads run on notify-watcher threads, not the UI thread
  (`spawn_snapshots_watcher`, `reload_snapshots`, ~lines 1384–1433).
- The 14-day Anthropic history walk runs on a spawned thread
  (`kick_daily_history`, ~lines 1437–1446).
- Settings launches the setup binary on a spawned thread
  (`crates/dashboard/src/settings.rs`, ~line 297).
- Tray event loop uses `ControlFlow::WaitUntil(250ms)`
  (`crates/ui/src/main.rs`, ~line 122) — stays responsive by construction.

## Open suspects (not yet verified)

1. **Settings browser-cookie import (`rookie`) may run inline on the egui
   thread.** The setup-binary spawn is threaded, but the `rookie` import
   path itself was never traced end-to-end. On Linux it can raise a
   libsecret/keyring prompt, which blocks. Check: does the "not responding"
   dialog correlate with using Settings → Ollama Cloud → "Import from
   browser…"?
2. **`auto_save_if_changed` file write every frame while editing Settings**
   (`crates/dashboard/src/main.rs`, ~line 615). Normally fast, but worth
   confirming it can't stall (full disk, slow fs).
3. **GPU/vsync stall on Hyprland+NVIDIA.** A blocked swap would also starve
   the event loop. Check with `WAYLAND_DEBUG=1` whether ping/pong flows.
4. **Zombie dashboard process.** Observed live: a defunct
   `[llm-usage-dashb] <defunct>` alongside the running dashboard.
   `spawn_dashboard` drops the `Child` without reaping
   (`crates/ui/src/main.rs`, ~line 976–989), so every exited dashboard
   lingers as a zombie. May also leave stale singleton pid-file state
   (`dashboard.pid` + `dashboard.focus` in `~/.local/share/llm-usage/`).
   Probably a hygiene issue rather than the dialog cause, but worth fixing
   in the same pass.

## Evidence still needed (next steps)

1. Next time the dialog appears, capture all of:
   - screenshot / exact dialog text,
   - what the dashboard was doing (which tab, idle / typing / just clicked Refresh?),
   - `hyprctl clients` output,
   - dashboard stderr / `RUST_LOG=debug` logs,
   - one stack sample of the dashboard PID:
     `gdb -p <pid> -batch -ex 'thread apply all bt'` (or `perf record -g -p <pid>` briefly).
   One capture usually names the blocker.
2. Audit the Settings `rookie` import path for UI-thread blocking.
3. Reap dashboard children in the tray (retain `Child`, or spawn detached);
   add logging to the singleton acquire path so the next zombie leaves a trace.

## Process notes (2026-09-23 session)

- Running tray: `/home/oberonix/code/llm-usage/target/release/llm-usage-tray`
  (PID 1486 at time of writing).
- Running dashboard: `target/release/llm-usage-dashboard` + one defunct entry.
- `~/.local/share/llm-usage/` holds `dashboard.pid`, `dashboard.focus`,
  `refresh.trigger`, `snapshots.json`, `usage.sqlite`.
- No code was changed for this issue. Focused work so far: read-only
  (reads, greps, `ps`, log inspection).
