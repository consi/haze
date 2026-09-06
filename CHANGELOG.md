# Changelog

Notable changes to Haze. Generated from Git history with git-cliff.

## v0.6.0 — 2026-09-05

### Features

- Add ICMP route history and efficient trace metadata storage ([07145d6](https://github.com/consi/haze/commit/07145d62dd1435648709484634f5b1bba5ffd008))

[Compare changes](https://github.com/consi/haze/compare/v0.5.0...v0.6.0)
## v0.5.0 — 2026-09-01

### Maintenance and improvements

- Improve alerting, graphs, and dependency security ([4d3e675](https://github.com/consi/haze/commit/4d3e675cf0ed2feaf7752de35da8bce44404ca68))

Remove multi-period graphs, preserve historical ranges, and improve loading feedback. Make alerts public-safe, grouped, ordered, and linked into the matching sidebar tree. Upgrade and audit the Rust and frontend dependency stacks.

[Compare changes](https://github.com/consi/haze/compare/v0.4.0...v0.5.0)
## v0.4.0 — 2026-06-09

### Fixes

- Fix month-boundary rollup data loss: merge-rebundle, tier-finality gating ([9d7c47c](https://github.com/consi/haze/commit/9d7c47c4df12f0b4149adbda9cf2f02711887a90))

The G2/G3 rollup sealed calendar-span bundles while late-span data was
still in a finer tier, then deleted that data's chunks as stale leftovers
once the compactor downsampled them (the May 26 - Jun 1 gap). Replace the
sweep with a merge-and-rebundle primitive shared by G1/G2/G3 (decode every
group member incl. the existing bundle, merge, verify, only then delete),
stamp bundles with the actual data range instead of the calendar span,
gate monthly/yearly groups on tier finality, downsample G1+ chunks whole
(no per-boundary fragment churn), and dedupe identical timestamps in the
reader.

[Compare changes](https://github.com/consi/haze/compare/v0.3.1...v0.4.0)
## v0.3.1 — 2026-05-25

### Fixes

- Heal replication gaps via in-band range-fill on SSE samples ([63efd92](https://github.com/consi/haze/commit/63efd924f6e2c2cb5a21a6be439ba20f86a3aa29))

When a live SSE sample lands far past the persisted cursor (initial-pairing
backfill window, source-side hole, or post-reconnect lag), pull the
missing range from the source before writing and bumping the cursor.
Falls through to a cursor advance if the source can't serve the window
(retention dropped, transient network error) so the loop never re-queries
an unfillable hole.

Extracts the range-fetch loop out of backfill_host so initial catch-up
and gap-fill share one code path.

[Compare changes](https://github.com/consi/haze/compare/v0.3.0...v0.3.1)
## v0.3.0 — 2026-05-25

### Features

- Add G2/G3 rollup bundles, per-sample tier downsampling, deterministic seqs ([a707d1b](https://github.com/consi/haze/commit/a707d1bc143b5a066b85afa1b1b0414ca64d2cf7))

- G2 (monthly) and G3 (yearly) rollup phases on top of G1, scheduled after G1
  in the existing rollup task. New settings hzc.rollup_g2_settled_after_secs
  and hzc.rollup_g3_settled_after_secs.
- Per-generation zstd level: G0=3, G1=9, G2=13, G3=15. encode_chunk gains a
  level param; on-disk format unchanged (zstd self-describes).
- compactor: per-sample tier bucketing for G1+ sources fixes the sparse-chunk
  edge case where a bundle's nominal end_ts is inside a tier but its actual
  samples are past the horizon. Splits a chunk into per-tier sub-spans.
- Deterministic bundle_seq(gen, res, start_ts) with top-bit-set replaces the
  max(seq)+1 scheme - crash-and-retry now overwrites cleanly.
- READ_RETRY_LIMIT 3 -> 5 to absorb concurrent G1/G2/G3 publish/delete.
- Clock trait + ManualClock for tests; HzcStore::new_with_clock seam.
- Lifecycle and compression-measurement tests under crates/haze-store/tests,
  gated by #[ignore] and run via --ignored.

- Add cross-instance replication: pull-direction config + SSE streaming ([336b978](https://github.com/consi/haze/commit/336b978bb1c95f5f7b75bbd619dbc39ba54889b9))

Each Haze can register one or more peer instances and pull groups (or
the entire tree) into local groups, with same-name groups merging under
the same parent. Source UUIDs are preserved on replicated hosts so
cross-instance references stay stable. Replicated hosts/groups are
immutable apart from local display name + group memberships, and the
probe scheduler skips them so data is never re-probed locally - the
worker just lands incoming samples in the local HZC store.

Wire: admin-gated /api/v1/replication/{peers,rules,inbound,slots}.
Destinations open a long-lived SSE stream after an initial /manifest +
/range catch-up; reqwest has connect_timeout only (no overall timeout)
so streams aren't killed every 15 s. A periodic background reconcile
re-runs catch-up alongside the open stream. Worker has error
hysteresis (3 consecutive failures before surfacing in UI) and a 60 s
backoff cap so admin actions like Unblock take effect within a minute.

Loop guard: stable per-instance UUID, X-Replication-Path header on
every call, transitive upstream_chain refreshed on each reconcile.
Multi-path topologies (8 <- 6 AND 8 <- 7 with shared upstreams) are
deduplicated by a per-host high-water-mark cursor so the same sample
written via multiple paths lands once.

Tokens: new replication_only scope, settable only by admins; middleware
returns 403 on any non-/replication path for these tokens.

Block/unblock on inbound slots: force-remove flips a blocked_at flag
that lives on the slot row (peer instance UUID preserved). Destination
sees 403 on every wire call until an admin clicks Unblock - no rule
recreation needed on the destination side. Live SSE streams are
terminated immediately on block via the existing event broadcaster.

Worker pool: new worker_pools.replication setting (default 16) bounds
concurrent catch-up bursts. Per-rule SSE streams do NOT hold permits
so a pool of 16 supervises hundreds of streaming rules; only the
I/O-heavy catch-up phase serialises. Pool saturation is logged so an
operator knows when to raise the cap.

UI:
 - Settings > Replication section with paginated Peers / Rules / Inbound
   tables, live lag counter (ticks every second locally, refetches on
   ChangeKind::Replication every ~30 s).
 - Status column shows OK / version / latency from the last
   /instance-info handshake (matches the manual Test button).
 - Topology modal with zoom + drag SVG graph of upstream chain, merges
   shared ancestors; hidden when no peer has upstreams.
 - "My instance id" next to the Replication header so operators know
   what to hand to a peer.
 - Replicated hosts rendered in dark grey in the sidebar tree; groups
   keep default colour because they can be merge targets.
 - Edit modals strictly gated: replicated host modal shows Name +
   Groups + Delete only; replicated group modal hides parent picker.
   API enforces the same gate (422 on probe-* PATCH).

Fixes encountered along the way:
 - reqwest 15 s timeout killing SSE every ~15 s with "error decoding
   response body" - removed for stream client.
 - Local probe scheduler started replicated hosts at boot - now filters
   replication_peer_id IS NULL.
 - Group + host membership union across rules instead of replace
   (prevents thrashing when same host is in multiple rules).
 - Catch-up cursor advances by +1 to avoid duplicating the boundary
   sample with the live stream.
 - hosts_routes::update no longer calls scheduler.restart() for
   replicated hosts.
 - last_error is cleared on every successful instance-info refresh and
   ack flush so the UI doesn't pin on stale errors.
 - HostResp / GroupResp now serialise replication_peer_id so the
   frontend can branch on it.
 - Source side serves replicated hosts in /manifest + /range so
   cascading works (A -> B -> C); cycle detection still refuses loops.

Docker / packaging:
 - Production Dockerfile uses setcap cap_net_raw=+ep in the picker
   stage so the non-root distroless runtime can run ping probes.
 - README updates show --cap-add NET_RAW + ping_group_range sysctl.
 - HAZE_BOOTSTRAP_PASSWORD env var skips the random first-boot admin
   password when set.

Test harness (scripts/replication_e2e_test.sh +
docker-compose.replication-test.yml): 8 instances on ports 4001-4008
with the topology 1->2->3->4->5, 4->6, 2->7, 6->8, 7->8. Exercises
cascading data flow, group remap, late peer wire-up, rule
delete/recreate, block/unblock data freeze + resume,
replication-only token scope, cycle refusal (1 trying to peer with 8),
host orphaning on source-side removal, and per-round symmetry checks
that detect stalls by tracking drift growth between rounds.

- Allow dist/ into Docker context for release image build ([559ab09](https://github.com/consi/haze/commit/559ab09afc37e6b9976c5c226d074f031276febb))

The release Dockerfile copies pre-built musl binaries from dist/, but
.dockerignore was filtering the directory out, causing buildx to fail
with '/dist: not found' during the multi-arch image build.

### Documentation

- Readme update ([333b72f](https://github.com/consi/haze/commit/333b72fe52ca9bb140b2ecee6ada3aea2405290f))

[Compare changes](https://github.com/consi/haze/compare/v0.2.3...v0.3.0)
## v0.2.3 — 2026-05-19

### Security

- Patch hickory CVE, fix multi-period zoom + render delay, gate tree edit ([68d2fe2](https://github.com/consi/haze/commit/68d2fe23372802e46c3d8a4be0cdac4a75b7616d))

[Compare changes](https://github.com/consi/haze/compare/v0.2.2...v0.2.3)
## v0.2.2 — 2026-05-19

### Fixes

- Recover malformed WAL, copy charts as PNG, timezone setting, UX polish ([066ab91](https://github.com/consi/haze/commit/066ab915b8de9fa9d27fbeda262f820119c2ee3c))

[Compare changes](https://github.com/consi/haze/compare/v0.2.1...v0.2.2)
## v0.2.1 — 2026-05-19

### Fixes

- Fix stale zoom-out span, mobile touch inset, HTTP cookie Secure, FD ulimit ([25e2072](https://github.com/consi/haze/commit/25e20720e6c8da627179bf5361595bdacd8f9b57))

[Compare changes](https://github.com/consi/haze/compare/v0.2.0...v0.2.1)
## v0.2.0 — 2026-05-18

### Features

- Add public mode, anonymous rate limits, mobile UI, daily rollup ([9242d9d](https://github.com/consi/haze/commit/9242d9d837e96be27328e28310813b9eb05af5bf))

- Public mode toggle exposes the read-only dashboard anonymously;
  per-IP rate limits and SSE-per-IP cap gate anonymous traffic only.
- ViewerAccess extractor on read endpoints; writes unchanged.
- OpenAPI servers + per-path security follow HAZE_BASE_URL and the
  live public_mode setting.
- Burger drawer nav, sticky preset bar, full-screen modals, touch
  drag-to-zoom + double-tap zoom-out + long-press bottom tooltip.
- Daily rollup task bundles settled-day chunks into one zstd file.
- Dockerfile: pre-chown /var/lib/haze for distroless nonroot.

[Compare changes](https://github.com/consi/haze/compare/v0.1.2...v0.2.0)
## v0.1.2 — 2026-05-18

### Features

- Add HAZE_BASE_URL for sub-path deployments ([ad878ee](https://github.com/consi/haze/commit/ad878eee5c139c84fd2603df79fc3316d30e30a9))

[Compare changes](https://github.com/consi/haze/compare/v0.1.1...v0.1.2)
## v0.1.1 — 2026-05-18

### Features

- Add SSE live refresh, session liveness, modal polish, SIGTERM handling ([416052b](https://github.com/consi/haze/commit/416052b2222312cf8e416a06e0694a46c8c7eeb8))

- /api/v1/events SSE stream pushes typed change events; frontend
  reload-key dispatcher refetches tree, alerts, webhooks, users,
  settings without polling.
- Global 401 handler in the API client redirects to /login so
  background-revoked sessions don't silently fail; password change
  forces re-login.
- Three chip-input pickers (host group + alert target) gain arrow-key
  navigation with visible highlight and scroll-into-view; Esc closes
  the picker before the modal.
- Modal backdrop close now requires mousedown + click both on the
  backdrop, so drag-to-select escaping the dialog no longer dismisses.
- shutdown_signal races SIGINT against SIGTERM (PID 1 in distroless)
  and wakes SSE handlers via a shared Notify so graceful shutdown
  actually drains.
- Tests workflow now also runs on push to main so the badge reports.

[Compare changes](https://github.com/consi/haze/compare/v0.1.0...v0.1.1)
## v0.1.0 — 2026-05-17

### Maintenance and improvements

- Initial public release ([30d45b0](https://github.com/consi/haze/commit/30d45b07fc82858e0da2247edc0f42a55babdddf))

Haze v0.1.0: network latency monitor with embedded SvelteKit UI.

- Six probe types (ping, dns, tcp_connect, tls_connect, http_ttfb, http_total)
  rendering as percentile bands with loss-driven opacity.
- Single static musl binary; SvelteKit frontend embedded via rust-embed.
- SQLite storage with auto-migrations and first-boot admin provisioning.
- Password + WebAuthn passkey auth, alerts, multi-host overlay views.
- Distroless Docker image (linux/amd64, linux/arm64).
- DEB + RPM packages with systemd unit (CAP_NET_RAW only).
- Tag-driven release pipeline: 'v*.*.*' triggers static binaries, packages,
  multi-arch image push to ghcr.io, and a GitHub Release.

