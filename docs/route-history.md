# ICMP route history

ICMP hosts capture a route around every 30 completed monitoring periods (configurable in Settings → Workers),
independent of the number of ping packets in a period. History starts after
upgrade; older graphs do not contain paths that can be reconstructed.

Use **Route history** on a host graph or graph card. The modal inherits the
graph range. Drag the overview to zoom, click it to load nearby events, or
jump to a local date and time. Earlier/later controls and arrow keys navigate
events. The event list pages in both directions and renders at most 400 rows.
DNS/IP switches use names captured with the trace, with IP fallback.

Routes are sampled observations, not continuous topology tracking. A change
was first observed between two captures. Missing intermediate replies are
shown separately from changed responding addresses. Hop loss measures replies
to five traceroute probes and is not evidence of forwarding loss; destination
loss comes from ordinary ping measurements, including short incidents between
captures.

## Collection and capacity

The native collector uses one shared ICMP socket per address family, separate
from ordinary ping sockets. A dispatcher matches target, identifier and sequence;
a short send lock preserves each packet's TTL. No subprocess or external
traceroute package is required. IPv4 and IPv6 use up to 32 hops and five rounds.
The default network deadline is 60 seconds, with 2 seconds per hop reply.
Unreached destinations and network deadlines retain responding transit hops
and the measurements collected so far. Partial captures do not replace the
last complete route checkpoint. New records include an optional `previous_id`
so the comparison panel uses that complete capture even when queue gaps or
partial captures intervene; old records retain their existing behavior. A first partial capture is visible as
"Destination not reached", including in the default event filter.

PTR names are best-effort enrichment: deduplicated lookups run concurrently
(up to eight per capture), with 250 ms per lookup and one second overall.
Failures, missing PTR records, and budget expiry leave numeric IPs intact and
never fail reporting. A queue-deadline event means no worker started the trace;
it cannot contain transit observations and is distinct from an incomplete path.

`Settings → Workers → ICMP traceroutes` controls simultaneous captures (default
8, maximum 64; restart required). One capture may wait per host, for at most
300 seconds by default. Cadence, queue, network and reply timeouts are configurable
(restart required). Each capture interval is randomized ±⅓ (20–40 entries at the
default of 30); the first capture is randomized across the full cycle, including
on upgrade from the old fixed cadence. Countdown state survives restarts. Missed captures become
collection-gap events. Increasing this pool adds in-flight work, not sockets.
The service needs its existing `CAP_NET_RAW` capability.

## Metadata storage and durability

`haze-store::MetadataStore` is a reusable, versioned append-only store. Each
record has an origin UUID, host UUID, timestamp, kind, schema version, shared
context, observation data, and local ingestion sequence. Origin IDs deduplicate
replication; ingestion sequences safely paginate late/out-of-order observations.

Files live beside graph chunks under `hzc/<shard>/<host>/metadata/`:

- `active.wal`: bounded length-framed JSON records with SHA-256 checksums.
- `*.hzm.zst`: immutable bounded blocks with independently compressed and
  checksummed indexes and payloads; context dictionaries and delta timestamps
  avoid repeating route structure. Files are sealed on host-window boundaries
  or size limits. Older consecutive blocks are bundled within a UTC day.
- Small JSON checkpoints retain cadence, last observed route, and replication
  progress. Routine checkpoints are coalesced in memory and written every
  30 seconds without per-counter fsyncs.

WAL writes do not fsync on the collection path. Background flushes sync changed
WALs; shutdown, sealing and replication establish explicit durability boundaries.
A power failure can lose recent unflushed observations/cadence. Incomplete final
WAL frames are discarded on recovery; corrupt checksums or unknown formats are
reported without rewriting the affected file. Normal graph collection continues
when trace collection or metadata storage fails.

Metadata uses the longest configured graph retention horizon, retaining boundary
context for traces and loss. Event timestamps are not downsampled. Host deletion
removes its metadata with its graph directory. Existing HZC formats and embedded
SQL migrations are unchanged by this feature.

## API and replication compatibility

`GET /api/v1/hosts/{uuid}/route-history` accepts `from`, `to`, `all`, `limit`,
`before`, `newer`, and `at`; it returns a 240-bucket timeline and a cursor-paged
event list. `GET .../route-history/{id}` loads a selected observation and its
preceding path. These endpoints use the same viewer/public-mode access as graphs.

Replication advertises `metadata_v1`. New peers use slot-scoped `/metadata`
catch-up plus `metadata` SSE events. Metadata checkpoints are independent of
sample cursors and are advanced only after durable ingestion. Live events never
advance catch-up checkpoints; periodic reconciliation repairs stream gaps.
Unknown SSE event types remain compatible with older receivers. Missing source
capabilities disable metadata transfer while sample replication continues.

The Debian package upgrades existing installations using the existing automatic
migration mechanism. Existing settings without a traceroute worker field receive
the default. No graph conversion or manual database migration is needed.

## Verification

- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `npm --prefix frontend run check && npm --prefix frontend run build`
- `cargo test -p haze-store --test metadata_lifecycle benchmark_metadata_thousand_hosts -- --ignored --nocapture`
- `cargo test -p haze-probe live_loopback -- --ignored --nocapture` requires
  `CAP_NET_RAW` and tests IPv4/IPv6 loopback only.
- `python scripts/route_history_ui_test.py` requires Playwright and Chromium,
  plus a local Haze server (`HAZE_TEST_URL`, default `http://127.0.0.1:4421`).
  It mocks API responses and checks light/dark/mobile layouts and interactions.
