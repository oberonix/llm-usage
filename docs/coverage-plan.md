# Coverage plan & test follow-ups

A working doc for contributors. Captures the current state of automated
test coverage, what the gap is, and a ranked plan to close it.

## Current state

- **127 unit tests** across the workspace.
- **53 % line coverage** workspace-wide (`cargo llvm-cov --workspace --summary-only`).

### What's well-tested

Pure logic modules — the ones a regression would be embarrassing in:

| File | Lines |
|------|------:|
| `core/src/model.rs` | 100.00 % |
| `core/src/pricing.rs` | 100.00 % |
| `core/src/quota.rs` | 100.00 % |
| `core/src/config.rs` | 96.86 % |
| `ui/src/icon.rs` | 93.62 % |
| `core/src/storage.rs` | 91.33 % |

Provider parsers and OAuth helpers are at 70–82 % — the missing lines
are mostly the async happy-path through `poll()`, which makes a live
HTTP call.

### What's not, and why

| File | Lines | Why |
|------|------:|-----|
| `core/src/updates.rs` | 23.26 % | Network fetch in `check()` — only the semver comparison is tested. |
| `dashboard/src/main.rs` | 10.53 % | egui rendering closures inside `update()`. |
| `dashboard/src/settings.rs` | 23.68 % | egui form widgets + child-process spawn. |
| `ui/src/main.rs` | 18.96 % | tao event loop, tray-icon menu wiring. |
| `ui/src/runtime.rs` | 0.00 % | tokio polling loop + reqwest. |
| `dashboard/src/history.rs` | 0.00 % | Walks `~/.claude/projects/**` JSONL files on disk. |
| `core/src/provider.rs` | 0.00 % | Trait declaration only (3 lines); no implementations live here. |
| `setup/src/main.rs` | 39.32 % | wry/tao event loop + GTK init dominates. |

If you discount the GUI / event-loop / live-network paths, the
testable library + CLI logic is at **~85–90 % covered**. The 53 %
workspace number is dragged down by code that genuinely needs
integration-style testing.

## Ranked plan to push higher

Each entry lists the rough effort, the file(s) it touches, and what
fraction of the gap it closes.

### 1. Mock the GitHub Releases call in `updates::check` &nbsp;·&nbsp; **S** &nbsp;·&nbsp; pulls `updates.rs` from 23 % → ~90 %

Add `wiremock = "0.6"` as a dev-dep. Refactor `check()` to accept a
`base_url: &str` (default `"https://api.github.com"`). Tests then
spin up a wiremock server and verify:

- Newer tag → `Some(UpdateInfo)` with the right `version` and `url`.
- Same tag → `None`.
- Draft tag → `None`.
- Pre-release tag → `None`.
- Non-200 → `Err`.
- Malformed body → `Err`.

```rust
// crates/core/src/updates.rs
pub async fn check_with(base: &str, current_version: &str) -> Result<Option<UpdateInfo>> { ... }
pub async fn check(current_version: &str) -> Result<Option<UpdateInfo>> {
    check_with("https://api.github.com", current_version).await
}
```

### 2. Fixture-tree tests for `dashboard::history::anthropic_daily_spend` &nbsp;·&nbsp; **S** &nbsp;·&nbsp; pulls `history.rs` from 0 % → ~80 %

The Anthropic provider already has `parses_real_schema` writing a
JSONL fixture into a `tempfile::TempDir`. Reuse the shape:

```rust
// In dashboard/src/history.rs
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn aggregates_daily_spend_for_last_n_days() {
        let dir = TempDir::new().unwrap();
        // Write a project/session.jsonl with two assistant entries on
        // two different days; assert the resulting Vec<(NaiveDate, f64)>
        // has both rows with correct totals.
    }

    #[test]
    fn returns_empty_when_no_projects_dir() { ... }

    #[test]
    fn deduplicates_consecutive_identical_message_ids() { ... }
}
```

Move the projects-dir resolution to a parameterised helper that takes
`&AnthropicConfig` so the test can point at the TempDir.

### 3. Wiremock-based test for Ollama Cloud `fetch_settings_html` &nbsp;·&nbsp; **M** &nbsp;·&nbsp; +5–10 % on `ollama_cloud.rs`

Same pattern as #1 — make the base URL injectable, run a wiremock
server that returns a recorded `/settings` HTML, verify:

- Successful parse → `Ok(UsageSnapshot)` with the right windows.
- 401 / 403 → error message "session cookie likely expired".
- Any other non-2xx → bubbles up.
- Body without recognisable markers → `Unavailable` with the
  "parse failed" diagnostic.

### 4. End-to-end snapshot-file integration test &nbsp;·&nbsp; **L** &nbsp;·&nbsp; high value, doesn't move the % much

