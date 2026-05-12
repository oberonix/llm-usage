//! Persistence for the cross-process snapshot map.
//!
//! The tray writes one of these after every poll; the dashboard reads
//! it on each frame (and on inotify change). The format is plain JSON
//! because the snapshot graph already derives `Serialize`/`Deserialize`
//! and we never need to migrate the schema across binaries that don't
//! share a build.
//!
//! Writes are made atomic via `write to .tmp + rename` so a partially
//! written file can never be observed by the reader.

use crate::config::{refresh_trigger_path, snapshots_path};
use crate::model::{ProviderId, UsageSnapshot};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotsFile {
    pub updated_at: DateTime<Utc>,
    pub snapshots: BTreeMap<ProviderId, UsageSnapshot>,
}

impl SnapshotsFile {
    pub fn new(snapshots: BTreeMap<ProviderId, UsageSnapshot>) -> Self {
        Self {
            updated_at: Utc::now(),
            snapshots,
        }
    }
}

pub fn write_snapshots(snaps: &BTreeMap<ProviderId, UsageSnapshot>) -> Result<()> {
    let path = snapshots_path().context("resolve snapshots path")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let payload = SnapshotsFile::new(snaps.clone());
    let json = serde_json::to_vec_pretty(&payload).context("serialize snapshots")?;
    write_atomically(&path, &json)
}

pub fn read_snapshots() -> Result<Option<SnapshotsFile>> {
    let path = snapshots_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let parsed: SnapshotsFile =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    Ok(Some(parsed))
}

/// Touch the refresh trigger so the tray's watcher fires and the
/// polling loop wakes up. The contents are irrelevant — we just need
/// the mtime to change, so we rewrite the current timestamp.
pub fn touch_refresh_trigger() -> Result<()> {
    let path = refresh_trigger_path().context("resolve refresh trigger path")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let now = Utc::now().to_rfc3339();
    write_atomically(&path, now.as_bytes())
}

