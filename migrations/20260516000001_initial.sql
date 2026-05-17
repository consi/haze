-- Initial schema: groups, hosts, users, passkeys, sessions, alerts, audit.

CREATE TABLE groups (
    id           INTEGER PRIMARY KEY,
    -- Opaque identifier used to build the materialized `path` so the tree can
    -- be subtree-queried (`path LIKE prefix%`) without involving display names.
    uuid         BLOB NOT NULL UNIQUE,
    parent_id    INTEGER REFERENCES groups(id) ON DELETE CASCADE,
    display_name TEXT NOT NULL,
    path         TEXT NOT NULL UNIQUE,
    depth        INTEGER NOT NULL,
    created_at   INTEGER NOT NULL
);
CREATE INDEX idx_groups_path ON groups(path);
-- Sibling groups must have distinct display names (case-insensitive).
-- `COALESCE(parent_id, -1)` collapses NULL parents into a single "root"
-- bucket so SQLite's NULL-is-distinct semantics don't let two top-level
-- groups share a name.
CREATE UNIQUE INDEX ux_groups_sibling_name
    ON groups(COALESCE(parent_id, -1), display_name COLLATE NOCASE);

CREATE TABLE hosts (
    id                  INTEGER PRIMARY KEY,
    -- Opaque identifier shared with HZC storage (the per-host directory is
    -- sharded by the same UUID), and what the API returns for external refs.
    uuid                BLOB NOT NULL UNIQUE,
    display_name        TEXT NOT NULL,
    probe_type          TEXT NOT NULL CHECK (probe_type IN ('ping','dns','tcp_connect','tls_connect','http_ttfb','http_total')),
    probe_config        TEXT NOT NULL,
    interval_secs       INTEGER NOT NULL DEFAULT 60,
    samples_per_period  INTEGER NOT NULL DEFAULT 20,
    -- Window (seconds) the host's writer uses for rolling chunks. Decided
    -- at host creation, stored here AND in the host's HZC meta.json on
    -- disk - so existing hosts keep their original window for life.
    -- Changing it on disk later would require a one-off migration that
    -- re-keys the chunk directory.
    chunk_window_secs   INTEGER NOT NULL DEFAULT 3600,
    enabled             INTEGER NOT NULL DEFAULT 1,
    created_at          INTEGER NOT NULL
);
CREATE INDEX idx_hosts_kind ON hosts(probe_type);
-- Host display names are globally unique (case-insensitive). Hosts can
-- belong to any number of groups, so there's no per-parent scope to use
-- like groups have.
CREATE UNIQUE INDEX ux_hosts_display_name
    ON hosts(display_name COLLATE NOCASE);

-- A host can live in any number of groups (including zero, in which case it
-- shows up at the tree root). Group membership is many-to-many; rows here
-- are the join table.
CREATE TABLE host_groups (
    host_id  INTEGER NOT NULL REFERENCES hosts(id) ON DELETE CASCADE,
    group_id INTEGER NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    PRIMARY KEY (host_id, group_id)
);
CREATE INDEX idx_host_groups_group ON host_groups(group_id);

CREATE TABLE users (
    id            INTEGER PRIMARY KEY,
    username      TEXT NOT NULL UNIQUE,
    password_hash TEXT,
    role          TEXT NOT NULL CHECK (role IN ('admin','user','reader','disabled')),
    created_at    INTEGER NOT NULL,
    disabled_at   INTEGER
);

CREATE TABLE passkey_credentials (
    id            INTEGER PRIMARY KEY,
    user_id       INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    credential_id BLOB NOT NULL UNIQUE,
    passkey_json  TEXT NOT NULL,
    label         TEXT,
    created_at    INTEGER NOT NULL,
    last_used_at  INTEGER
);

CREATE TABLE sessions (
    id          BLOB PRIMARY KEY,
    user_id     INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at  INTEGER NOT NULL,
    expires_at  INTEGER NOT NULL,
    user_agent  TEXT,
    remote_addr TEXT
);
CREATE INDEX idx_sessions_user    ON sessions(user_id);
CREATE INDEX idx_sessions_expires ON sessions(expires_at);

-- Webhook library managed in /settings (admin only). Rules reference these
-- by id; deleting a webhook with live rules is refused at the API layer.
-- `headers` is a JSON object: { "Header-Name": "value", ... } sent with
-- every notification POST (lets the receiver authenticate via, e.g.,
-- `Authorization: Bearer …` without burying it in the URL).
CREATE TABLE webhooks (
    id          INTEGER PRIMARY KEY,
    uuid        BLOB    NOT NULL UNIQUE,
    name        TEXT    NOT NULL,
    url         TEXT    NOT NULL,
    headers     TEXT    NOT NULL DEFAULT '{}',
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);

