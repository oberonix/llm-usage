# Launch readiness — autonomous worklist

Tracking file for the overnight autonomous pass. Each iteration:
read this file → pick the topmost open item → land it → tick the box
→ commit → schedule next wake-up.

**Source baseline:** 2026-05-11, 55.92 % line coverage, 130 unit tests,
all green on `cargo test --workspace`.

**Working rules**
- Land work in small, focused commits with a clear message — one
  item per commit unless they're trivially related.
- Run `cargo test --workspace --quiet` and `cargo clippy --workspace
  --quiet -- -D warnings` (if clippy is configured) before each
  commit.
- Update this file in the same commit as the work.
- Never amend prior commits; always create a new one.
- Don't touch `~/.codex`, `~/.claude`, `~/.local/share/opencode` —
  those are the user's live data, not fixtures.
- If a task turns out to be wrong or much bigger than expected,
  cross it out (`~~strikethrough~~`) and leave a `> note: …` line
  explaining why.

## Quick status

- Coverage baseline: **55.92 %** lines.
- After current pass: see footer.

---

## A. Test coverage — net-new (ranked by leverage)

These come from `docs/coverage-plan.md`, re-ranked by "biggest
coverage-per-effort" and re-stated in the form of concrete actions.

- [x] **A1. Mock the GitHub Releases call in `updates::check`** — `3eff196`. `updates.rs` 23 → 97 %.
- [x] **A2. Fixture-tree tests for `dashboard::history::anthropic_daily_spend`** — `2452679`. `history.rs` 0 → 96 %.
- [x] **A3. Wiremock test for Ollama Cloud `/settings` scrape** — `f63d5d5`. `ollama_cloud.rs` 63 → 84 %.
- [x] **A4. Snapshot-file robustness tests** — `53e9142`. `snapshots_io.rs` 65 → 85 %; schema-forward-compat + crash-recovery covered.
- [x] ~~A5. Test `UsageSnapshot::merge_stale_from` runtime integration~~
  > note: punted — merge helper has full unit coverage in `model.rs`, and
  > the runtime loop's only new contribution (seed-from-disk → poll →
  > graft → write) is mechanical glue around well-tested calls. Adding
  > a `LoopState` seam would inflate surface area without finding bugs.
  > Re-open if a regression slips through.
- [x] **A6. Tests for opencode SQLite reader corner cases** — `c9fdd9c`. `opencode.rs` 85 → 97 %.
- [x] **A7. Anthropic OAuth: HTTP path coverage** — `941e7fc`. `anthropic_oauth.rs` 78 → 95 %.
  > note: original A7 framing was "OAuth refresh"; the code intentionally has
  > no refresh path (it surfaces `Expired` and tells the user to re-auth via
  > Claude Code). Reframed to test the `fetch_usage` HTTP surface instead.
- [x] **A8. Singleton-acquire race test** — `4e43157`. PID-reuse regression guarded.
- [x] **A9. Codex parser: rate_limits corner cases** — `5fbac2c`. `codex_cli.rs` 83 → 85 %.

---

## B. Bugs / risk hunt

Things to actively look for, not just write tests for. Add findings
inline as bullets when discovered.

- [ ] **B1. Audit error paths for `unwrap`/`expect`/`panic` in non-test code**
  - Run `grep -rn 'unwrap()\|expect(' crates/*/src/` and triage. The
    Provider trait runs inside a tokio task — a panic there silently
    kills the polling loop on that provider.

- [ ] **B2. Audit `tokio::time::sleep` / select! for cancel-safety**
  - The runtime's `select!` has three branches. Verify that
    cancelling the refresh-notified branch doesn't lose state.

- [ ] **B3. Config reload semantics**
  - When the user changes `poll_interval_secs` from 900 → 60 mid-run,
    is the in-flight sleep cancelled? Verify the reload Notify wakes
    the select! and the new interval applies on the next iteration.

- [ ] **B4. Snapshot-file growth / staleness on disabled providers**
  - When a provider is toggled disabled, its entry is removed
    in-memory but the on-disk file still has it after the next
    write. Confirm the write replaces the file wholesale (it does,
    via atomic write) — but seed-on-startup will re-introduce the
    disabled provider into the in-memory map for one iteration.
    Maybe filter at read time.

