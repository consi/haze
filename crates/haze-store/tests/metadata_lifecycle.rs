//! Regression and measured performance coverage for historical metadata.
use haze_store::{MetadataRecord, MetadataStore};
use serde_json::json;
use std::{fs, time::Instant};
use uuid::Uuid;

fn trace(host: Uuid, ts: i64, variant: u32) -> MetadataRecord {
    let hops:Vec<_>=(1..=16).map(|hop|json!([{"ip":format!("10.{variant}.{hop}.1"),"dns":format!("router-{hop}.example.net")}])).collect();
    let metrics: Vec<_> = (1..=16)
        .map(|hop| json!({"sent":5,"received":5,"avg_ms":f64::from(hop)+0.5,"loss_pct":0.0}))
        .collect();
    MetadataRecord::new(
        host,
        ts,
        "trace",
        json!({"target":"1.1.1.1","hops":hops}),
        json!({"event":if variant==0 {""} else {"route_changed"},"hops":metrics,"reached":true}),
    )
}
fn size(path: &std::path::Path) -> u64 {
    fs::read_dir(path)
        .unwrap()
        .map(|e| {
            let p = e.unwrap().path();
            if p.is_dir() {
                size(&p)
            } else {
                fs::metadata(p).unwrap().len()
            }
        })
        .sum()
}
#[test]
fn compaction_replication_and_retention_preserve_records() {
    let dir = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let host = Uuid::new_v4();
    let store = MetadataStore::new(dir.path().into());
    let dest = MetadataStore::new(destination.path().into());
    let mut ids = Vec::new();
    for i in 0..300 {
        let r = trace(host, 1000 + i * 60, 0);
        ids.push(r.id);
        store.append_local(r, 600).unwrap();
    }
    store.maintain(host, 0).unwrap();
    assert_eq!(store.range(host, 0, 30000).unwrap().len(), 300);
    let mut cursor = 0;
    loop {
        let page = store.page(host, cursor, 37).unwrap();
        if page.records.is_empty() {
            break;
        }
        for r in &page.records {
            assert!(dest.append(r.clone(), 600).unwrap());
            assert!(!dest.append(r.clone(), 600).unwrap());
        }
        cursor = page.records.last().unwrap().sequence;
        if page.next.is_none() {
            break;
        }
    }
    dest.flush(host).unwrap();
    drop(dest);
    let dest = MetadataStore::new(destination.path().into());
    assert_eq!(
        dest.range(host, 0, 30000)
            .unwrap()
            .iter()
            .map(|r| r.id)
            .collect::<Vec<_>>(),
        ids
    );
    let predecessor = dest
        .predecessor(host, 7000, Uuid::nil(), "trace")
        .unwrap()
        .unwrap();
    assert_eq!(predecessor.timestamp, 6940);
    dest.maintain(host, 15000).unwrap();
    assert!(
        dest.predecessor(host, 15000, Uuid::nil(), "trace")
            .unwrap()
            .is_some()
    );
}
#[test]
fn checkpoints_flush_and_future_formats_are_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let host = Uuid::new_v4();
    let store = MetadataStore::new(dir.path().into());
    store
        .checkpoint(host, "schedule", &json!({"count":7}))
        .unwrap();
    assert_eq!(store.read_checkpoint(host, "schedule").unwrap()["count"], 7);
    store.flush_all().unwrap();
    drop(store);
    let store = MetadataStore::new(dir.path().into());
    assert_eq!(store.read_checkpoint(host, "schedule").unwrap()["count"], 7);
    let mut r = trace(host, 100, 0);
    r.version = 99;
    assert!(store.append(r, 60).is_err());
    assert!(store.page(host, 0, 100).unwrap().records.is_empty());
}
#[test]
#[ignore = "Measured workload; run explicitly with --ignored --nocapture"]
fn benchmark_metadata_thousand_hosts() {
    let dir = tempfile::tempdir().unwrap();
    let store = MetadataStore::new(dir.path().into());
    let hosts: Vec<_> = (0..1000).map(|_| Uuid::new_v4()).collect();
    let start = Instant::now();
    for host in &hosts {
        for i in 0..10 {
            store
                .append_local(trace(*host, 1000 + i * 300, 0), 3600)
                .unwrap();
            store
                .checkpoint(*host, "schedule", &json!({"count":i}))
                .unwrap();
        }
    }
    let elapsed = start.elapsed();
    let flush = Instant::now();
    store.flush_all().unwrap();
    eprintln!(
        "1000 hosts, 10000 observations: append={elapsed:?}, batch flush={:?}, bytes={}",
        flush.elapsed(),
        size(dir.path())
    );
    for changing in [false, true] {
        let d = tempfile::tempdir().unwrap();
        let s = MetadataStore::new(d.path().into());
        let host = Uuid::new_v4();
        let mut raw = Vec::new();
        for i in 0..1440 {
            let r = trace(
                host,
                86400 + i * 60,
                if changing { (i % 16) as u32 } else { 0 },
            );
            serde_json::to_writer(&mut raw, &r).unwrap();
            raw.push(b'\n');
            s.append_local(r, 3600).unwrap();
        }
        s.maintain(host, 0).unwrap();
        s.flush(host).unwrap();
        let stored = size(d.path());
        let repeated_zstd = zstd::encode_all(raw.as_slice(), 3).unwrap().len();
        let start = Instant::now();
        let index = s.index(host, 86400, 172_800).unwrap();
        let query = start.elapsed();
        eprintln!(
            "changing={changing}: JSON={} repeated-JSON-zstd={repeated_zstd} indexed-metadata={stored}, index={query:?} for {} records",
            raw.len(),
            index.len()
        );
        assert!(
            stored < raw.len() as u64 / 3,
            "metadata should substantially reduce repeated snapshots"
        );
    }
}
