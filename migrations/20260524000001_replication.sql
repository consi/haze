-- Replication: pull-direction config between Haze instances. The destination
-- side stores peers + rules + per-host cursors, the source side stores slots
-- (one per destination/group pair) so it knows who's tailing what.
--
-- Replicated hosts/groups carry `replication_peer_id` so the UI can render
-- them differently (dark grey) and the API can refuse edits to anything
-- other than the display name.

-- ---------------------------------------------------------------------------
-- Destination side
-- ---------------------------------------------------------------------------

-- A peer is a remote Haze instance we pull from. `api_token` is the
-- plaintext bearer (`hzt_…`) of an admin user on the source; we send it on
-- every wire call. It's never returned in GET responses. `base_url` is
-- immutable post-creation because changing it would re-pair to a different
-- instance and invalidate `slot_uuid`s stored on rules. `source_version`
-- and `last_latency_ms` are populated by the worker on each successful
-- `/instance-info` handshake so the Settings UI's Status column can show
-- the same `OK · ver · ms` shape as the manual Test button without an
-- extra click.
CREATE TABLE replication_peers (
    id                      INTEGER PRIMARY KEY,
    uuid                    BLOB    NOT NULL UNIQUE,
    name                    TEXT    NOT NULL UNIQUE,
    base_url                TEXT    NOT NULL,
    api_token               TEXT    NOT NULL,
    source_instance_uuid    BLOB,
    upstream_chain          TEXT    NOT NULL DEFAULT '[]',
    tls_skip_verify         INTEGER NOT NULL DEFAULT 0,
    reconcile_interval_secs INTEGER NOT NULL DEFAULT 300,
    created_at              INTEGER NOT NULL,
    last_contact_at         INTEGER,
    last_error              TEXT,
    source_version          TEXT,
    last_latency_ms         INTEGER
);

-- A rule is "pull source_group_uuid into dest_group_uuid". Both UUIDs are
-- the zero UUID when they refer to the root. `slot_uuid` is assigned by the
-- source on the first POST /slots and stored here so subsequent calls (and
-- the SSE stream) can address the slot.
CREATE TABLE replication_rules (
    id                INTEGER PRIMARY KEY,
    uuid              BLOB    NOT NULL UNIQUE,
    peer_id           INTEGER NOT NULL REFERENCES replication_peers(id) ON DELETE CASCADE,
    source_group_uuid BLOB    NOT NULL,
    dest_group_uuid   BLOB    NOT NULL,
    slot_uuid         BLOB,
    enabled           INTEGER NOT NULL DEFAULT 1,
    created_at        INTEGER NOT NULL,
    UNIQUE(peer_id, source_group_uuid, dest_group_uuid)
);
CREATE INDEX idx_replication_rules_peer ON replication_rules(peer_id);

-- Per-(rule, source-host) cursor. `host_uuid` matches the LOCAL host's UUID
-- (we preserve the source UUID on ingest so cross-instance refs work).
-- `orphaned_at` is set when the source's manifest stops listing the host
-- but we keep the local data.
CREATE TABLE replication_cursors (
    rule_id         INTEGER NOT NULL REFERENCES replication_rules(id) ON DELETE CASCADE,
    host_uuid       BLOB    NOT NULL,
    last_synced_ts  INTEGER NOT NULL DEFAULT 0,
    last_attempt_at INTEGER,
    last_error      TEXT,
    orphaned_at     INTEGER,
    PRIMARY KEY (rule_id, host_uuid)
);

-- Per-rule mapping from source group UUID to the LOCAL group UUID we
-- merged it into. Lets the destination keep merging into the same local
-- group across renames on the source.
CREATE TABLE replication_group_map (
    rule_id           INTEGER NOT NULL REFERENCES replication_rules(id) ON DELETE CASCADE,
    source_group_uuid BLOB    NOT NULL,
    local_group_uuid  BLOB    NOT NULL,
    PRIMARY KEY (rule_id, source_group_uuid)
);

-- ---------------------------------------------------------------------------
-- Source side
-- ---------------------------------------------------------------------------

-- A slot is "a destination Haze pulling source_group from us". Created on
-- the first POST /replication/slots from that destination and removed when
-- the destination deletes its rule. `replication_path` is a JSON array of
-- instance UUIDs ending at the destination - used to refuse cycles.
-- `blocked_at` is set by the operator's "Block" action in the Inbound
-- table: the row stays so the destination's instance UUID is preserved
-- (and re-pair attempts keep getting 403) until an admin unblocks.
CREATE TABLE replication_slots (
    id                 INTEGER PRIMARY KEY,
    slot_uuid          BLOB    NOT NULL UNIQUE,
    peer_instance_uuid BLOB    NOT NULL,
    peer_label         TEXT    NOT NULL,
    source_group_uuid  BLOB    NOT NULL,
    replication_path   TEXT    NOT NULL,
    created_at         INTEGER NOT NULL,
    last_stream_at     INTEGER,
    blocked_at         INTEGER,
    UNIQUE(peer_instance_uuid, source_group_uuid)
);

-- Informational ack from the destination ("I've stored up to this ts for
-- this host"). Source never trims data based on acks; this is purely for
-- the operator-facing Inbound table.
CREATE TABLE replication_slot_cursors (
    slot_id       INTEGER NOT NULL REFERENCES replication_slots(id) ON DELETE CASCADE,
    host_uuid     BLOB    NOT NULL,
    last_acked_ts INTEGER NOT NULL,
    PRIMARY KEY (slot_id, host_uuid)
);

-- ---------------------------------------------------------------------------
-- Mark hosts/groups whose origin is replication. Nullable: NULL means
-- locally created (probe-sourced for hosts, operator-created for groups).
-- ON DELETE SET NULL keeps the data after the peer is removed; the row
-- becomes a plain local one with the same UUID.
-- ---------------------------------------------------------------------------
ALTER TABLE hosts  ADD COLUMN replication_peer_id INTEGER REFERENCES replication_peers(id) ON DELETE SET NULL;
ALTER TABLE groups ADD COLUMN replication_peer_id INTEGER REFERENCES replication_peers(id) ON DELETE SET NULL;

-- ---------------------------------------------------------------------------
-- Token scope flag: when `replication_only = 1`, the token is only
-- accepted on paths under `/api/v1/replication`. Lets an operator hand
-- out a token to another Haze instance for cross-instance pulls without
-- granting full admin authority over this instance. Default 0 keeps
-- existing tokens unchanged - they continue to work everywhere.
-- ---------------------------------------------------------------------------
ALTER TABLE api_tokens ADD COLUMN replication_only INTEGER NOT NULL DEFAULT 0;
