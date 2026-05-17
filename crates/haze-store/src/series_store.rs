//! In-memory per-host slot ring buffer the alert evaluator reads.
//!
//! Probes append a `(timestamp_secs, Slot)` after a successful HZC write,
//! the alert engine slices the buffer for any rule's window when it
//! evaluates. Reads happen far more often than writes (one append per host
//! per period vs. fan-out across the whole rule set on every eval tick), so
//! the buffer lives behind a `parking_lot::RwLock` for cheap shared
//! locking.
//!
//! Trimming is lazy: every append drops samples whose `ts` is older than
//! `now - max_age_secs`. `max_age_secs` is reset by the alert engine each
//! cycle to the longest active rule window plus a safety margin, so the
//! footprint shrinks when rules are deleted and grows when long windows
//! are added without ever drifting on stale state.

use std::{
    collections::VecDeque,
    sync::atomic::{AtomicI64, Ordering},
};

use dashmap::DashMap;
use parking_lot::RwLock;
use uuid::Uuid;

use crate::slot::Slot;

/// Lowest cap the trimmer will honour. Even if no rules are configured we
/// still want a minute of headroom so probes have somewhere to land before
/// the first rule arrives.
const MIN_MAX_AGE_SECS: i64 = 60;

#[derive(Debug)]
pub struct SeriesStore {
    inner: DashMap<Uuid, RwLock<VecDeque<(i64, Slot)>>>,
    /// Largest window across active rules + safety factor. Updated by the
    /// alert engine; appends use this to drop stale samples.
    max_age_secs: AtomicI64,
}

impl SeriesStore {
    pub fn new() -> Self {
        Self {
            inner: DashMap::new(),
            max_age_secs: AtomicI64::new(MIN_MAX_AGE_SECS * 60), // 1 h default
        }
    }

    /// Cap on the in-memory age, in seconds. The alert engine resets this
    /// every cycle to `max(window) + grace`; the trimmer enforces it on
    /// the next append.
    pub fn set_max_age_secs(&self, secs: i64) {
        let clamped = secs.max(MIN_MAX_AGE_SECS);
        self.max_age_secs.store(clamped, Ordering::Relaxed);
    }

    pub fn max_age_secs(&self) -> i64 {
        self.max_age_secs.load(Ordering::Relaxed)
    }

    /// Push a new sample for `host_uuid`. Drops anything older than
    /// `max_age_secs` before insertion.
    pub fn append(&self, host_uuid: Uuid, ts: i64, slot: Slot) {
        let cutoff = ts - self.max_age_secs.load(Ordering::Relaxed);
        let entry = self
            .inner
            .entry(host_uuid)
            .or_insert_with(|| RwLock::new(VecDeque::with_capacity(64)));
        let mut buf = entry.write();
        while let Some(&(front_ts, _)) = buf.front() {
            if front_ts < cutoff {
                buf.pop_front();
            } else {
                break;
            }
        }
        buf.push_back((ts, slot));
    }

    /// Return every sample with `from <= ts <= to`. Inclusive bounds; the
    /// evaluator already pads its query window a little so the inclusivity
    /// is harmless.
    pub fn slice(&self, host_uuid: Uuid, from: i64, to: i64) -> Vec<(i64, Slot)> {
        let Some(entry) = self.inner.get(&host_uuid) else {
            return Vec::new();
        };
        let buf = entry.read();
        buf.iter()
            .filter(|(ts, _)| *ts >= from && *ts <= to)
            .copied()
            .collect()
    }

    /// Timestamp of the most recently appended sample. None if the host
    /// has no entries.
    pub fn newest_ts(&self, host_uuid: Uuid) -> Option<i64> {
        let entry = self.inner.get(&host_uuid)?;
        let buf = entry.read();
        buf.back().map(|(ts, _)| *ts)
    }

    /// Drop every sample for `host_uuid` (e.g. when the host is deleted).
    pub fn forget(&self, host_uuid: Uuid) {
        self.inner.remove(&host_uuid);
    }

    /// Snapshot of every host's current buffer. Used by the periodic flush
    /// task; the closure receives `(uuid, samples)` and decides what to
    /// persist (e.g. skip hosts whose `newest_ts` hasn't advanced).
    pub fn for_each_snapshot(&self, mut f: impl FnMut(Uuid, &[(i64, Slot)])) {
        for entry in &self.inner {
            let buf = entry.value().read();
            let view: Vec<(i64, Slot)> = buf.iter().copied().collect();
            f(*entry.key(), &view);
        }
    }

    /// Replace whatever is in memory for `host_uuid` with `samples`. Used
    /// at startup to rehydrate from a snapshot. Caller is responsible for
    /// the staleness check (don't rehydrate if `newest_ts` is older than the
    /// engine's window).
    pub fn rehydrate(&self, host_uuid: Uuid, samples: Vec<(i64, Slot)>) {
        let entry = self
            .inner
            .entry(host_uuid)
            .or_insert_with(|| RwLock::new(VecDeque::with_capacity(samples.len())));
        let mut buf = entry.write();
        buf.clear();
        buf.extend(samples);
    }

    pub fn host_count(&self) -> usize {
        self.inner.len()
    }
}

impl Default for SeriesStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(median: f32) -> Slot {
        Slot {
            min: median - 0.5,
            p2_5: median - 0.25,
            p25: median - 0.1,
            median,
            p75: median + 0.1,
            p97_5: median + 0.25,
            loss_pct: 0.0,
        }
    }

    #[test]
    fn append_then_slice_returns_inclusive_window() {
        let s = SeriesStore::new();
        let u = Uuid::new_v4();
        for i in 0..10 {
            s.append(u, i * 60, slot(10.0 + i as f32));
        }
        let got = s.slice(u, 120, 360);
        assert_eq!(got.len(), 5);
        assert_eq!(got.first().map(|(ts, _)| *ts), Some(120));
        assert_eq!(got.last().map(|(ts, _)| *ts), Some(360));
    }

    #[test]
    fn trim_drops_old_samples_on_append() {
        let s = SeriesStore::new();
        s.set_max_age_secs(120);
        let u = Uuid::new_v4();
        s.append(u, 100, slot(1.0));
        s.append(u, 200, slot(2.0));
        s.append(u, 300, slot(3.0));
        // 100 is now 200 s before the most recent append; cutoff = 300 - 120 = 180.
        let got = s.slice(u, 0, 10_000);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, 200);
    }

    #[test]
    fn newest_ts_tracks_back() {
        let s = SeriesStore::new();
        let u = Uuid::new_v4();
        assert!(s.newest_ts(u).is_none());
        s.append(u, 50, slot(1.0));
        s.append(u, 80, slot(1.0));
        assert_eq!(s.newest_ts(u), Some(80));
    }

    #[test]
    fn rehydrate_replaces_contents() {
        let s = SeriesStore::new();
        let u = Uuid::new_v4();
        s.append(u, 1, slot(1.0));
        s.append(u, 2, slot(2.0));
        s.rehydrate(u, vec![(10, slot(9.0)), (20, slot(8.0))]);
        let got = s.slice(u, 0, 1000);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, 10);
        assert_eq!(got[1].0, 20);
    }
}