- [ ] **B5. Anthropic OAuth: token write race**
  - When the credentials file is refreshed by Claude Code in
    parallel with the tray, both may want to write. Confirm we
    re-read on every poll rather than caching the parsed cred for
    the lifetime of the provider.

- [ ] **B6. macOS path resolution**
  - Spot-check every `~/.codex`, `~/.claude`, `~/.local/share/opencode`
    site for a macOS-equivalent. opencode uses XDG on Linux but
    Library/Application Support on macOS. Confirm we handle both.

- [ ] **B7. `--release` build is the one used in distribution**
  - Confirm `cargo test --release` still passes (some bugs only
    show with optimizations on, eg. inlined floating-point rounding).

- [ ] **B8. Tracing levels in shipped binaries**
  - `tracing::debug!` should be filtered out by default. Confirm
    `tracing_subscriber` filter respects `RUST_LOG` and defaults to
    `info` everywhere.

- [ ] **B9. The 5-min stale grace is hard-coded in three places**
  - `apply_rate_limits` (codex), `quota_suffix` (tray), `quota_suffix`
    (cli). Now that I've simplified the renderers to dispatch on the
    `stale` flag, the renderers don't repeat the grace logic. But
    the provider-side grace is still a magic number. Consider
    making it a `const STALE_GRACE: Duration`.

- [ ] **B10. Provider list rebuild on config reload**
  - `build_state` recreates every Provider from scratch. If a
    Provider had any internal cache (eg. HTTP client connection
    pool), that's dropped. Verify there's no surprising perf hit.
    Mostly informational.

---

## C. Improvements / polish

- [ ] **C1. `merge_stale_from`: keep last-fresh-time per window**
  - Right now we know "this is stale" but not "this is from
    3 hours ago". Adding a `last_fresh_at: Option<DateTime<Utc>>`
    field would let renderers say "5 % · ⚠ 3h ago" instead of just
    "5 % · ⚠". Trade-off: more UI noise. Worth it? Decide after
    living with the current behaviour for a day.

- [ ] **C2. Cap cache age**
  - If Anthropic OAuth has been broken for 7 days, the cached 55 %
    no longer reflects reality. Should we expire from cache after
    some absolute age? Probably 7d.

- [ ] **C3. Centralise the warning character**
  - `" · ⚠"` appears in two renderers (tray + cli). Move to a
    `WARN_MARK: &str = "⚠"` const in `core::model` or a small
    `core::display` module so a future "swap to ??" change is
    one-line.

- [ ] **C4. Headline "(no headline)" placeholder leaks into UI**
  - Saw this once in `print_snapshots` output during a degraded
    poll. Make sure the tray menu / CLI don't render that literal —
    they should fall back to the cached headline (the merge does
    this) or skip.

- [ ] **C5. Coverage gate in CI**
  - Add `cargo llvm-cov --workspace --fail-under-lines 60` to
    `.github/workflows/ci.yml`. Bump the floor each time a coverage
    PR lands.

- [ ] **C6. `cargo deny` + `cargo audit` in CI**
  - Same workflow file. Helps catch licence drift and CVE'd deps
    pre-tag.

- [ ] **C7. README "first run" walkthrough screenshot**
  - The README has setup steps but no picture of the tray icon /
    menu / dashboard. A single screenshot at the top conveys the
    value prop instantly. Out of scope for autonomous code work —
    note here for the maintainer.

---

## Log

`{timestamp} — {what got done in this iteration, in 1–2 lines}`

- 2026-05-11 — created this file from `docs/coverage-plan.md`,
  captured 55.92 % baseline.
- 2026-05-11 — A1: wiremock the GitHub Releases lookup; 9 new tests;
  `updates.rs` 23 → 97 %, workspace 55.92 → 57.64 %. `3eff196`.
- 2026-05-12 — A2..A9 landed across `2452679`, `f63d5d5`, `53e9142`,
  `c9fdd9c`, `941e7fc`, `4e43157`, `5fbac2c`. Coverage 57.64 → 66.80 %.
  Net 67 new tests; nothing flaky; A5 punted with rationale.

---

## Footer (updated each pass)

- Latest cargo test workspace: **199 passed, 0 failed.**
- Latest coverage: **66.80 % lines.**
- Open items: 17 (A: 0, B: 10, C: 7).
