//! Process-wide handle cache for per-host writers. Mirrors `HzrStore`'s
//! interface so swapping the production wiring from `.hzr` to `.hzc` is a
//! local change inside the probe scheduler.

use std::{
    fs::File,
    path::{Path, PathBuf},
    sync::Arc,
};

use dashmap::DashMap;
use fs2::FileExt;
use uuid::Uuid;

use super::writer::{HostWriter, HzcError};

const STORE_LOCK_FILE: &str = "hzc.lock";

pub struct HzcStore {
    data_dir: PathBuf,
    handles: DashMap<Uuid, Arc<HostWriter>>,
    /// Held for the lifetime of the store - prevents two daemons from
    /// stomping on each other's chunks even before any per-host lock kicks
    /// in.
    #[allow(dead_code)]
    lock: File,
}

impl HzcStore {
    pub fn new(data_dir: impl Into<PathBuf>) -> Result<Self, HzcError> {
        let data_dir: PathBuf = data_dir.into();
        std::fs::create_dir_all(&data_dir)?;
        let lock_path = data_dir.join(STORE_LOCK_FILE);
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;
        lock.try_lock_exclusive()
            .map_err(|_| HzcError::LockHeld(lock_path))?;
        Ok(Self {
            data_dir,
            handles: DashMap::new(),
            lock,
        })
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Get or create the writer for `host_uuid`. Initial `base_interval_secs`
    /// and `chunk_window_secs` are only used the first time the host is
    /// opened - subsequent calls return the existing handle even if these
    /// values differ.
    pub fn writer(
        &self,
        host_uuid: Uuid,
        base_interval_secs: u32,
        chunk_window_secs: u32,
    ) -> Result<Arc<HostWriter>, HzcError> {
        if let Some(h) = self.handles.get(&host_uuid) {
            return Ok(h.clone());
        }
        let w = HostWriter::open(
            &self.data_dir,
            host_uuid,
            base_interval_secs,
            chunk_window_secs,
        )?;
        let arc = Arc::new(w);
        self.handles.insert(host_uuid, arc.clone());
        Ok(arc)
    }

    /// Drop the in-memory handle. The host's files stay on disk; next call
    /// to `writer()` re-opens.
    pub fn close(&self, host_uuid: Uuid) {
        if let Some((_, h)) = self.handles.remove(&host_uuid) {
            let _ = h.flush();
        }
    }

    /// Remove a host's data directory entirely. Use when deleting a host
    /// from the system.
    pub fn delete(&self, host_uuid: Uuid) -> Result<(), HzcError> {
        if let Some((_, h)) = self.handles.remove(&host_uuid) {
            let _ = h.flush();
        }
        let host_dir = super::writer::host_directory(&self.data_dir, host_uuid);
        if host_dir.exists() {
            std::fs::remove_dir_all(&host_dir)?;
        }
        Ok(())
    }

    /// Iterate every host currently materialised under `data/hzc/`. Used by
    /// the compactor to walk the world; returns UUIDs parsed from directory
    /// names.
    pub fn list_hosts(&self) -> Result<Vec<Uuid>, HzcError> {
        let base = self.data_dir.join("hzc");
        if !base.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for shard in std::fs::read_dir(&base)? {
            let shard = shard?;
            if !shard.file_type()?.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(shard.path())? {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                if let Some(name) = entry.file_name().to_str() {
                    if let Ok(uuid) = Uuid::parse_str(name) {
                        out.push(uuid);
                    }
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slot::Slot;
    use tempfile::TempDir;

    #[test]
    fn round_trip_via_store() {
        let dir = TempDir::new().unwrap();
        let store = HzcStore::new(dir.path()).unwrap();
        let uuid = Uuid::new_v4();
        let w = store.writer(uuid, 30, 60).unwrap();
        w.write_sample(
            10,
            Slot {
                min: 1.0,
                p2_5: 1.0,
                p25: 1.0,
                median: 1.0,
                p75: 1.0,
                p97_5: 1.0,
                loss_pct: 0.0,
            },
        )
        .unwrap();
        w.flush().unwrap();
    }

    #[test]
    fn second_store_fails_lock() {
        let dir = TempDir::new().unwrap();
        let _s = HzcStore::new(dir.path()).unwrap();
        let r = HzcStore::new(dir.path());
        assert!(matches!(r, Err(HzcError::LockHeld(_))));
    }

    #[test]
    fn list_hosts_finds_materialised_dirs() {
        let dir = TempDir::new().unwrap();
        let store = HzcStore::new(dir.path()).unwrap();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let _ = store.writer(a, 30, 60).unwrap();
        let _ = store.writer(b, 30, 60).unwrap();
        let mut found = store.list_hosts().unwrap();
        found.sort();
        let mut want = vec![a, b];
        want.sort();
        assert_eq!(found, want);
    }
}
