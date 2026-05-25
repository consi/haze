// Worker code is full of pragmatic patterns (many arguments, closures,
// match arms returning the same thing). Pedantic noise is silenced at the
// module level instead of fighting the shape at every site.
#![allow(
    clippy::needless_pass_by_value,
    clippy::too_many_arguments,
    clippy::implicit_clone,
    clippy::map_unwrap_or,
    clippy::manual_let_else,
    clippy::single_match_else,
    clippy::collapsible_if,
    clippy::items_after_statements,
    clippy::used_underscore_binding,
    clippy::too_long_first_doc_paragraph,
    clippy::doc_markdown,
    clippy::redundant_closure_for_method_calls,
    clippy::use_self,
    clippy::cast_possible_wrap
)]

//! Replication worker: pulls samples from configured source instances
//! and lands them in the local HZC store. One supervisor task plus one
//! task per enabled rule. Bounded by the `replication` worker pool.
//!
//! Design overview:
//!
//! - **Supervisor** ([`run_manager`]) re-reads `replication_rules` every 5 s
//!   and starts a worker for every newly-enabled rule, cancels workers for
//!   removed / disabled rules. Re-uses the existing `enabled` flag for
//!   pause/resume so admins can pause without losing the rule.
//!
//! - **Worker** ([`run_rule`]) goes through a small state machine:
//!   pair → catch-up → stream. Catch-up runs the manifest + range pulls
//!   below `reconcile_interval_secs` apart. Stream is SSE; on disconnect
//!   we back off exponentially and re-enter catch-up.
//!
//! - **Logging**: every state transition is an `info!` with the rule UUID
//!   so the admin can `grep rule_uuid=...` to follow one rule end-to-end.
//!   Sample-level events are `debug!` to keep the info channel skimmable.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use anyhow::{Result, anyhow};
use dashmap::DashMap;
use haze_api::{ReplicationPool, replication_routes::wire};
use haze_store::{
    HzcStore, Slot,
    repo::{
        groups,
        hosts::{self, GroupFilter, NewHost},
        replication::{self, PeerPatch},
    },
};
use sqlx::SqlitePool;
use tokio::{
    sync::{Notify, broadcast, oneshot},
    task::JoinHandle,
    time::Instant,
};
use uuid::Uuid;

/// Cancellation handle for a running per-rule worker. The `JoinHandle` is
/// retained so the supervisor can extend the API later (e.g. graceful
/// shutdown await); right now we cancel via the notify and let the task
/// drop naturally.
struct WorkerSlot {
    #[allow(dead_code)]
    handle: JoinHandle<()>,
    cancel: Arc<Notify>,
}

/// Spawn the supervisor that owns per-rule worker tasks. Returns
/// immediately; the supervisor itself runs forever (or until the
/// process exits). Errors from individual workers are logged and don't
/// take down the supervisor.
/// Per-host high-water mark of the most recent timestamp written by ANY
/// replication worker on this instance. Multi-path topologies (where a
/// host arrives via 2+ peers) consult this map to dedup: if a worker is
/// about to write `(host, ts)` but the map already has `>= ts` for that
/// host, the write is a duplicate of an earlier path's delivery and is
/// dropped. Without this, downstreams accumulate N copies of every
/// sample where N is the path count.
pub type WriteCursors = Arc<DashMap<Uuid, i64>>;

pub fn run_manager(
    pool: SqlitePool,
    hzc: Arc<HzcStore>,
    data_dir: std::path::PathBuf,
    instance_uuid: Uuid,
    replication_pool: ReplicationPool,
    events: broadcast::Sender<haze_api::events_routes::ChangeKind>,
    samples: broadcast::Sender<haze_store::SampleEvent>,
) {
    let workers: Arc<DashMap<i64, WorkerSlot>> = Arc::new(DashMap::new());
    let write_cursors: WriteCursors = Arc::new(DashMap::new());
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        tick.tick().await;
        loop {
            tick.tick().await;
            if let Err(e) = manager_step(
                &pool,
                &hzc,
                &data_dir,
                instance_uuid,
                &replication_pool,
                &events,
                &samples,
                &workers,
                &write_cursors,
            )
            .await
            {
                tracing::warn!(error = %e, "replication manager step failed");
            }
        }
    });
}

/// Conditionally update the dedup cursor: returns true iff `ts` strictly
/// exceeds the previous high-water mark and so the caller should proceed
/// with the write. Uses DashMap's Entry API for cheap per-key locking.
///
/// Delay tradeoff: this is a strictly-monotonic high-water-mark. If one
/// replication path runs minutes behind another but each delivers
/// samples in order, the dedup still correctly admits the leading
/// path's writes and drops only the trailing path's already-superseded
/// duplicates. It does NOT backfill historical gaps left by a faster
/// path that arrived out-of-order; for that we'd need per-host
/// range-set tracking, which is heavier and only matters when paths
/// deliver samples non-monotonically (which haze probes never do -
/// the probe scheduler always writes in increasing ts).
fn dedup_admit(cursors: &WriteCursors, host_uuid: Uuid, ts: i64) -> bool {
    use dashmap::mapref::entry::Entry;
    match cursors.entry(host_uuid) {
        Entry::Occupied(mut o) => {
            if *o.get() >= ts {
                false
            } else {
                *o.get_mut() = ts;
                true
            }
        }
        Entry::Vacant(v) => {
            v.insert(ts);
            true
        }
    }
}

