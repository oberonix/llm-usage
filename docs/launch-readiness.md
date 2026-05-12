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

- [x] **B1. unwrap/expect/panic audit** — clean.
  > Every non-test `unwrap`/`expect` falls into one of three safe
  > buckets: (a) provably-safe ops like `Utc.timestamp_opt(0, 0)` and
  > pre-compiled regex/icon construction, (b) `Mutex::lock().expect("poisoned")`
  > where panic propagation is the right semantic, (c) `reqwest::Client::builder()`
  > whose only failure is a misconfigured TLS backend baked at compile time.
- [x] **B2. tokio select! cancel safety** — audited, correct.
  > `Notify::notify_one` queues one permit if no waiter exists; on
  > select! cancellation, `Notified::drop` returns any claimed permit.
  > So a refresh / reload signal arriving between iterations is
  > preserved across the next `notified()` poll.
- [x] **B3. Config reload semantics** — audited, correct.
  > `state.interval` is captured per `build_state(new_cfg)`. When the
  > reload branch wins, the in-flight `tokio::time::sleep` is dropped
  > (cancel-safe), state is rebuilt with the new interval, and the
  > next iteration's sleep uses it.
- [x] **B4. Snapshot-file growth on disabled providers** — audited, correct.
  > The poll-loop removes `!provider.enabled()` from the in-memory
  > map before `write_snapshots`. Seed-on-startup re-introduces them
  > for one iteration but the first loop pass scrubs them. No durable
  > drift.
- [x] **B5. Anthropic OAuth token write race** — audited, no race.
  > `OAuthCredentials::load()` re-reads the file on every poll. We
  > never write to that file; Claude Code is the sole writer. A read
  > mid-Claude-Code-write fails JSON parse → one degraded snapshot,
  > recovers on next poll.
- [~] **B6. macOS path resolution** — mostly correct, one open question.
  > `~/.codex`, `~/.claude/.credentials.json`, `~/.claude/projects/`
  > are written by Claude Code / codex CLI consistently across
  > platforms. Open: `dirs::data_dir()` resolves to
  > `~/Library/Application Support/opencode/opencode.db` on macOS;
  > confirm opencode's macOS build actually writes there (vs an
  > XDG-style `~/.local/share/opencode/`). Manual check needed.
- [x] **B7. `cargo test --release` passes** — verified.
- [x] **B8. Tracing levels in shipped binaries** — aligned at `info`.
  > Tray's default dropped from `info,llm_usage=debug,llm_usage_core=debug`
  > to plain `info` to match dashboard / setup. Users debug via
  > `RUST_LOG=...`.
- [x] **B9. STALE_GRACE_SECS const** — landed in `e0b5d81`.
- [~] **B10. Provider list rebuild on config reload** — informational.
  > Each reload constructs three new providers, each with a fresh
  > `reqwest::Client` (no shared keep-alive pool). Cold-start cost
  > of building a reqwest client is sub-millisecond; ignoring.

---

## C. Improvements / polish

- [ ] **C1. `merge_stale_from`: keep last-fresh-time per window**
  - Right now we know "this is stale" but not "this is from
    3 hours ago". Trade-off: more UI noise. Worth it? Decide after
    living with the current behaviour for a day.

- [x] **C2. Cap cache age at 7 days** — `31d63ef`.

- [ ] **C3. Centralise the warning character**
  - `" · ⚠"` appears in two renderers (tray + cli). Move to a
    `WARN_MARK: &str = "⚠"` const so a future "swap to ??" change
    is one-line. Low priority — current is two-site, trivially
    grep-able.

- [x] **C4. Headline "(no headline)" placeholder leaks into UI** —
  handled by `merge_stale_from`'s headline fallback. The literal
  "(no headline)" only appears in `print_snapshots` (a debug
  example), not the tray/CLI/dashboard.

- [x] **C5. Coverage gate in CI** — landed in `acf8775`. Floor 60 %.

- [ ] **C6. `cargo deny` + `cargo audit` in CI**
  - Helps catch licence drift and CVE'd deps pre-tag. Worth doing
    pre-launch — separate PR.

- [ ] **C7. README "first run" walkthrough screenshot**
  - Out of scope for autonomous work — note for the maintainer.

- [ ] **C8. clippy cleanup pass (NEW)**
  - `cargo clippy --workspace --all-targets -- -D warnings` flags
    ~17 nits in the test code I added (mostly
    `field_reassign_with_default` patterns: building a struct via
    `Default::default()` then assigning fields). Tidying them up
    would let CI enforce `-D warnings` without false positives.
    Also: `items after a test module` in `config.rs` (real code
    comes after the `#[cfg(test)] mod tests` block) and one
    `unnecessary use of get().is_none()` in `anthropic.rs:720`.

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
- 2026-05-12 — second iteration: anthropic `apply_oauth_usage`
  corner tests (`e0b5d81`); STALE_GRACE_SECS const; cache-age cap
  at 7 days (`31d63ef`); tray tracing default aligned to `info`;
  full B-list audit (clean except B6 macOS opencode path needs
  manual check); CI gains workspace tests + 60 % coverage gate
  (`acf8775`). C2/C4/C5 done. New item C8 captures clippy nits.

---

## Footer (updated each pass)

- Latest cargo test workspace: **224 passed, 0 failed.**
- Latest coverage: **~67.7 % lines.**
- Open items: 5 (A: 0, B: 1 [B6 macOS manual check], C: 4 — C1, C3, C6, C7, C8).