Spawn the tray binary against a stub config that has every provider
disabled except a synthetic one. Wait for `snapshots.json` to appear.
Assert structure. Tear down. This catches the kind of regression
that no unit test will — a refactor that breaks `snapshots_io.rs` →
notify → dashboard wiring.

Use `assert_cmd` + `tempfile` for the harness. Goes in
`crates/core/tests/snapshot_file.rs` (integration test target).

### 5. Splash a few egui rendering smoke tests &nbsp;·&nbsp; **M** &nbsp;·&nbsp; mostly catches panics, not bugs

`eframe` can run headless via the `wgpu` feature with a software
renderer, but the setup is fiddly. Better: pull `egui::Context::new()`
directly in tests and exercise the form-render closures with a fake
ui. Won't render pixels but proves the widget tree builds.

### Stretch — bring CI quality bar up

Once the above lands:

- Add `cargo llvm-cov --fail-under-lines 60` to `.github/workflows/ci.yml`
  so PRs can't regress below baseline.
- Add `cargo deny check licenses` to assert workspace deps stay MIT /
  Apache 2.0 compatible.
- Add `cargo audit` to flag known CVEs in the lockfile.

## Test infrastructure

### Running

```bash
# Plain unit + integration tests
cargo test --release --workspace

# Coverage summary (installs cargo-llvm-cov on first run)
cargo llvm-cov --workspace --summary-only

# HTML report for clicking around per-file misses
cargo llvm-cov --workspace --html
xdg-open target/llvm-cov/html/index.html
```

### Fixture conventions

- Sample-of-the-real-thing fixtures (Codex rollouts, Anthropic
  JSONL, Ollama Cloud `/settings`) live inside the test module they
  exercise as `const SAMPLE: &str = r#"..."#;`. Keeps the schema and
  the assertions in one file so a parser change shows the matching
  fixture diff.
- Anything bigger than a screenful or anything that's bytes-not-text
  goes in `crates/<crate>/tests/fixtures/`.
- **Never** check in a real session cookie, OAuth token, or live
  API key. The `dump_ollama_cloud` example warns about this at
  runtime; treat fixtures the same way.

## Observations from the test-writing pass

Things I noticed while writing tests that are worth tracking but
weren't directly about coverage:

### macOS not yet validated

We haven't compiled or run any binary on macOS in CI or locally
(audit covered in the earlier session). The maintainer plans manual
validation; CI for macOS is part of `.github/workflows/release.yml`
but only fires on tag pushes. A no-op-tag dry run before the first
real release is the cheapest way to surface link errors.

### `ConfigDraft::to_config` re-loads from disk every call

`ConfigDraft::to_config` calls `Config::load_or_default()` to seed
non-editable fields, then overrides the editable ones. With auto-save
this fires on every Settings keystroke. The cost is small (one
~5 KB TOML parse) but unnecessary — caching the loaded base inside
the draft would tighten it.

### `cli/src/main.rs` is the only test that uses `BTreeMap` from a binary

Cargo binaries don't expose their modules to external test crates,
so the unit tests live in the `main.rs` itself (`#[cfg(test)] mod
tests { use super::*; }`). This pattern works fine but means
binaries can't be tested via integration tests in `tests/`.
Integration-style smoke tests need a separate crate or `assert_cmd`.

### `dashboard/src/history.rs` has no `enabled` short-circuit

`history.rs::anthropic_daily_spend` runs unconditionally even when
the Anthropic provider is disabled. Today this is gated by the
caller (`kick_daily_history` checks `enabled && show_spend`), but
moving the check into the function itself would be defensive.

### Codex parser's stale-window clamp depends on `Utc::now()`

`apply_rate_limits` uses `now: DateTime<Utc>` parameterised, but
the test that exercises the clamp would benefit from an explicit
fixture rather than a wall-clock comparison. Pure-function refactor
candidate.

### Public re-exports could be tightened

`core/src/lib.rs` re-exports `read_snapshots`, `write_snapshots`,
`touch_refresh_trigger`, and `SnapshotsFile` at the crate root.
Other modules (e.g. `Provider`, `Config`) are also re-exported.
Consider whether all of those need to be `pub use` at the root or
if some should live under their own modules — affects what an
external user of the lib would see.

### No tests for the singleton race

`try_acquire_singleton` + the focus-trigger pipeline is the most
subtle piece of cross-process plumbing in the project. The PID
reuse bug we fixed previously could regress and unit tests wouldn't
catch it. Worth a tempdir + spawn-and-kill harness.

## Why we won't chase 100 % blindly

Some lines genuinely don't pay off:

- `Drop` impls that only matter at process exit.
- `gtk::init()` calls behind `cfg(target_os = "linux")`.
- `tracing::warn!` / `tracing::error!` branches inside watcher
  callbacks.
- Display impls / error messages that are exercised by manual usage.

Trying to cover them through testing adds cost without insurance.
The 70–80 % line is the right floor; pure-logic modules should be at
or near 100 %.