async fn manager_step(
    pool: &SqlitePool,
    hzc: &Arc<HzcStore>,
    data_dir: &std::path::Path,
    instance_uuid: Uuid,
    replication_pool: &ReplicationPool,
    events: &broadcast::Sender<haze_api::events_routes::ChangeKind>,
    samples: &broadcast::Sender<haze_store::SampleEvent>,
    workers: &Arc<DashMap<i64, WorkerSlot>>,
    write_cursors: &WriteCursors,
) -> Result<()> {
    let enabled = replication::list_enabled_rules(pool).await?;
    let enabled_ids: HashSet<i64> = enabled.iter().map(|r| r.id).collect();

    // Stop workers whose rules disappeared / disabled.
    let stale: Vec<i64> = workers
        .iter()
        .filter(|kv| !enabled_ids.contains(kv.key()))
        .map(|kv| *kv.key())
        .collect();
    for id in stale {
        if let Some((_, slot)) = workers.remove(&id) {
            tracing::info!(
                rule_id = id,
                "stopping replication worker (rule disabled or removed)"
            );
            slot.cancel.notify_waiters();
        }
    }

    // Start workers for new rules.
    for rule in enabled {
        if workers.contains_key(&rule.id) {
            continue;
        }
        let Some(peer) = replication::get_peer_by_id(pool, rule.peer_id).await? else {
            tracing::warn!(rule_uuid = %rule.uuid, peer_id = rule.peer_id,
                "rule references missing peer; skipping");
            continue;
        };
        let cancel = Arc::new(Notify::new());
        let cancel_clone = cancel.clone();
        let pool = pool.clone();
        let hzc = hzc.clone();
        let data_dir = data_dir.to_path_buf();
        let events = events.clone();
        let samples = samples.clone();
        let pool_sem = replication_pool.clone();
        let write_cursors = write_cursors.clone();
        let rule_for_task = rule.clone();
        let peer_for_task = peer.clone();
        let handle = tokio::spawn(async move {
            tracing::info!(
                rule_uuid = %rule_for_task.uuid,
                peer_uuid = %peer_for_task.uuid,
                peer_name = %peer_for_task.name,
                "replication worker starting"
            );
            if let Err(e) = run_rule(
                pool,
                hzc,
                data_dir,
                instance_uuid,
                rule_for_task.clone(),
                peer_for_task.clone(),
                pool_sem,
                events,
                samples,
                write_cursors,
                cancel_clone,
            )
            .await
            {
                tracing::error!(
                    rule_uuid = %rule_for_task.uuid,
                    peer_name = %peer_for_task.name,
                    error = ?e,
                    "replication worker exited with error"
                );
            } else {
                tracing::info!(
                    rule_uuid = %rule_for_task.uuid,
                    "replication worker stopped cleanly"
                );
            }
        });
        workers.insert(rule.id, WorkerSlot { handle, cancel });
    }
    Ok(())
}

/// Run a single rule end-to-end. Returns when cancelled or on fatal error.
///
/// Permit semantics: a `replication` worker-pool permit is held only during
/// the *catch-up phase* (manifest + range fetches), which is the I/O-heavy
/// burst. The persistent SSE stream that follows does NOT hold a permit -
/// it's a single idle async receiver, so a pool of 16 happily supervises
/// hundreds of streaming rules; only the periodic catch-up bursts get
/// serialised through the permits. If more rules than permits all hit
/// catch-up at once (e.g. mass reconnect after a network blip), the extras
/// queue cleanly on `acquire_owned()` and run in FIFO order - none are
/// starved, just serialised. Streaming continues for everyone throughout.
async fn run_rule(
    pool: SqlitePool,
    hzc: Arc<HzcStore>,
    data_dir: std::path::PathBuf,
    instance_uuid: Uuid,
    rule: replication::ReplicationRule,
    mut peer: replication::ReplicationPeer,
    pool_sem: ReplicationPool,
    events: broadcast::Sender<haze_api::events_routes::ChangeKind>,
    samples: broadcast::Sender<haze_store::SampleEvent>,
    write_cursors: WriteCursors,
    cancel: Arc<Notify>,
) -> Result<()> {
    let mut backoff = Duration::from_secs(5);
    // Error hysteresis: SSE streams over HTTP/1.1 chunked encoding flap
    // for benign reasons (server-side `tokio::select` cancellation when
    // we trigger a manifest reconcile, transient body-decode errors,
    // upstream restarts). Without hysteresis the UI status pin-pongs
    // between "active" and "error" every few seconds even though the
    // worker recovers instantly. We only surface the error to operators
    // once it's been failing for `ERROR_THRESHOLD` consecutive attempts;
    // before that the worker is silently backing off + retrying.
    const ERROR_THRESHOLD: u32 = 3;
    let mut consecutive_errors: u32 = 0;
    loop {
        let attempt = run_rule_attempt(
            &pool,
            &hzc,
            &data_dir,
            instance_uuid,
            &rule,
            &peer,
            &pool_sem,
            &events,
            &samples,
            &write_cursors,
            &cancel,
        )
        .await;

        match attempt {
            Ok(AttemptOutcome::Cancelled) => return Ok(()),
            Ok(AttemptOutcome::Reconnect) => {
                backoff = Duration::from_secs(5);
                if consecutive_errors > 0 {
                    consecutive_errors = 0;
                    // Clear any error we previously surfaced so the UI
                    // flips back to active immediately on a clean
                    // reconnect.
                    let _ = replication::update_peer(
                        &pool,
                        peer.uuid,
                        PeerPatch {
                            last_error: Some(None),
                            ..Default::default()
                        },
                    )
                    .await;
                    let _ = events.send(haze_api::events_routes::ChangeKind::Replication);
                }
            }
            Err(e) => {
                consecutive_errors = consecutive_errors.saturating_add(1);
                if consecutive_errors >= ERROR_THRESHOLD {
                    tracing::warn!(
                        rule_uuid = %rule.uuid,
                        peer_name = %peer.name,
                        error = %e,
                        attempts = consecutive_errors,
                        backoff_ms = backoff.as_millis() as u64,
                        "replication attempt failed (past threshold); surfacing error in UI"
                    );
                    let _ = replication::update_peer(
                        &pool,
                        peer.uuid,
                        PeerPatch {
                            last_error: Some(Some(e.to_string())),
                            ..Default::default()
                        },
                    )
                    .await;
                    let _ = events.send(haze_api::events_routes::ChangeKind::Replication);
                } else {
                    tracing::debug!(
                        rule_uuid = %rule.uuid,
                        error = %e,
                        attempts = consecutive_errors,
                        "transient replication error (below threshold, not surfaced)"
                    );
                }
                tokio::select! {
                    () = cancel.notified() => return Ok(()),
                    () = tokio::time::sleep(backoff) => {},
                }
                // Cap the backoff at 60 s so a destination resumes
                // quickly when the source recovers (admin unblocks a
                // slot, restarts the source, fixes the network). Five
                // minutes was too long: a destination that hit the cap
                // during a block storm would stay quiescent for a full
                // five minutes after the operator clicks Unblock.
                backoff = std::cmp::min(backoff * 2, Duration::from_secs(60));
                // Reload peer config in case admin rotated the token/url while we were sleeping.
                if let Some(refreshed) = replication::get_peer_by_uuid(&pool, peer.uuid).await? {
                    peer = refreshed;
                }
            }
        }
    }
}

enum AttemptOutcome {
    /// Cancelled cleanly (rule disabled or process shutting down).
    Cancelled,
    /// Worker should re-enter catch-up immediately (clean disconnect /
    /// non-fatal lag event). Backoff is reset.
    Reconnect,
}