-- One rule = one metric / aggregation / window / direction with up to two
-- thresholds (warning, critical). At least one threshold must be non-NULL
-- (validated at the API layer). The aggregation is applied to the chosen
-- Slot field across every sample in the sliding window.
CREATE TABLE alert_rules (
    id                  INTEGER PRIMARY KEY,
    uuid                BLOB    NOT NULL UNIQUE,
    name                TEXT    NOT NULL,
    enabled             INTEGER NOT NULL DEFAULT 1,
    metric              TEXT    NOT NULL CHECK (metric IN
                            ('min','p2_5','p25','median','p75','p97_5','loss_pct')),
    aggregation         TEXT    NOT NULL CHECK (aggregation IN
                            ('max','avg','min','p50','p75','p90','p95','p99')),
    direction           TEXT    NOT NULL CHECK (direction IN ('above','below')),
    warning_threshold   REAL,
    critical_threshold  REAL,
    window_secs         INTEGER NOT NULL,
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL
);

-- Mixed targets: one rule can reference any number of hosts and groups.
-- target_id resolves to hosts.id or groups.id depending on target_kind.
CREATE TABLE alert_rule_targets (
    rule_id     INTEGER NOT NULL REFERENCES alert_rules(id) ON DELETE CASCADE,
    target_kind TEXT    NOT NULL CHECK (target_kind IN ('host','group')),
    target_id   INTEGER NOT NULL,
    PRIMARY KEY (rule_id, target_kind, target_id)
);
CREATE INDEX idx_alert_rule_targets_rule ON alert_rule_targets(rule_id);

CREATE TABLE alert_rule_webhooks (
    rule_id    INTEGER NOT NULL REFERENCES alert_rules(id) ON DELETE CASCADE,
    webhook_id INTEGER NOT NULL REFERENCES webhooks(id)    ON DELETE CASCADE,
    PRIMARY KEY (rule_id, webhook_id)
);

-- One row per (rule, host) pair. severity transitions persist immediately,
-- the in-memory series snapshot below is what gets flushed periodically.
-- `last_value` / `last_threshold` capture what tripped the alert on the
-- most recent transition so the UI can show *why* a rule is firing without
-- re-running the evaluation.
CREATE TABLE alert_state (
    rule_id           INTEGER NOT NULL REFERENCES alert_rules(id) ON DELETE CASCADE,
    host_id           INTEGER NOT NULL REFERENCES hosts(id)       ON DELETE CASCADE,
    severity          TEXT    NOT NULL CHECK (severity IN ('ok','warning','critical')),
    since             INTEGER NOT NULL,
    last_notified_at  INTEGER,
    last_value        REAL,
    last_threshold    REAL,
    PRIMARY KEY (rule_id, host_id)
);

-- Periodic checkpoint of the in-memory per-host ring buffer the alert
-- engine evaluates over. On restart we rehydrate from this; rows whose
-- newest_ts is older than the longest active rule window are discarded so
-- a long downtime can't fire stale "resolved" notifications.
CREATE TABLE alert_series_snapshot (
    host_id      INTEGER PRIMARY KEY REFERENCES hosts(id) ON DELETE CASCADE,
    saved_at     INTEGER NOT NULL,
    newest_ts    INTEGER NOT NULL,
    samples_json TEXT    NOT NULL
);

CREATE TABLE audit_log (
    id             INTEGER PRIMARY KEY,
    ts             INTEGER NOT NULL,
    actor_user_id  INTEGER REFERENCES users(id) ON DELETE SET NULL,
    action         TEXT NOT NULL,
    target_kind    TEXT,
    target_id      INTEGER,
    payload        TEXT
);
CREATE INDEX idx_audit_ts ON audit_log(ts);

-- Per-user API tokens for Bearer authentication. `token_hash` is SHA-256 of
-- the plaintext token so a DB leak doesn't yield live tokens. Plaintext is
-- returned once on creation; we never store it.
CREATE TABLE api_tokens (
    id           INTEGER PRIMARY KEY,
    user_id      INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name         TEXT NOT NULL,
    token_hash   BLOB NOT NULL UNIQUE,
    created_at   INTEGER NOT NULL,
    expires_at   INTEGER,
    last_used_at INTEGER
);
CREATE INDEX idx_api_tokens_user ON api_tokens(user_id);

-- System-wide key/value settings. `value` is always a JSON literal so the
-- repo can deserialise into typed structs without changing the schema.
CREATE TABLE settings (
    key         TEXT PRIMARY KEY,
    value       TEXT NOT NULL,
    updated_at  INTEGER NOT NULL,
    updated_by  INTEGER REFERENCES users(id) ON DELETE SET NULL
);
INSERT INTO settings (key, value, updated_at) VALUES
    ('hzc.compactor_interval_secs', '3600', 0),
    ('hzc.retention_tiers', '[
        {"max_age_secs":604800,"resolution_secs":0},
        {"max_age_secs":2592000,"resolution_secs":300},
        {"max_age_secs":15552000,"resolution_secs":1800},
        {"max_age_secs":31536000,"resolution_secs":7200},
        {"max_age_secs":157680000,"resolution_secs":86400}
    ]', 0),
    ('runtime.worker_pool_size', '1024', 0);