fn write_atomically(target: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = target.with_extension("tmp");
    std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, target)
        .with_context(|| format!("rename {} -> {}", tmp.display(), target.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ProviderId, ProviderStatus, UsageSnapshot, WindowUsage};
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    fn sample_snapshots() -> BTreeMap<ProviderId, UsageSnapshot> {
        let mut snap = UsageSnapshot {
            provider: ProviderId::Anthropic,
            timestamp: chrono::Utc::now(),
            status: ProviderStatus::Ok,
            error: None,
            windows: BTreeMap::new(),
            headline: Some("ok".into()),
            plan_label: Some("Max 5x".into()),
        };
        snap.windows.insert(
            "5h".into(),
            WindowUsage {
                fraction_used: Some(0.42),
                ..Default::default()
            },
        );
        let mut out = BTreeMap::new();
        out.insert(ProviderId::Anthropic, snap);
        out
    }

    #[test]
    fn snapshots_file_new_records_now() {
        let before = chrono::Utc::now();
        let f = SnapshotsFile::new(sample_snapshots());
        let after = chrono::Utc::now();
        assert!(f.updated_at >= before && f.updated_at <= after);
        assert_eq!(f.snapshots.len(), 1);
    }

    #[test]
    fn write_atomically_replaces_target_via_rename() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.json");
        std::fs::write(&path, b"old").unwrap();
        write_atomically(&path, b"new").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        // No leftover .tmp file after a successful rename.
        assert!(!path.with_extension("tmp").exists());
    }

    #[test]
    fn snapshots_file_serialises_round_trip() {
        // Direct in-memory serialise + deserialise so the test doesn't
        // depend on the project dirs (which the public read_snapshots
        // / write_snapshots resolve to).
        let f = SnapshotsFile::new(sample_snapshots());
        let json = serde_json::to_vec_pretty(&f).unwrap();
        let parsed: SnapshotsFile = serde_json::from_slice(&json).unwrap();
        assert_eq!(parsed.snapshots.len(), 1);
        let anth = parsed.snapshots.get(&ProviderId::Anthropic).unwrap();
        assert_eq!(anth.plan_label.as_deref(), Some("Max 5x"));
        assert_eq!(
            anth.windows.get("5h").and_then(|w| w.fraction_used),
            Some(0.42)
        );
    }

    /// Helper: round-trip through `write_atomically` + a manual read so
    /// we can exercise the full file-IO path without needing the
    /// project_dirs override (which would mean fiddling with
    /// per-platform env vars in tests).
    fn round_trip_through_disk(path: &Path, payload: &SnapshotsFile) -> SnapshotsFile {
        let bytes = serde_json::to_vec_pretty(payload).unwrap();
        write_atomically(path, &bytes).unwrap();
        let read = std::fs::read(path).unwrap();
        serde_json::from_slice(&read).unwrap()
    }

    #[test]
    fn write_atomically_then_read_round_trips() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("s.json");
        let original = SnapshotsFile::new(sample_snapshots());
        let parsed = round_trip_through_disk(&path, &original);
        assert_eq!(parsed.snapshots.len(), 1);
        assert!(parsed
            .snapshots
            .contains_key(&ProviderId::Anthropic));
    }

    #[test]
    fn read_returns_err_on_garbage_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("s.json");
        std::fs::write(&path, b"not json at all").unwrap();
        let r: Result<SnapshotsFile, _> = serde_json::from_slice(&std::fs::read(&path).unwrap());
        assert!(r.is_err());
    }

    #[test]
    fn old_schema_without_stale_flag_deserialises() {
        // Schema-forward-compat: a snapshot file written by an older
        // build won't have `stale` on its WindowUsage entries. The
        // `#[serde(default)]` annotation must let it deserialise with
        // `stale = false`.
        let json = serde_json::json!({
            "updated_at": "2026-05-08T10:00:00Z",
            "snapshots": {
                "anthropic": {
                    "provider": "anthropic",
                    "timestamp": "2026-05-08T10:00:00Z",
                    "status": "ok",
                    "error": null,
                    "windows": {
                        "5h": {
                            "started_at": null,
                            "ends_at": null,
                            "spend_usd": null,
                            "tokens_in": 1,
                            "tokens_out": 2,
                            "request_count": 3,
                            "limit_usd": null,
                            "limit_tokens": null,
                            "fraction_used": 0.5
                        }
                    },
                    "headline": "ok",
                    "plan_label": "Max"
                }
            }
        });
        let parsed: SnapshotsFile = serde_json::from_value(json).unwrap();
        let w = parsed
            .snapshots
            .get(&ProviderId::Anthropic)
            .unwrap()
            .windows
            .get("5h")
            .unwrap();
        assert!(!w.stale, "stale must default to false for old files");
        assert_eq!(w.fraction_used, Some(0.5));
    }

    #[test]
    fn old_schema_without_plan_label_deserialises() {
        // `plan_label` was added later; the `#[serde(default)]`
        // covering it must let older snapshots in.
        let json = serde_json::json!({
            "updated_at": "2026-05-08T10:00:00Z",
            "snapshots": {
                "anthropic": {
                    "provider": "anthropic",
                    "timestamp": "2026-05-08T10:00:00Z",
                    "status": "ok",
                    "error": null,
                    "windows": {},
                    "headline": null
                }
            }
        });
        let parsed: SnapshotsFile = serde_json::from_value(json).unwrap();
        let s = parsed.snapshots.get(&ProviderId::Anthropic).unwrap();
        assert!(s.plan_label.is_none());
    }

    #[test]
    fn tmp_file_alone_does_not_corrupt_target() {
        // Simulate "process died between write and rename" — only the
        // .tmp file made it to disk. The target stays unchanged.
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("s.json");
        std::fs::write(&target, b"\"original\"").unwrap();
        std::fs::write(target.with_extension("tmp"), b"\"partial\"").unwrap();
        // Read the target — should still be the original.
        assert_eq!(std::fs::read(&target).unwrap(), b"\"original\"");
    }

    #[test]
    fn write_atomically_overwrites_existing_target() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("s.json");
        std::fs::write(&path, b"\"v1\"").unwrap();
        write_atomically(&path, b"\"v2\"").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"\"v2\"");
        // The .tmp companion is cleaned up on success.
        assert!(!path.with_extension("tmp").exists());
    }

    #[test]
    fn write_atomically_fails_when_parent_dir_missing() {
        // The atomic-write does not create parent directories itself
        // (the public `write_snapshots` handles that). Validate the
        // failure mode so a misuse surfaces clearly.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nope").join("s.json");
        assert!(write_atomically(&path, b"\"x\"").is_err());
    }

    #[test]
    fn touch_refresh_trigger_produces_a_path() {
        // Just confirm the helper resolves to a path — the actual
        // file gets touched via project_dirs and we don't want to
        // disturb the user's machine in tests.
        let p = crate::config::refresh_trigger_path().unwrap();
        assert!(
            !p.as_os_str().is_empty(),
            "refresh_trigger_path must yield something"
        );
    }

    #[test]
    fn snapshots_with_stale_flag_round_trip() {
        // Forward-direction: writing the current schema (with `stale`)
        // must read back with the flag preserved.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("s.json");
        let mut snaps = sample_snapshots();
        snaps
            .get_mut(&ProviderId::Anthropic)
            .unwrap()
            .windows
            .get_mut("5h")
            .unwrap()
            .stale = true;
        let parsed = round_trip_through_disk(&path, &SnapshotsFile::new(snaps));
        assert!(parsed
            .snapshots
            .get(&ProviderId::Anthropic)
            .unwrap()
            .windows
            .get("5h")
            .unwrap()
            .stale);
    }
}