async fn run_rule_attempt(
    pool: &SqlitePool,
    hzc: &Arc<HzcStore>,
    data_dir: &std::path::Path,
    instance_uuid: Uuid,
    rule: &replication::ReplicationRule,
    peer: &replication::ReplicationPeer,
    pool_sem: &ReplicationPool,
    events: &broadcast::Sender<haze_api::events_routes::ChangeKind>,
    samples: &broadcast::Sender<haze_store::SampleEvent>,
    write_cursors: &WriteCursors,
    cancel: &Arc<Notify>,
) -> Result<AttemptOutcome> {
    // ─── 1. Pair (refresh chain, verify token) ──────────────────────
    let pair_started = std::time::Instant::now();
    let (source_uuid, src_version, upstream_chain) =
        wire::fetch_instance_info(&peer.base_url, &peer.api_token, peer.tls_skip_verify)
            .await
            .map_err(|e| anyhow!("instance-info: {e}"))?;
    let pair_latency_ms = pair_started.elapsed().as_millis() as i64;
    let _ = replication::update_peer(
        pool,
        peer.uuid,
        PeerPatch {
            source_instance_uuid: Some(Some(source_uuid)),
            upstream_chain: Some(&upstream_chain),
            last_contact_at: Some(Some(chrono::Utc::now().timestamp())),
            last_error: Some(None),
            source_version: Some(Some(src_version.as_str())),
            last_latency_ms: Some(Some(pair_latency_ms)),
            ..Default::default()
        },
    )
    .await;
    // Tell any open settings pages that this peer's status just turned
    // healthy so the Status column flips from "error" / "paused" to
    // "active" without the user reloading.
    let _ = events.send(haze_api::events_routes::ChangeKind::Replication);
    if upstream_chain.contains(&instance_uuid) {
        return Err(anyhow!(
            "replication loop detected: source's upstream chain contains our instance uuid"
        ));
    }

    // ─── 2. Create / refresh slot on source ─────────────────────────
    let client = wire::http_client(peer.tls_skip_verify);
    let mut path = upstream_chain;
    if !path.contains(&instance_uuid) {
        path.push(instance_uuid);
    }
    let slot_resp = client
        .post(format!("{}/api/v1/replication/slots", peer.base_url))
        .bearer_auth(&peer.api_token)
        .header(
            haze_api::replication_routes::wire::path_header_name(),
            haze_api::replication_routes::render_path(&path),
        )
        .json(&wire::UpsertSlotReq {
            peer_instance_uuid: instance_uuid,
            peer_label: format!("haze-{}", instance_uuid.as_simple()),
            source_group_uuid: Some(rule.source_group_uuid),
            replication_path: path.clone(),
        })
        .send()
        .await?;
    if !slot_resp.status().is_success() {
        return Err(anyhow!("/slots returned {}", slot_resp.status()));
    }
    let slot_body: wire::UpsertSlotResp = slot_resp.json().await?;
    replication::set_rule_slot_uuid(pool, rule.id, slot_body.slot_uuid).await?;
    let slot_uuid = slot_body.slot_uuid;
    tracing::info!(
        rule_uuid = %rule.uuid,
        peer_name = %peer.name,
        %slot_uuid,
        "paired with source; entering catch-up"
    );

    // ─── 3. Catch-up loop: manifest + ranges ────────────────────────
    //
    // Hold a worker-pool permit for the duration of catch-up. This is the
    // I/O-heavy burst that we want to serialise when the box is busy;
    // once we drop into streaming below, the permit goes back to the pool
    // immediately so other rules can run catch-up while we sit on SSE.
    {
        let _permit =
            acquire_pool_permit(pool_sem, "initial-catch-up", rule.uuid, &peer.name).await?;
        catch_up(
            pool,
            hzc,
            data_dir,
            rule,
            peer,
            slot_uuid,
            &client,
            events,
            write_cursors,
            cancel,
        )
        .await?;
    }

    // ─── 4. Live SSE stream ─────────────────────────────────────────
    tracing::info!(
        rule_uuid = %rule.uuid,
        peer_name = %peer.name,
        %slot_uuid,
        "catch-up complete; opening live stream"
    );
    let stream_resp = client
        .get(format!(
            "{}/api/v1/replication/slots/{slot_uuid}/stream",
            peer.base_url
        ))
        .bearer_auth(&peer.api_token)
        .header("Accept", "text/event-stream")
        .header(
            haze_api::replication_routes::wire::path_header_name(),
            haze_api::replication_routes::render_path(&path),
        )
        .send()
        .await?;
    if !stream_resp.status().is_success() {
        return Err(anyhow!("/stream returned {}", stream_resp.status()));
    }
    let mut byte_stream = stream_resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();

    use futures::StreamExt;
    let reconcile_after = Duration::from_secs(peer.reconcile_interval_secs.max(30) as u64);
    let ack_interval = Duration::from_secs(30);
    let mut last_ack = Instant::now();
    let mut last_reconcile = Instant::now();
    let mut pending_acks: HashMap<Uuid, i64> = HashMap::new();
    // Background reconcile task handle. Kept here so we don't kick off a
    // second one before the first finishes - reconciles can be slow if the
    // source has a lot of hosts. The stream stays open while this runs.
    let mut bg_reconcile: Option<tokio::task::JoinHandle<Result<()>>> = None;
    let mut needs_reconcile = false;

    loop {
        // Reap a finished reconcile so we can start a new one if needed.
        if let Some(h) = &mut bg_reconcile {
            if h.is_finished() {
                let handle = bg_reconcile.take().unwrap();
                match handle.await {
                    Ok(Ok(())) => tracing::info!(
                        rule_uuid = %rule.uuid, "background reconcile complete"
                    ),
                    Ok(Err(e)) => tracing::warn!(
                        rule_uuid = %rule.uuid, error = %e, "background reconcile failed"
                    ),
                    Err(e) => tracing::warn!(
                        rule_uuid = %rule.uuid, error = %e, "background reconcile task joined err"
                    ),
                }
            }
        }
        // Start a background reconcile if conditions warrant. We only
        // launch one at a time and DO NOT close the SSE stream - the
        // stream keeps consuming sample events while the reconcile runs
        // in parallel, fetching a permit from the worker pool when it's
        // ready.
        if needs_reconcile && bg_reconcile.is_none() {
            needs_reconcile = false;
            last_reconcile = Instant::now();
            let pool_c = pool.clone();
            let hzc_c = hzc.clone();
            let data_dir_c = data_dir.to_path_buf();
            let rule_c = rule.clone();
            let peer_c = peer.clone();
            let client_c = client.clone();
            let events_c = events.clone();
            let cancel_c = cancel.clone();
            let write_cursors_c = write_cursors.clone();
            let sem = pool_sem.semaphore.clone();
            let rule_uuid = rule.uuid;
            let peer_name = peer.name.clone();
            let capacity = pool_sem.capacity;
            bg_reconcile = Some(tokio::spawn(async move {
                let _permit =
                    acquire_pool_permit_inner(sem, capacity, "bg-reconcile", rule_uuid, &peer_name)
                        .await?;
                catch_up(
                    &pool_c,
                    &hzc_c,
                    &data_dir_c,
                    &rule_c,
                    &peer_c,
                    slot_uuid,
                    &client_c,
                    &events_c,
                    &write_cursors_c,
                    &cancel_c,
                )
                .await
            }));
            tracing::info!(rule_uuid = %rule.uuid,
                "spawned background reconcile (stream stays open)");
        }

        tokio::select! {
            biased;
            () = cancel.notified() => {
                tracing::info!(rule_uuid = %rule.uuid, "replication worker cancellation received");
                if let Some(h) = bg_reconcile.take() { h.abort(); }
                return Ok(AttemptOutcome::Cancelled);
            }
            chunk = byte_stream.next() => {
                match chunk {
                    Some(Ok(bytes)) => {
                        buf.extend_from_slice(&bytes);
                        while let Some((event, rest)) = take_sse_event(&buf) {
                            buf = rest;
                            match handle_sse_event(
                                pool, hzc, data_dir, rule, peer, slot_uuid,
                                &event, &mut pending_acks, events, samples,
                                write_cursors, &client,
                            ).await {
                                Ok(true) => {}
                                Ok(false) => {
                                    // manifest-changed or sample for unknown
                                    // host: trigger a background reconcile.
                                    // Stream stays open; new hosts will land
                                    // before subsequent samples for them.
                                    tracing::info!(
                                        rule_uuid = %rule.uuid,
                                        "stream signalled reconcile needed; queueing bg catch-up"
                                    );
                                    needs_reconcile = true;
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        rule_uuid = %rule.uuid,
                                        error = %e,
                                        "failed to apply stream event"
                                    );
                                }
                            }
                        }
                    }
                    Some(Err(e)) => {
                        if let Some(h) = bg_reconcile.take() { h.abort(); }
                        return Err(anyhow!("stream read error: {e}"));
                    }
                    None => {
                        tracing::info!(rule_uuid = %rule.uuid, "source closed stream; will reconnect");
                        if let Some(h) = bg_reconcile.take() { h.abort(); }
                        return Ok(AttemptOutcome::Reconnect);
                    }
                }
            }
            () = tokio::time::sleep(Duration::from_secs(15)) => {
                if last_reconcile.elapsed() >= reconcile_after {
                    // Safety-net reconcile in case the source skipped a
                    // structural change event. The stream stays open and
                    // a background task handles the catch-up.
                    needs_reconcile = true;
                }
                if last_ack.elapsed() >= ack_interval && !pending_acks.is_empty() {
                    flush_acks(pool, &client, peer, slot_uuid, &mut pending_acks).await;
                    last_ack = Instant::now();
                    let _ = events.send(haze_api::events_routes::ChangeKind::Replication);
                }
            }
        }
    }
}

