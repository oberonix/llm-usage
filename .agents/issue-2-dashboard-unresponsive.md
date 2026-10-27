# Issue 2 — OS "dashboard app is unresponsive" dialog (Omarchy/Wayland)

Status: mitigated on 2026-07-24. The warning is no longer reproduced after switching the Hyprland dashboard to XWayland.

## Symptom

Hyprland 0.56.2 displayed its "Application Not Responding" dialog for `llm-usage-dashboard` a few seconds after the dashboard lost focus. The window remained interactive and no dashboard thread was stuck.

## Confirmed behavior

- Hyprland's ANR manager sends an `xdg_wm_base.ping` every 1.5 seconds and uses the configured `misc:anr_missed_pings = 3` threshold.
- The dashboard main thread remained in normal `poll`/Wayland waits, and CPU usage stayed low.
- No application-level health endpoint or focus-change deadlock was involved.
- Native Wayland with eframe 0.29.1 / winit 0.30.13 failed to deliver the required pong reliably while unfocused. `WAYLAND_DEBUG=1` changed the timing and produced immediate ping/pong replies.
- Repaint-based workarounds were insufficient, so the production workaround avoids the failing native-Wayland path rather than disabling Hyprland's watchdog.

## Implemented fixes

- `crates/ui/src/main.rs`: retain tray-spawned dashboard children and reap them without blocking the tray event loop.
- `crates/dashboard/src/settings.rs`: run browser-cookie/keyring discovery on a background thread and return results through mpsc.
- `crates/dashboard/src/main.rs`: add singleton acquisition/focus diagnostics and poll background results from the dashboard event loop.
- `crates/dashboard/Cargo.toml`: depend on the exact winit 0.30.13 version expected by eframe.
- On Hyprland only, create the dashboard through XWayland and apply a `0.8` egui zoom factor to preserve the previous visual scale.

## Cleanup

- The experimental 500 ms delayed repaint was reverted to the existing 2-second UI refresh.
- The experimental background repaint/keepalive thread was removed after it failed to stop the warning.
- Hyprland's ANR settings were not changed or disabled.
- The zombie reaper and browser-import worker address separate confirmed problems and should remain.

## Trade-offs

- The Hyprland path now uses XWayland, adding the normal XWayland bridge overhead and losing native Wayland-only clipboard/drop/color behavior for this window.
- Other Linux compositors, macOS, and Windows remain on their native backends.
- The `0.8` zoom is a visual compatibility adjustment for XWayland scaling; egui's own Ctrl+scroll zoom still works at runtime.
- A future native-Wayland fix should target winit/eframe queue flushing or the unused `smithay-clipboard` secondary queue rather than disabling ANR detection.

## Verification

- The native-Wayland build reproduced Hyprland's dialog after focus loss.
- The Hyprland XWayland build passed repeated focus-away cycles without a dialog.
- `cargo check --workspace --all-targets`, `cargo test --workspace`, and warning-denied Clippy pass.
