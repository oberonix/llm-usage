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