/// Pull the source's manifest, mirror groups + hosts locally (merging
/// same-named groups), then range-fetch each host's outstanding samples.
async fn catch_up(
    pool: &SqlitePool,
    hzc: &Arc<HzcStore>,
    data_dir: &std::path::Path,
    rule: &replication::ReplicationRule,
    peer: &replication::ReplicationPeer,
    slot_uuid: Uuid,
    client: &reqwest::Client,
    events: &broadcast::Sender<haze_api::events_routes::ChangeKind>,
    write_cursors: &WriteCursors,
    cancel: &Arc<Notify>,
) -> Result<()> {
    // Refresh the peer's instance-info each reconcile pass so the
    // locally-stored `upstream_chain` reflects whatever the source is
    // currently advertising. This lets the operator see updated
    // topology in the UI when the SOURCE itself starts replicating from
    // new instances after our initial pairing.
    let inst_started = std::time::Instant::now();
    if let Ok((src_uuid, ver, chain)) =
        wire::fetch_instance_info(&peer.base_url, &peer.api_token, peer.tls_skip_verify).await
    {
        let latency_ms = inst_started.elapsed().as_millis() as i64;
        let _ = replication::update_peer(
            pool,
            peer.uuid,
            replication::PeerPatch {
                source_instance_uuid: Some(Some(src_uuid)),
                upstream_chain: Some(&chain),
                last_contact_at: Some(Some(chrono::Utc::now().timestamp())),
                // Clear any stale error: if instance-info just worked,
                // the peer is healthy regardless of what a previous
                // transient hiccup logged. Without this the UI sticks
                // on "error" even while lag is ticking down.
                last_error: Some(None),
                source_version: Some(Some(ver.as_str())),
                last_latency_ms: Some(Some(latency_ms)),
                ..Default::default()
            },
        )
        .await;
        let _ = events.send(haze_api::events_routes::ChangeKind::Replication);
    }

    let manifest = client
        .get(format!(
            "{}/api/v1/replication/slots/{slot_uuid}/manifest",
            peer.base_url
        ))
        .bearer_auth(&peer.api_token)
        .send()
        .await?;
    if !manifest.status().is_success() {
        return Err(anyhow!("/manifest returned {}", manifest.status()));
    }
    let m: wire::ManifestResp = manifest.json().await?;
    tracing::info!(
        rule_uuid = %rule.uuid,
        peer_name = %peer.name,
        groups = m.groups.len(),
        hosts = m.hosts.len(),
        "manifest fetched"
    );

    // Materialise groups (with merge-by-name) and record source→local map.
    materialise_groups(pool, rule, peer.id, &m).await?;

    // Materialise hosts (preserve source uuid; remap group memberships).
    let mut current_uuids: HashSet<Uuid> = HashSet::new();
    for h in &m.hosts {
        current_uuids.insert(h.uuid);
        materialise_host(pool, rule, peer.id, h).await?;
    }
    // Hosts that fell out of the manifest get marked orphaned. We keep
    // their data and metadata, but stop pulling.
    let local_cursors = replication::list_cursors_for_rule(pool, rule.id).await?;
    for c in local_cursors {
        if !current_uuids.contains(&c.host_uuid) && c.orphaned_at.is_none() {
            replication::mark_cursor_orphaned(pool, rule.id, c.host_uuid).await?;
            tracing::info!(
                rule_uuid = %rule.uuid,
                host_uuid = %c.host_uuid,
                "host no longer in source manifest; marked orphaned locally"
            );
        }
    }

    let _ = events.send(haze_api::events_routes::ChangeKind::Tree);

    // Range-fetch each host from its cursor to "now" in batches.
    for h in &m.hosts {
        if cancel_pending(cancel) {
            return Ok(());
        }
        if let Err(e) = backfill_host(
            pool,
            hzc,
            data_dir,
            rule,
            peer,
            slot_uuid,
            h,
            client,
            write_cursors,
        )
        .await
        {
            tracing::warn!(
                rule_uuid = %rule.uuid,
                host_uuid = %h.uuid,
                error = %e,
                "host backfill failed; will retry next cycle"
            );
        }
    }
    Ok(())
}

