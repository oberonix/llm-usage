# Next-pass worklist

A checkpoint of in-flight + still-open work. Six items from the last
batch are partially landed (all changes are in the working tree but
uncommitted), and one new item — adding more activity stats to the
Codex dashboard card — was just filed.

## Status of last batch

| # | Ask | State |
|---|---|---|
| 1 | Stop alerts firing on app open | ✅ implemented (`runtime.rs`) — first-poll alerts are still recorded in SQLite but not dispatched as notifications. |
| 2 | Unify the Codex + Ollama Cloud cookie UI | ✅ implemented (`settings.rs`) — shared `render_cookie_import_block` helper drives both cards (same green button, "✓ captured" pill, status line). The Ollama-only popup-webview fallback stays gated behind `allow_popup: true`. |
| 3 | Don't hit Anthropic's `/usage` rate limit; report how often | ✅ implemented (`anthropic.rs`) — `MIN_HTTP_INTERVAL` bumped 300 s → **900 s** (4 calls/hour ceiling). Each actual HTTP call now logs at `info`. The user can confirm cadence via `RUST_LOG=info` in `/tmp/llm-usage-tray.log`. |
| 4 | Pacing markers on the dashboard bars | ✅ implemented (`dashboard/src/main.rs`) — 2 px overlay using `Painter::rect_filled` against the `ProgressBar` response rect. New free fn `pace_fraction(label, w, now)` mirrors the CLI's `pace_index` and the tray icon's `bar_slot_for` so all three surfaces agree on what a "5h" or "week" window means. |
| 5 | White pace marker over red bars | 🟡 partial — done in the tray icon (`icon.rs`) and dashboard. **CLI is still magenta**. The marker swap in `crates/cli/src/main.rs` (`colored_bar`) needs the same `if frac >= 0.85 && !stale → white` branch. Probably 10 lines + a test update. |
| 6 | Capital-letter / nicer error messages | ✅ implemented — Anthropic 429 + cooldown messages now read "Rate-limited by Anthropic — backing off" / "Rate-limited by Anthropic — refresh paused for N min"; the `"quota:"` prefix dropped. Ollama Cloud's "not signed in" / "fetch failed" / "parse failed" all start with a capital. |

## Open: more activity stats in the Codex dashboard card

The Codex card stands out in the dashboard because it shows only two
rows (5h + week, both quota-bearing). The other providers also
surface activity-only windows (1h, today, month for Anthropic) with
token counts + spend, which gives those cards visual weight and
useful data.

Codex's provider DOES already compute `tokens_in`, `tokens_out`,
`request_count`, and `spend_usd` for the 5h and week buckets — they
just aren't surfaced anywhere visible. Look at
`crates/core/src/providers/codex_cli.rs` around
`bucket_5h` / `bucket_7d` assignment.

Plausible additions, in rough order of effort:

1. **Show tokens / requests under the 5h and week quota bars.**
   `render_window_usage` already renders `spend_usd` as a "$X.YZ"
   chip when set. Easiest: also render "tokens_in in / tokens_out
   out / N reqs" as weak text alongside.
2. **Add a `today` activity-only window for Codex.** Aggregate
   today's rollout events the same way Anthropic's `Aggregate::add`
   does (`Aggregate { last_hour, today, this_week, this_month }`).
   The codex provider currently only fills the 5h + 7d buckets; the
   rollouts have per-turn timestamps so an hour / day / month
   breakdown is free.
3. **Add `1h` + `month` windows too.** Same pattern as Anthropic so
   the dashboard layouts match.
4. **Per-model breakdown.** The Codex rollouts carry `current_model`
   per turn (gpt-5-codex, gpt-5, …). Could split tokens by model the
   way Anthropic splits its weekly window into Sonnet / Opus.

The first option is the smallest change with the most user-visible
density bump. Pick one before starting.

## In-tree state right now

Six files modified, uncommitted:

- `crates/core/src/providers/anthropic.rs`
- `crates/core/src/providers/ollama_cloud.rs`
- `crates/dashboard/src/main.rs`
- `crates/dashboard/src/settings.rs`
- `crates/ui/src/icon.rs`
- `crates/ui/src/runtime.rs`

All compile (`cargo build --workspace`); the 298-test suite passes
(`cargo test --workspace`); `cargo clippy --workspace --all-targets
-- -D warnings` last ran clean.

Recommended commit shape when picking this back up:

1. Item #1 (`runtime.rs` first-poll alert suppression) — standalone.
2. Item #3 (`anthropic.rs` throttle bump + log + message casing) —
   standalone.
3. Item #6 misc casing (`ollama_cloud.rs` casing + test
   `to_lowercase()` updates) — fold into the message-tightening
   commit alongside item #3.
4. Item #2 + #4 + #5 (dashboard cookie-UI unification,
   pace-marker overlay, white-on-red logic for icon + dashboard) —
   one "dashboard pace + cookie UI" commit. Then a follow-up
   "cli: white pace marker over red bar" once the CLI side lands.

## Out-of-batch work to do next

After the above lands:

- The new **Codex dashboard activity stats** item from above.
- Anything else the user surfaces from the next live session.
