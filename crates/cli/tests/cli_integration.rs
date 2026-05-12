//! Integration tests for the `llm-usage` CLI binary.
//!
//! These spawn the actual `target/<profile>/llm-usage` binary via
//! `assert_cmd` and assert on its observable behaviour — stdout
//! shape, exit code, ANSI escapes (or their absence). Anything that
//! would require a live HTTP call to a provider is sidestepped by
//! pointing `XDG_DATA_HOME` at a tempdir pre-seeded with a fresh
//! `snapshots.json` so `cached_snapshots()` wins over `poll_fresh()`.
//!
//! Why bother? The unit tests in `crates/cli/src/main.rs` cover the
//! pure helpers (`format_quota_row`, `colored_bar`, `pace_index`,
//! `quota_suffix`, the `build_screen` orchestration). What they
//! *can't* catch is the binary-level wiring: argument parsing,
//! `is_terminal()` colour detection, exit codes, the "couldn't load
//! config, here's a friendly error" path. One regression there on
//! launch day would have us debugging in the dark — these tests are
//! a thin, fast safety net for that.

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::{Path, PathBuf};

/// Project-dirs resolution mirrored from `directories::ProjectDirs`
/// so the integration tests can pre-seed the snapshot file at the
/// exact path the binary will read. Setting HOME + clearing
/// XDG_DATA_HOME (Linux) directs `ProjectDirs::from(...)` here.
fn seed_data_dir_under(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library")
            .join("Application Support")
            .join("dev.buffbit.llm-usage")
    } else {
        // Linux default when XDG_DATA_HOME is unset.
        home.join(".local").join("share").join("llm-usage")
    }
}

fn seed_config_dir_under(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        // macOS deliberately collapses config + data under
        // Application Support — directories follows Apple's HIG.
        home.join("Library")
            .join("Application Support")
            .join("dev.buffbit.llm-usage")
    } else {
        home.join(".config").join("llm-usage")
    }
}

/// Build a `Command` for the CLI binary that's hermetic — its HOME
/// points at a tempdir and any leaked XDG_*_HOME from the user's
/// shell is cleared. Caller seeds the relevant files under the
/// returned `home` path.
fn cli_with_isolated_home(home: &Path) -> Command {
    let mut cmd = Command::cargo_bin("llm-usage").unwrap();
    cmd.env("HOME", home)
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_CONFIG_HOME");
    cmd
}

#[test]
fn help_flag_prints_usage_and_exits_zero() {
    Command::cargo_bin("llm-usage")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("USAGE:"))
        .stdout(predicate::str::contains("--once"))
        .stdout(predicate::str::contains("--refresh"))
        // --help itself must be documented in the output.
        .stdout(predicate::str::contains("--help"));
}

#[test]
fn short_help_flag_is_equivalent() {
    Command::cargo_bin("llm-usage")
        .unwrap()
        .arg("-h")
        .assert()
        .success()
        .stdout(predicate::str::contains("USAGE:"));
}

#[test]
fn version_flag_prints_cargo_pkg_version() {
    Command::cargo_bin("llm-usage")
        .unwrap()
        .arg("--version")
        .assert()
        .success()
        // We don't pin the exact version (it bumps with releases);
        // just confirm the "llm-usage <X.Y.Z>" shape.
        .stdout(predicate::str::starts_with("llm-usage "));
}

#[test]
fn short_version_flag_is_equivalent() {
    Command::cargo_bin("llm-usage")
        .unwrap()
        .arg("-V")
        .assert()
        .success()
        .stdout(predicate::str::starts_with("llm-usage "));
}

#[test]
fn unknown_flag_exits_with_code_two_and_prints_usage() {
    Command::cargo_bin("llm-usage")
        .unwrap()
        .arg("--definitely-not-a-flag")
        .assert()
        .failure()
        .code(2)
        // Stderr surfaces the unknown flag so a typo is obvious…
        .stderr(predicate::str::contains("--definitely-not-a-flag"))
        // …and the usage block is reproduced on stdout to give the
        // user the correct spelling.
        .stdout(predicate::str::contains("USAGE:"));
}