fn cancel_pending(cancel: &Notify) -> bool {
    let (_tx, mut rx): (oneshot::Sender<()>, oneshot::Receiver<()>) = oneshot::channel();
    drop(_tx); // immediately Drop so rx resolves Err if not signalled
    // try_recv pattern: if cancel was signalled we want to stop. Notify
    // doesn't expose a non-async "was notified" check, so we instead try
    // to grab a notification with `now_or_never`.
    use futures::FutureExt;
    let mut fut = Box::pin(cancel.notified());
    matches!(fut.as_mut().now_or_never(), Some(())) || rx.try_recv().is_ok()
}

async fn materialise_groups(
    pool: &SqlitePool,
    rule: &replication::ReplicationRule,
    peer_id: i64,
    m: &wire::ManifestResp,
) -> Result<()> {
    // Local destination parent for the rule's mapping. Nil source means
    // "the rule's dest group" is the parent for every top-level source
    // group; otherwise the rule's source root becomes the dest group.
    let dest_root_id = if rule.dest_group_uuid.is_nil() {
        None
    } else {
        Some(
            groups::resolve_id(pool, rule.dest_group_uuid)
                .await?
                .ok_or_else(|| {
                    anyhow!(
                        "destination group {} no longer exists",
                        rule.dest_group_uuid
                    )
                })?,
        )
    };

    // Build source-id-by-uuid map for parent resolution and remember
    // source's tree structure.
    let by_uuid: HashMap<Uuid, &wire::ManifestGroup> =
        m.groups.iter().map(|g| (g.uuid, g)).collect();
    // Topo-sort by depth (parent before child). Source's manifest doesn't
    // ship depths, so derive by walking parents.
    let mut order: Vec<Uuid> = Vec::with_capacity(by_uuid.len());
    let mut placed: HashSet<Uuid> = HashSet::new();
    fn visit(
        u: Uuid,
        by_uuid: &HashMap<Uuid, &wire::ManifestGroup>,
        placed: &mut HashSet<Uuid>,
        order: &mut Vec<Uuid>,
    ) {
        if placed.contains(&u) {
            return;
        }
        if let Some(g) = by_uuid.get(&u) {
            if let Some(p) = g.parent_uuid {
                if by_uuid.contains_key(&p) {
                    visit(p, by_uuid, placed, order);
                }
            }
        }
        placed.insert(u);
        order.push(u);
    }
    for u in by_uuid.keys() {
        visit(*u, &by_uuid, &mut placed, &mut order);
    }

    for src_uuid in order {
        let Some(src) = by_uuid.get(&src_uuid) else {
            continue;
        };
        // Skip the rule's source root - it maps directly to the rule's dest.
        if src_uuid == rule.source_group_uuid {
            replication::put_group_mapping(pool, rule.id, src_uuid, rule.dest_group_uuid).await?;
            continue;
        }
        // Already mapped? Then nothing to do.
        if replication::get_group_mapping(pool, rule.id, src_uuid)
            .await?
            .is_some()
        {
            continue;
        }
        // Resolve local parent id from the mapped parent.
        let parent_local_id = match src.parent_uuid {
            None => dest_root_id,
            Some(p) => match replication::get_group_mapping(pool, rule.id, p).await? {
                Some(local) if local.is_nil() => None,
                Some(local) => groups::resolve_id(pool, local).await?,
                None => dest_root_id,
            },
        };
        // Merge-by-name with any same-named local sibling under that parent.
        if let Some(existing) =
            groups::find_sibling_by_name(pool, parent_local_id, &src.display_name).await?
        {
            replication::put_group_mapping(pool, rule.id, src_uuid, existing.uuid_typed()).await?;
            tracing::info!(
                rule_uuid = %rule.uuid,
                source_group_uuid = %src_uuid,
                local_group_uuid = %existing.uuid_typed(),
                name = %src.display_name,
                "merged source group into existing local group"
            );
            continue;
        }
        let g =
            groups::create_replicated(pool, parent_local_id, &src.display_name, peer_id).await?;
        replication::put_group_mapping(pool, rule.id, src_uuid, g.uuid_typed()).await?;
        tracing::info!(
            rule_uuid = %rule.uuid,
            source_group_uuid = %src_uuid,
            local_group_uuid = %g.uuid_typed(),
            name = %g.display_name,
            "created replicated group"
        );
    }
    Ok(())
}

async fn materialise_host(
    pool: &SqlitePool,
    rule: &replication::ReplicationRule,
    peer_id: i64,
    h: &wire::ManifestHost,
) -> Result<()> {
    // Translate source group memberships to local ones using the rule's map.
    let mut local_groups: Vec<Uuid> = Vec::new();
    for g in &h.group_uuids {
        match replication::get_group_mapping(pool, rule.id, *g).await? {
            Some(local) if !local.is_nil() => local_groups.push(local),
            _ => {}
        }
    }
    // If the rule maps a non-root source group and this host inherits
    // from it, ensure the rule's dest group is in the membership list.
    if !rule.source_group_uuid.is_nil()
        && h.group_uuids.contains(&rule.source_group_uuid)
        && !rule.dest_group_uuid.is_nil()
    {
        if !local_groups.contains(&rule.dest_group_uuid) {
            local_groups.push(rule.dest_group_uuid);
        }
    }

    if let Some(existing) = hosts::get_by_uuid(pool, h.uuid).await? {
        // Allow co-ownership across multiple replication peers: in
        // cascading topologies the same source host can reach this
        // instance through more than one path (e.g. host-1 arrives on
        // haze-8 via both 8<-6 and 8<-7 chains). The host stays tagged
        // with whichever peer materialised it first - that determines
        // which peer's deletion clears the row, and all peers agree on
        // the data because they all chain back to the same origin.
        // Only refuse if the local row is purely local (no peer at all)
        // - that's an operator error we don't want to silently clobber.
        if existing.replication_peer_id.is_none() {
            return Err(anyhow!(
                "local host {} exists as a locally-probed host; refusing to overwrite \
                 via replication. Delete or rename the local host first.",
                h.uuid
            ));
        }
        // Mirror everything that's mutable on the local row from the
        // source manifest: display_name, group memberships, AND the
        // sampling cadence (interval + samples-per-period). The HZC
        // `chunk_window_secs` is intentionally NOT patchable - it's baked
        // into the host's `meta.json` on disk and migrating sealed chunks
        // isn't supported. The local writer keeps using whatever was set
        // at create time; the source can change interval but we honour
        // the original chunk window for sealing.
        if existing.chunk_window_secs != h.chunk_window_secs {
            tracing::warn!(
                rule_uuid = %rule.id,
                host_uuid = %h.uuid,
                local_chunk_window = existing.chunk_window_secs,
                source_chunk_window = h.chunk_window_secs,
                "source changed chunk_window_secs after pairing; local writer keeps original \
                 (HZC meta.json is immutable). Delete and recreate the host locally to migrate."
            );
        }
        // Union new memberships from this rule into whatever the host
        // already has locally. Multiple rules can touch the same source
        // host (e.g. a root->root cascade + a Special->Renamed remap):
        // each rule adds the groups it maps to, never removes another
        // rule's contributions. Avoids the thrash that would happen if
        // we always REPLACED memberships with the current rule's view.
        let mut union_groups: Vec<Uuid> = existing.group_uuids.clone();
        for g in &local_groups {
            if !union_groups.contains(g) {
                union_groups.push(*g);
            }
        }
        let needs_group_update = union_groups.len() != existing.group_uuids.len();
        let needs_interval_update = existing.interval_secs != h.interval_secs;
        let needs_spp_update = existing.samples_per_period != h.samples_per_period;
        let _ = hosts::update_by_uuid(
            pool,
            h.uuid,
            hosts::HostPatch {
                display_name: Some(&h.display_name),
                group_uuids: needs_group_update.then_some(&union_groups[..]),
                interval_secs: needs_interval_update.then_some(h.interval_secs),
                samples_per_period: needs_spp_update.then_some(h.samples_per_period),
                ..Default::default()
            },
        )
        .await;
        if needs_interval_update || needs_spp_update {
            tracing::info!(
                rule_uuid = %rule.id,
                host_uuid = %h.uuid,
                new_interval_secs = h.interval_secs,
                new_samples_per_period = h.samples_per_period,
                "host sampling cadence updated from source"
            );
        }
    } else {
        let _ = hosts::create_replicated(
            pool,
            h.uuid,
            NewHost {
                display_name: &h.display_name,
                probe_type: &h.probe_type,
                // Replicated hosts have no probe config locally. Store an
                // empty JSON object so the column remains valid JSON and
                // the UI's "managed by replication" gating hides it.
                probe_config: "{}",
                interval_secs: h.interval_secs,
                samples_per_period: h.samples_per_period,
                chunk_window_secs: h.chunk_window_secs,
                group_uuids: &local_groups,
            },
            peer_id,
        )
        .await?;
        tracing::info!(
            rule_uuid = %rule.uuid,
            host_uuid = %h.uuid,
            name = %h.display_name,
            probe_type = %h.probe_type,
            "created replicated host"
        );
    }
    Ok(())
}

async fn backfill_host(
    pool: &SqlitePool,
    hzc: &Arc<HzcStore>,
    _data_dir: &std::path::Path,
    rule: &replication::ReplicationRule,
    peer: &replication::ReplicationPeer,
    slot_uuid: Uuid,
    h: &wire::ManifestHost,
    client: &reqwest::Client,
    write_cursors: &WriteCursors,
) -> Result<()> {
    let now = chrono::Utc::now().timestamp();
    let cursor = replication::get_cursor(pool, rule.id, h.uuid).await?;
    // Resume strictly AFTER the highest timestamp we've already written -
    // both the SSE stream and this catch-up pass can run concurrently, so
    // re-fetching the boundary sample would duplicate it in the WAL and
    // double the apparent sample count downstream. `+1` skips the
    // boundary; one second of overlap is negligible because samples are
    // monotonically increasing at the host's interval.
    let mut from = cursor.as_ref().map(|c| c.last_synced_ts + 1).unwrap_or(0);
    // Don't backfill anything older than the source still has - source
    // will clamp us anyway, but skipping the round-trip is cheaper.
    if let Some(earliest) = h.earliest_sample_ts {
        if from < earliest {
            tracing::info!(
                rule_uuid = %rule.uuid,
                host_uuid = %h.uuid,
                from_was = from,
                earliest_available = earliest,
                "retention gap: advancing local cursor to source's earliest"
            );
            from = earliest;
        }
    }
    let (total_pulled, highest_ts) = pull_range_into_store(
        rule,
        peer,
        slot_uuid,
        h.uuid,
        h.interval_secs,
        h.chunk_window_secs,
        from,
        now,
        hzc,
        client,
        write_cursors,
    )
    .await?;
    let last_ts = if highest_ts >= from { highest_ts } else { from };
    if total_pulled > 0 {
        replication::upsert_cursor(pool, rule.id, h.uuid, last_ts, None).await?;
        tracing::info!(
            rule_uuid = %rule.uuid,
            host_uuid = %h.uuid,
            pulled = total_pulled,
            last_ts,
            "backfill complete"
        );
    } else if cursor.is_none() {
        // Initial pass with no data on source yet; still record a cursor.
        replication::upsert_cursor(pool, rule.id, h.uuid, from.max(now), None).await?;
    }
    Ok(())
}