#[test]
fn once_mode_renders_cached_snapshot_without_polling() {
    // Spin up a tempdir, drop in a fresh snapshots.json under the
    // platform-appropriate ProjectDirs path. With a fresh file
    // (`updated_at = now`), `cached_snapshots()` short-circuits and
    // we never make a real HTTP call.
    let dir = tempfile::TempDir::new().unwrap();
    let home = dir.path();
    let data = seed_data_dir_under(home);
    std::fs::create_dir_all(&data).unwrap();
    write_fresh_snapshots(&data.join("snapshots.json"));

    cli_with_isolated_home(home)
        .arg("--once")
        .assert()
        .success()
        .stdout(predicate::str::contains("Anthropic"))
        .stdout(predicate::str::contains("42%"))
        // Footer carries both timestamps so the user can distinguish
        // "tray polled at X" from "this frame was painted at Y".
        .stdout(predicate::str::contains("updated "))
        .stdout(predicate::str::contains("refreshed "));
}

#[test]
fn once_mode_with_no_data_prints_friendly_stderr_and_exits_one() {
    // Empty data dir → no cached snapshots. Combined with a config
    // that disables every provider, `poll_fresh()` returns an empty
    // map. The CLI's contract for "I have nothing to show you" is
    // an actionable stderr message + exit-1 — `--once` is meant to
    // be scriptable, so a silent zero-exit would mislead callers.
    let dir = tempfile::TempDir::new().unwrap();
    let home = dir.path();
    std::fs::create_dir_all(seed_data_dir_under(home)).unwrap();
    let cfg_dir = seed_config_dir_under(home);
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(
        cfg_dir.join("config.toml"),
        r#"
poll_interval_secs = 900
icon_rotation_secs = 15
show_pace_marker = false
check_for_updates = false

[anthropic]
enabled = false
warn_at = []

[codex_cli]
enabled = false
warn_at = []

[ollama_cloud]
enabled = false
warn_at = []

[alerts]
debounce_secs = 3600
disabled = true
"#,
    )
    .unwrap();

    cli_with_isolated_home(home)
        .arg("--once")
        .timeout(std::time::Duration::from_secs(15))
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("No provider data"))
        .stderr(predicate::str::contains("Settings"));
}

#[test]
fn once_mode_emits_no_ansi_when_stdout_not_a_tty() {
    // assert_cmd captures stdout into a pipe, so the CLI's
    // `is_terminal()` check returns false and colour is suppressed.
    // Verify we don't emit raw ANSI escape sequences in that case —
    // they'd corrupt downstream pipes (`| grep`, `> file`).
    let dir = tempfile::TempDir::new().unwrap();
    let home = dir.path();
    let data = seed_data_dir_under(home);
    std::fs::create_dir_all(&data).unwrap();
    write_fresh_snapshots(&data.join("snapshots.json"));

    let out = cli_with_isolated_home(home).arg("--once").output().unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        !stdout.contains('\u{001b}'),
        "non-TTY stdout must not contain ANSI escapes, got: {stdout:?}"
    );
}

// ---- helpers ----

/// Write a `snapshots.json` with one Anthropic-provider snapshot at
/// 42 % utilisation, timestamped now so `cached_snapshots()` accepts
/// it (the 5-minute staleness cap applies otherwise).
fn write_fresh_snapshots(path: &Path) {
    let now = chrono::Utc::now().to_rfc3339();
    let body = format!(
        r#"{{
            "updated_at": "{now}",
            "snapshots": {{
                "anthropic": {{
                    "provider": "anthropic",
                    "timestamp": "{now}",
                    "status": "ok",
                    "error": null,
                    "windows": {{
                        "5h": {{
                            "started_at": null,
                            "ends_at": null,
                            "spend_usd": null,
                            "tokens_in": 0,
                            "tokens_out": 0,
                            "request_count": 0,
                            "limit_usd": null,
                            "limit_tokens": null,
                            "fraction_used": 0.42,
                            "stale": false
                        }}
                    }},
                    "headline": "5h 42%",
                    "plan_label": "Max"
                }}
            }}
        }}"#
    );
    std::fs::write(path, body).unwrap();
}