/// Pull samples from the source's `/range` endpoint into the local HZC
/// store. Used by `backfill_host` for the initial per-host catch-up and
/// by `handle_sse_event` for in-band gap-fill when a live sample lands
/// far ahead of the local cursor.
///
/// Loops until the source signals `exhausted` or returns an empty page,
/// chasing the cursor forward through `truncated_to` markers when the
/// source's retention drops a window mid-pull. Returns
/// `(samples_written, highest_ts_seen)`. `highest_ts_seen` is `from-1`
/// when nothing landed - callers use that to decide whether to advance
/// a persisted cursor over an empty range.
async fn pull_range_into_store(
    rule: &replication::ReplicationRule,
    peer: &replication::ReplicationPeer,
    slot_uuid: Uuid,
    host_uuid: Uuid,
    interval_secs: i64,
    chunk_window_secs: i64,
    initial_from: i64,
    to: i64,
    hzc: &Arc<HzcStore>,
    client: &reqwest::Client,
    write_cursors: &WriteCursors,
) -> Result<(usize, i64)> {
    let expected_cadence = interval_secs.max(1);
    let mut from = initial_from;
    let mut total: usize = 0;
    let mut highest: i64 = from - 1;
    let mut sparse_warning_logged = false;
    loop {
        if from > to {
            break;
        }
        let resp = client
            .get(format!(
                "{}/api/v1/replication/slots/{slot_uuid}/range",
                peer.base_url
            ))
            .bearer_auth(&peer.api_token)
            .query(&[
                ("host", host_uuid.to_string()),
                ("from", from.to_string()),
                ("to", to.to_string()),
            ])
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(anyhow!("/range returned {}", resp.status()));
        }
        let body: wire::RangeResp = resp.json().await?;
        if let Some(skipped_to) = body.truncated_to {
            tracing::warn!(
                rule_uuid = %rule.uuid,
                %host_uuid,
                from_was = from,
                truncated_to = skipped_to,
                "source dropped retention before we could pull; advancing past the hole"
            );
            from = skipped_to;
            // Reflect the advance in `highest` so the caller's cursor
            // moves past the unfillable hole; otherwise we'd retry the
            // same range forever every reconcile pass.
            if from - 1 > highest {
                highest = from - 1;
            }
        }
        if body.samples.is_empty() {
            break;
        }
        // Detect sparse incoming data once per pull. If the average gap
        // between samples is more than 2× the host's declared interval,
        // source has almost certainly downsampled them and the
        // destination is now treating them as raw - logged so an
        // operator can spot when their tier policy needs widening to
        // match source's archive resolution.
        if !sparse_warning_logged && body.samples.len() >= 2 {
            let span = body.samples.last().unwrap().ts - body.samples.first().unwrap().ts;
            let avg_gap = span.checked_div(body.samples.len() as i64 - 1).unwrap_or(0);
            if avg_gap > expected_cadence.saturating_mul(2) {
                sparse_warning_logged = true;
                tracing::info!(
                    rule_uuid = %rule.uuid,
                    %host_uuid,
                    expected_cadence_secs = expected_cadence,
                    observed_avg_gap_secs = avg_gap,
                    "incoming samples sparser than probe cadence; source has likely \
                     downsampled this range. Destination will store at the cadence \
                     received and apply its own retention tiers to whatever lands."
                );
            }
        }
        // Write samples through HostWriter. Out-of-order timestamps are
        // supported by the WAL; the chunk encoder sorts on seal. The
        // HZC writer's API takes u32; sample-per-period intervals always
        // fit, but cap with a min to defend against bad manifest data.
        let writer = hzc.writer(
            host_uuid,
            interval_secs.clamp(1, i64::from(u32::MAX)) as u32,
            chunk_window_secs.clamp(60, i64::from(u32::MAX)) as u32,
        )?;
        for s in &body.samples {
            // Multi-path dedup: when this host's data is reachable via
            // more than one peer (cascading topologies), at most one
            // path writes any given timestamp. Subsequent paths drop
            // duplicates silently.
            if !dedup_admit(write_cursors, host_uuid, s.ts) {
                continue;
            }
            let slot = Slot {
                min: s.min,
                p2_5: s.p2_5,
                p25: s.p25,
                median: s.median,
                p75: s.p75,
                p97_5: s.p97_5,
                loss_pct: s.loss_pct,
            };
            writer.write_sample(s.ts, slot)?;
            total += 1;
        }
        highest = body.samples.last().map(|s| s.ts).unwrap_or(highest);
        from = highest + 1;
        if body.exhausted {
            break;
        }
    }
    Ok((total, highest))
}

/// Pop one fully-buffered SSE event from `buf`. Returns the event and
/// the remaining bytes. Events terminate with a blank line ("\n\n").
fn take_sse_event(buf: &[u8]) -> Option<(SseEvent, Vec<u8>)> {
    // SSE separator is "\n\n" (LF LF) per the WHATWG spec. We accept the
    // common CRLF variant too.
    let mut sep_at: Option<usize> = None;
    for i in 0..buf.len().saturating_sub(1) {
        if &buf[i..i + 2] == b"\n\n" {
            sep_at = Some(i);
            break;
        }
        if i + 3 < buf.len() && &buf[i..i + 4] == b"\r\n\r\n" {
            sep_at = Some(i + 2);
            break;
        }
    }
    let end = sep_at?;
    let event_bytes = &buf[..end];
    let rest = buf[end + 2..].to_vec();
    let text = String::from_utf8_lossy(event_bytes);
    let mut name = String::from("message");
    let mut data: Vec<String> = Vec::new();
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("event:") {
            name = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("data:") {
            data.push(v.trim().to_string());
        }
    }
    Some((
        SseEvent {
            name,
            data: data.join("\n"),
        },
        rest,
    ))
}

struct SseEvent {
    name: String,
    data: String,
}

#[allow(clippy::too_many_arguments)]
async fn handle_sse_event(
    pool: &SqlitePool,
    hzc: &Arc<HzcStore>,
    _data_dir: &std::path::Path,
    rule: &replication::ReplicationRule,
    peer: &replication::ReplicationPeer,
    slot_uuid: Uuid,
    ev: &SseEvent,
    pending_acks: &mut HashMap<Uuid, i64>,
    _events: &broadcast::Sender<haze_api::events_routes::ChangeKind>,
    samples_tx: &broadcast::Sender<haze_store::SampleEvent>,
    write_cursors: &WriteCursors,
    client: &reqwest::Client,
) -> Result<bool> {
    match ev.name.as_str() {
        "ping" => Ok(true),
        "lagged" => {
            tracing::warn!(
                rule_uuid = %rule.uuid,
                peer_name = %peer.name,
                "stream lagged on source; falling back to catch-up"
            );
            Ok(false)
        }
        "manifest-changed" => Ok(false),
        "sample" => {
            // Body shape mirrors what the source-side handler writes.
            #[derive(serde::Deserialize)]
            struct SamplePayload {
                host_uuid: Uuid,
                ts: i64,
                slot: SlotPayload,
            }
            #[derive(serde::Deserialize)]
            struct SlotPayload {
                min: f32,
                p2_5: f32,
                p25: f32,
                median: f32,
                p75: f32,
                p97_5: f32,
                loss_pct: f32,
            }
            let payload: SamplePayload = match serde_json::from_str(&ev.data) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(rule_uuid = %rule.uuid, error = %e, "bad sample payload");
                    return Ok(true);
                }
            };
            // Look up the host to find chunk_window + interval (it must
            // exist locally - catch-up materialised it).
            let host = match hosts::get_by_uuid(pool, payload.host_uuid).await? {
                Some(h) => h,
                None => {
                    tracing::warn!(
                        rule_uuid = %rule.uuid,
                        host_uuid = %payload.host_uuid,
                        "stream sample for unknown host; requesting manifest refresh"
                    );
                    return Ok(false);
                }
            };
            // Gap-aware write: if the live sample lands materially past
            // the persisted cursor, the destination missed a window
            // (initial-pairing backfill took a while, or a source-side
            // probe outage). Without this fill, advancing the cursor to
            // `payload.ts` would leave that window unfilled forever -
            // subsequent reconciles read `cursor + 1` and never look
            // back. We do an in-band range pull of `(cursor + 1,
            // payload.ts - 1)`; if the source can't serve that range
            // (retention dropped) the helper advances past the hole and
            // we move on.
            //
            // Threshold = max(3 × interval, 60 s) keeps the steady-state
            // happy-path silent: a one-sample SSE hiccup doesn't trigger
            // a round-trip. Sized so anything you'd notice in the UI
            // (anything above one minute) gets healed.
            if let Some(c) = replication::get_cursor(pool, rule.id, payload.host_uuid).await? {
                let interval = host.interval_secs.max(1);
                let gap_threshold = interval.saturating_mul(3).max(60);
                if payload.ts.saturating_sub(c.last_synced_ts) > gap_threshold {
                    tracing::info!(
                        rule_uuid = %rule.uuid,
                        host_uuid = %payload.host_uuid,
                        cursor_ts = c.last_synced_ts,
                        sample_ts = payload.ts,
                        gap_secs = payload.ts - c.last_synced_ts,
                        "SSE sample lands past gap; range-filling before live write"
                    );
                    match pull_range_into_store(
                        rule,
                        peer,
                        slot_uuid,
                        payload.host_uuid,
                        host.interval_secs,
                        host.chunk_window_secs,
                        c.last_synced_ts + 1,
                        payload.ts - 1,
                        hzc,
                        client,
                        write_cursors,
                    )
                    .await
                    {
                        Ok((wrote, highest)) => {
                            tracing::info!(
                                rule_uuid = %rule.uuid,
                                host_uuid = %payload.host_uuid,
                                wrote,
                                highest_ts = highest,
                                "gap-fill complete"
                            );
                        }
                        Err(e) => {
                            // Best-effort: log and fall through to the
                            // live write. The cursor will advance to
                            // payload.ts below, so the loop never
                            // retries this exact hole - that's the
                            // intentional fallback when source can't
                            // serve the range (network, 5xx, etc.).
                            tracing::warn!(
                                rule_uuid = %rule.uuid,
                                host_uuid = %payload.host_uuid,
                                error = %e,
                                "gap-fill failed; proceeding with live write and advancing cursor past hole"
                            );
                        }
                    }
                }
            }
            let writer = hzc.writer(
                payload.host_uuid,
                host.interval_secs.clamp(1, i64::from(u32::MAX)) as u32,
                host.chunk_window_secs.clamp(60, i64::from(u32::MAX)) as u32,
            )?;
            let slot = Slot {
                min: payload.slot.min,
                p2_5: payload.slot.p2_5,
                p25: payload.slot.p25,
                median: payload.slot.median,
                p75: payload.slot.p75,
                p97_5: payload.slot.p97_5,
                loss_pct: payload.slot.loss_pct,
            };
            if !dedup_admit(write_cursors, payload.host_uuid, payload.ts) {
                tracing::debug!(
                    rule_uuid = %rule.uuid,
                    host_uuid = %payload.host_uuid,
                    ts = payload.ts,
                    "dropped duplicate sample (already written via another replication path)"
                );
                return Ok(true);
            }
            writer.write_sample(payload.ts, slot)?;
            // Re-broadcast on our local samples channel so any downstream
            // SSE subscribers (instances that pull from US) see this
            // sample land. This is what makes cascading replication
            // forward through the chain (1 -> 2 -> 3 -> ...). The
            // downstream SSE handler still applies its own host_filter,
            // so it only ships events for hosts that match its slot's
            // source group - never a "dumb re-forward".
            let _ = samples_tx.send(haze_store::SampleEvent {
                host_uuid: payload.host_uuid,
                timestamp_secs: payload.ts,
                slot,
            });
            // Track + cursor-update batched per host.
            replication::upsert_cursor(pool, rule.id, payload.host_uuid, payload.ts, None).await?;
            pending_acks.insert(payload.host_uuid, payload.ts);
            tracing::debug!(
                rule_uuid = %rule.uuid,
                host_uuid = %payload.host_uuid,
                ts = payload.ts,
                "ingested replicated sample"
            );
            Ok(true)
        }
        other => {
            tracing::debug!(rule_uuid = %rule.uuid, event = other, "ignoring unknown SSE event");
            Ok(true)
        }
    }
}

async fn flush_acks(
    pool: &SqlitePool,
    client: &reqwest::Client,
    peer: &replication::ReplicationPeer,
    slot_uuid: Uuid,
    pending: &mut HashMap<Uuid, i64>,
) {
    if pending.is_empty() {
        return;
    }
    let body: Vec<wire::AckEntry> = pending
        .drain()
        .map(|(host_uuid, last_ts)| wire::AckEntry { host_uuid, last_ts })
        .collect();
    let resp = client
        .post(format!(
            "{}/api/v1/replication/slots/{slot_uuid}/ack",
            peer.base_url
        ))
        .bearer_auth(&peer.api_token)
        .json(&body)
        .send()
        .await;
    if let Ok(r) = resp {
        if r.status().is_success() {
            // Streaming is healthy: clear any stale error so the UI's
            // Status column flips from "error" back to "active" instead
            // of sticking on whatever the last transient hiccup was.
            let _ = replication::update_peer(
                pool,
                peer.uuid,
                replication::PeerPatch {
                    last_error: Some(None),
                    last_contact_at: Some(Some(chrono::Utc::now().timestamp())),
                    ..Default::default()
                },
            )
            .await;
        }
    }
}

// Suppress the unused warning until we wire dest-side host listing for stats.
#[allow(dead_code)]
fn _unused(_: GroupFilter) {}

/// Acquire a permit from the replication worker pool. Logs an INFO line
/// when the pool is saturated and we have to queue, then another when the
/// permit lands - so an admin grepping for the rule UUID sees exactly
/// when sharing caused a delay. When permits are free we acquire silently
/// (the common path).
async fn acquire_pool_permit(
    pool_sem: &ReplicationPool,
    phase: &'static str,
    rule_uuid: Uuid,
    peer_name: &str,
) -> Result<tokio::sync::OwnedSemaphorePermit> {
    acquire_pool_permit_inner(
        pool_sem.semaphore.clone(),
        pool_sem.capacity,
        phase,
        rule_uuid,
        peer_name,
    )
    .await
}

async fn acquire_pool_permit_inner(
    sem: Arc<tokio::sync::Semaphore>,
    capacity: usize,
    phase: &'static str,
    rule_uuid: Uuid,
    peer_name: &str,
) -> Result<tokio::sync::OwnedSemaphorePermit> {
    let free = sem.available_permits();
    if free == 0 {
        tracing::info!(
            %rule_uuid,
            peer_name,
            phase,
            pool_capacity = capacity,
            "replication worker pool saturated; waiting for a free slot \
             (raise worker_pools.replication to reduce queueing)"
        );
        let started = std::time::Instant::now();
        let permit = sem.acquire_owned().await?;
        tracing::info!(
            %rule_uuid,
            peer_name,
            phase,
            waited_ms = started.elapsed().as_millis() as u64,
            "replication worker permit acquired after wait"
        );
        Ok(permit)
    } else {
        Ok(sem.acquire_owned().await?)
    }
}
