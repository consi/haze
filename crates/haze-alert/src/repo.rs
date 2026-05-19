//! Sqlx queries for the alerting subsystem.
//!
//! Both the engine and the API crate go through this module so the SQL
//! lives in exactly one place.

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::types::{Aggregation, Direction, Metric, Severity, TargetKind};

#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error("alert rule not found")]
    RuleNotFound,
    #[error("webhook not found")]
    WebhookNotFound,
    #[error("invalid stored value: {0}")]
    Decode(String),
}

// ─── Webhooks ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Webhook {
    pub id: i64,
    pub uuid: Uuid,
    pub name: String,
    pub url: String,
    /// Optional headers sent on every POST (auth tokens, content
    /// negotiation, etc.). Empty map = just the standard
    /// `Content-Type: application/json` reqwest adds for `.json()`.
    pub headers: Vec<(String, String)>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(sqlx::FromRow)]
struct WebhookRow {
    id: i64,
    uuid: Vec<u8>,
    name: String,
    url: String,
    headers: String,
    created_at: i64,
    updated_at: i64,
}

fn parse_headers(raw: &str) -> Vec<(String, String)> {
    // Stored as a JSON object; preserve insertion order via Vec so we
    // don't shuffle headers on each save.
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|v| {
            v.as_object().map(|obj| {
                obj.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect::<Vec<_>>()
            })
        })
        .unwrap_or_default()
}

fn serialize_headers(headers: &[(String, String)]) -> String {
    let mut map = serde_json::Map::with_capacity(headers.len());
    for (k, v) in headers {
        map.insert(k.clone(), serde_json::Value::String(v.clone()));
    }
    serde_json::to_string(&serde_json::Value::Object(map)).unwrap_or_else(|_| "{}".to_string())
}

impl TryFrom<WebhookRow> for Webhook {
    type Error = RepoError;
    fn try_from(r: WebhookRow) -> Result<Self, RepoError> {
        Ok(Self {
            id: r.id,
            uuid: Uuid::from_slice(&r.uuid)
                .map_err(|e| RepoError::Decode(format!("webhook uuid: {e}")))?,
            name: r.name,
            url: r.url,
            headers: parse_headers(&r.headers),
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
    }
}

pub async fn list_webhooks(pool: &SqlitePool) -> Result<Vec<Webhook>, RepoError> {
    let rows: Vec<WebhookRow> = sqlx::query_as(
        "SELECT id, uuid, name, url, headers, created_at, updated_at \
         FROM webhooks ORDER BY name, id",
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(Webhook::try_from).collect()
}

pub async fn get_webhook_by_uuid(
    pool: &SqlitePool,
    uuid: Uuid,
) -> Result<Option<Webhook>, RepoError> {
    let row: Option<WebhookRow> = sqlx::query_as(
        "SELECT id, uuid, name, url, headers, created_at, updated_at \
         FROM webhooks WHERE uuid = ?1",
    )
    .bind(uuid.as_bytes().to_vec())
    .fetch_optional(pool)
    .await?;
    row.map(Webhook::try_from).transpose()
}

pub async fn create_webhook(
    pool: &SqlitePool,
    name: &str,
    url: &str,
    headers: &[(String, String)],
) -> Result<Webhook, RepoError> {
    let uuid = Uuid::new_v4();
    let now = chrono::Utc::now().timestamp();
    let headers_json = serialize_headers(headers);
    sqlx::query(
        "INSERT INTO webhooks (uuid, name, url, headers, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
    )
    .bind(uuid.as_bytes().to_vec())
    .bind(name)
    .bind(url)
    .bind(&headers_json)
    .bind(now)
    .execute(pool)
    .await?;
    let id = sqlx::query_scalar::<_, i64>("SELECT last_insert_rowid()")
        .fetch_one(pool)
        .await?;
    Ok(Webhook {
        id,
        uuid,
        name: name.to_string(),
        url: url.to_string(),
        headers: headers.to_vec(),
        created_at: now,
        updated_at: now,
    })
}

pub async fn update_webhook(
    pool: &SqlitePool,
    uuid: Uuid,
    name: &str,
    url: &str,
    headers: &[(String, String)],
) -> Result<Webhook, RepoError> {
    let now = chrono::Utc::now().timestamp();
    let headers_json = serialize_headers(headers);
    let rows = sqlx::query(
        "UPDATE webhooks SET name = ?1, url = ?2, headers = ?3, updated_at = ?4 \
         WHERE uuid = ?5",
    )
    .bind(name)
    .bind(url)
    .bind(&headers_json)
    .bind(now)
    .bind(uuid.as_bytes().to_vec())
    .execute(pool)
    .await?
    .rows_affected();
    if rows == 0 {
        return Err(RepoError::WebhookNotFound);
    }
    get_webhook_by_uuid(pool, uuid)
        .await?
        .ok_or(RepoError::WebhookNotFound)
}

/// Names of every rule referencing this webhook. Used by the API to reject
/// deletes that would leave dangling references.
pub async fn rules_referencing_webhook(
    pool: &SqlitePool,
    webhook_id: i64,
) -> Result<Vec<String>, RepoError> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT r.name FROM alert_rules r \
         JOIN alert_rule_webhooks w ON w.rule_id = r.id \
         WHERE w.webhook_id = ?1 ORDER BY r.name",
    )
    .bind(webhook_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(n,)| n).collect())
}

pub async fn delete_webhook(pool: &SqlitePool, uuid: Uuid) -> Result<(), RepoError> {
    let rows = sqlx::query("DELETE FROM webhooks WHERE uuid = ?1")
        .bind(uuid.as_bytes().to_vec())
        .execute(pool)
        .await?
        .rows_affected();
    if rows == 0 {
        return Err(RepoError::WebhookNotFound);
    }
    Ok(())
}

// ─── Alert rules ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleTarget {
    pub kind: TargetKind,
    pub uuid: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertRule {
    pub id: i64,
    pub uuid: Uuid,
    pub name: String,
    pub enabled: bool,
    pub metric: Metric,
    pub aggregation: Aggregation,
    pub direction: Direction,
    pub warning_threshold: Option<f32>,
    pub critical_threshold: Option<f32>,
    pub window_secs: i64,
    pub targets: Vec<RuleTarget>,
    pub webhook_uuids: Vec<Uuid>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(sqlx::FromRow)]
struct RuleRow {
    id: i64,
    uuid: Vec<u8>,
    name: String,
    enabled: i64,
    metric: String,
    aggregation: String,
    direction: String,
    warning_threshold: Option<f64>,
    critical_threshold: Option<f64>,
    window_secs: i64,
    created_at: i64,
    updated_at: i64,
}

async fn hydrate_rule(pool: &SqlitePool, r: RuleRow) -> Result<AlertRule, RepoError> {
    let uuid =
        Uuid::from_slice(&r.uuid).map_err(|e| RepoError::Decode(format!("rule uuid: {e}")))?;
    let metric = Metric::parse(&r.metric)
        .ok_or_else(|| RepoError::Decode(format!("metric '{}'", r.metric)))?;
    let aggregation = Aggregation::parse(&r.aggregation)
        .ok_or_else(|| RepoError::Decode(format!("aggregation '{}'", r.aggregation)))?;
    let direction = Direction::parse(&r.direction)
        .ok_or_else(|| RepoError::Decode(format!("direction '{}'", r.direction)))?;

    let target_rows: Vec<(String, i64)> =
        sqlx::query_as("SELECT target_kind, target_id FROM alert_rule_targets WHERE rule_id = ?1")
            .bind(r.id)
            .fetch_all(pool)
            .await?;
    let mut targets = Vec::with_capacity(target_rows.len());
    for (kind, id) in target_rows {
        let kind_typed = TargetKind::parse(&kind)
            .ok_or_else(|| RepoError::Decode(format!("target_kind '{kind}'")))?;
        let uuid_bytes: Option<(Vec<u8>,)> = match kind_typed {
            TargetKind::Host => {
                sqlx::query_as("SELECT uuid FROM hosts WHERE id = ?1")
                    .bind(id)
                    .fetch_optional(pool)
                    .await?
            }
            TargetKind::Group => {
                sqlx::query_as("SELECT uuid FROM groups WHERE id = ?1")
                    .bind(id)
                    .fetch_optional(pool)
                    .await?
            }
        };
        // Skip targets that have been deleted out from under the rule;
        // they'll get pruned the next time the rule is updated through
        // the API.
        if let Some((bytes,)) = uuid_bytes {
            if let Ok(t_uuid) = Uuid::from_slice(&bytes) {
                targets.push(RuleTarget {
                    kind: kind_typed,
                    uuid: t_uuid,
                });
            }
        }
    }

    let webhook_rows: Vec<(Vec<u8>,)> = sqlx::query_as(
        "SELECT w.uuid FROM webhooks w \
         JOIN alert_rule_webhooks rw ON rw.webhook_id = w.id \
         WHERE rw.rule_id = ?1 ORDER BY w.name",
    )
    .bind(r.id)
    .fetch_all(pool)
    .await?;
    let mut webhook_uuids = Vec::with_capacity(webhook_rows.len());
    for (bytes,) in webhook_rows {
        webhook_uuids.push(
            Uuid::from_slice(&bytes)
                .map_err(|e| RepoError::Decode(format!("webhook uuid: {e}")))?,
        );
    }

    Ok(AlertRule {
        id: r.id,
        uuid,
        name: r.name,
        enabled: r.enabled != 0,
        metric,
        aggregation,
        direction,
        warning_threshold: r.warning_threshold.map(|v| v as f32),
        critical_threshold: r.critical_threshold.map(|v| v as f32),
        window_secs: r.window_secs,
        targets,
        webhook_uuids,
        created_at: r.created_at,
        updated_at: r.updated_at,
    })
}

pub async fn list_rules(pool: &SqlitePool) -> Result<Vec<AlertRule>, RepoError> {
    let rows: Vec<RuleRow> = sqlx::query_as(
        "SELECT id, uuid, name, enabled, metric, aggregation, direction, \
                warning_threshold, critical_threshold, window_secs, \
                created_at, updated_at \
         FROM alert_rules ORDER BY name, id",
    )
    .fetch_all(pool)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        out.push(hydrate_rule(pool, r).await?);
    }
    Ok(out)
}

pub async fn get_rule_by_uuid(
    pool: &SqlitePool,
    uuid: Uuid,
) -> Result<Option<AlertRule>, RepoError> {
    let row: Option<RuleRow> = sqlx::query_as(
        "SELECT id, uuid, name, enabled, metric, aggregation, direction, \
                warning_threshold, critical_threshold, window_secs, \
                created_at, updated_at \
         FROM alert_rules WHERE uuid = ?1",
    )
    .bind(uuid.as_bytes().to_vec())
    .fetch_optional(pool)
    .await?;
    match row {
        Some(r) => Ok(Some(hydrate_rule(pool, r).await?)),
        None => Ok(None),
    }
}

/// Internal projection used by the engine's hot path: every enabled rule
/// with its expanded host-id set already computed. Avoids hitting the DB
/// per-host inside the eval cycle.
#[derive(Debug)]
pub struct EnabledRule {
    pub id: i64,
    pub uuid: Uuid,
    pub name: String,
    pub metric: Metric,
    pub aggregation: Aggregation,
    pub direction: Direction,
    pub warning_threshold: Option<f32>,
    pub critical_threshold: Option<f32>,
    pub window_secs: i64,
    /// `(host_id, host_uuid)` pairs - already deduplicated across mixed targets.
    pub hosts: Vec<(i64, Uuid)>,
    /// `(webhook_uuid, url, headers)` triples - inlined here so the eval
    /// hot path doesn't re-query webhooks for every transition.
    pub webhook_urls: Vec<WebhookTarget>,
}

/// One webhook the engine will POST to: its UUID (for logging), the
/// destination URL, and any custom headers to attach.
pub type WebhookTarget = (Uuid, String, Vec<(String, String)>);

/// Load every enabled rule with mixed targets expanded to a dedup'd host
/// set, and webhook urls inlined. The whole eval cycle works off the
/// result of this single query batch.
pub async fn load_enabled_rules(pool: &SqlitePool) -> Result<Vec<EnabledRule>, RepoError> {
    let rules = list_rules(pool).await?;
    let mut out = Vec::new();
    for rule in rules {
        if !rule.enabled {
            continue;
        }
        let hosts = expand_targets_to_hosts(pool, &rule).await?;
        let webhook_urls = fetch_webhook_urls(pool, rule.id).await?;
        out.push(EnabledRule {
            id: rule.id,
            uuid: rule.uuid,
            name: rule.name,
            metric: rule.metric,
            aggregation: rule.aggregation,
            direction: rule.direction,
            warning_threshold: rule.warning_threshold,
            critical_threshold: rule.critical_threshold,
            window_secs: rule.window_secs,
            hosts,
            webhook_urls,
        });
    }
    Ok(out)
}

async fn fetch_webhook_urls(
    pool: &SqlitePool,
    rule_id: i64,
) -> Result<Vec<WebhookTarget>, RepoError> {
    let rows: Vec<(Vec<u8>, String, String)> = sqlx::query_as(
        "SELECT w.uuid, w.url, w.headers FROM webhooks w \
         JOIN alert_rule_webhooks rw ON rw.webhook_id = w.id \
         WHERE rw.rule_id = ?1",
    )
    .bind(rule_id)
    .fetch_all(pool)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for (uuid_bytes, url, headers_raw) in rows {
        out.push((
            Uuid::from_slice(&uuid_bytes)
                .map_err(|e| RepoError::Decode(format!("webhook uuid: {e}")))?,
            url,
            parse_headers(&headers_raw),
        ));
    }
    Ok(out)
}

async fn expand_targets_to_hosts(
    pool: &SqlitePool,
    rule: &AlertRule,
) -> Result<Vec<(i64, Uuid)>, RepoError> {
    let mut host_ids: Vec<i64> = Vec::new();
    for t in &rule.targets {
        match t.kind {
            TargetKind::Host => {
                let row: Option<(i64,)> =
                    sqlx::query_as("SELECT id FROM hosts WHERE uuid = ?1 AND enabled = 1")
                        .bind(t.uuid.as_bytes().to_vec())
                        .fetch_optional(pool)
                        .await?;
                if let Some((id,)) = row {
                    host_ids.push(id);
                }
            }
            TargetKind::Group => {
                // Materialized-path subtree expansion: every host that
                // belongs (directly) to the target group OR any descendant.
                let rows: Vec<(i64,)> = sqlx::query_as(
                    "SELECT DISTINCT h.id FROM hosts h \
                     JOIN host_groups hg ON hg.host_id = h.id \
                     JOIN groups g ON hg.group_id = g.id \
                     WHERE h.enabled = 1 AND g.path LIKE ( \
                         SELECT path || '%' FROM groups WHERE uuid = ?1 \
                     )",
                )
                .bind(t.uuid.as_bytes().to_vec())
                .fetch_all(pool)
                .await?;
                host_ids.extend(rows.into_iter().map(|(id,)| id));
            }
        }
    }
    host_ids.sort_unstable();
    host_ids.dedup();

    let mut out = Vec::with_capacity(host_ids.len());
    for id in host_ids {
        let row: Option<(Vec<u8>,)> = sqlx::query_as("SELECT uuid FROM hosts WHERE id = ?1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
        if let Some((bytes,)) = row {
            if let Ok(u) = Uuid::from_slice(&bytes) {
                out.push((id, u));
            }
        }
    }
    Ok(out)
}

#[derive(Debug, Clone)]
pub struct NewRule<'a> {
    pub name: &'a str,
    pub enabled: bool,
    pub metric: Metric,
    pub aggregation: Aggregation,
    pub direction: Direction,
    pub warning_threshold: Option<f32>,
    pub critical_threshold: Option<f32>,
    pub window_secs: i64,
    pub targets: &'a [RuleTarget],
    pub webhook_uuids: &'a [Uuid],
}

#[derive(Debug, thiserror::Error)]
pub enum CreateRuleError {
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error("target host {0} not found")]
    HostNotFound(Uuid),
    #[error("target group {0} not found")]
    GroupNotFound(Uuid),
    #[error("webhook {0} not found")]
    WebhookNotFound(Uuid),
    #[error(transparent)]
    Repo(#[from] RepoError),
}

pub async fn create_rule(
    pool: &SqlitePool,
    new: NewRule<'_>,
) -> Result<AlertRule, CreateRuleError> {
    let uuid = Uuid::new_v4();
    let now = chrono::Utc::now().timestamp();

    let mut tx = pool.begin().await?;

    sqlx::query(
        "INSERT INTO alert_rules \
            (uuid, name, enabled, metric, aggregation, direction, \
             warning_threshold, critical_threshold, window_secs, \
             created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
    )
    .bind(uuid.as_bytes().to_vec())
    .bind(new.name)
    .bind(i64::from(new.enabled))
    .bind(new.metric.as_str())
    .bind(new.aggregation.as_str())
    .bind(new.direction.as_str())
    .bind(new.warning_threshold.map(f64::from))
    .bind(new.critical_threshold.map(f64::from))
    .bind(new.window_secs)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    let rule_id = sqlx::query_scalar::<_, i64>("SELECT last_insert_rowid()")
        .fetch_one(&mut *tx)
        .await?;

    set_targets_tx(&mut tx, rule_id, new.targets).await?;
    set_webhooks_tx(&mut tx, rule_id, new.webhook_uuids).await?;

    tx.commit().await?;

    let rule = get_rule_by_uuid(pool, uuid)
        .await?
        .ok_or(RepoError::RuleNotFound)?;
    Ok(rule)
}

#[derive(Debug, Clone)]
pub struct UpdateRule<'a> {
    pub name: &'a str,
    pub enabled: bool,
    pub metric: Metric,
    pub aggregation: Aggregation,
    pub direction: Direction,
    pub warning_threshold: Option<f32>,
    pub critical_threshold: Option<f32>,
    pub window_secs: i64,
    pub targets: &'a [RuleTarget],
    pub webhook_uuids: &'a [Uuid],
}

pub async fn update_rule(
    pool: &SqlitePool,
    uuid: Uuid,
    upd: UpdateRule<'_>,
) -> Result<AlertRule, CreateRuleError> {
    let now = chrono::Utc::now().timestamp();
    let mut tx = pool.begin().await?;

    let row: Option<(i64,)> = sqlx::query_as("SELECT id FROM alert_rules WHERE uuid = ?1")
        .bind(uuid.as_bytes().to_vec())
        .fetch_optional(&mut *tx)
        .await?;
    let Some((rule_id,)) = row else {
        return Err(CreateRuleError::Repo(RepoError::RuleNotFound));
    };

    sqlx::query(
        "UPDATE alert_rules SET \
            name = ?1, enabled = ?2, metric = ?3, aggregation = ?4, direction = ?5, \
            warning_threshold = ?6, critical_threshold = ?7, window_secs = ?8, \
            updated_at = ?9 \
         WHERE id = ?10",
    )
    .bind(upd.name)
    .bind(i64::from(upd.enabled))
    .bind(upd.metric.as_str())
    .bind(upd.aggregation.as_str())
    .bind(upd.direction.as_str())
    .bind(upd.warning_threshold.map(f64::from))
    .bind(upd.critical_threshold.map(f64::from))
    .bind(upd.window_secs)
    .bind(now)
    .bind(rule_id)
    .execute(&mut *tx)
    .await?;

    sqlx::query("DELETE FROM alert_rule_targets WHERE rule_id = ?1")
        .bind(rule_id)
        .execute(&mut *tx)
        .await?;
    set_targets_tx(&mut tx, rule_id, upd.targets).await?;

    sqlx::query("DELETE FROM alert_rule_webhooks WHERE rule_id = ?1")
        .bind(rule_id)
        .execute(&mut *tx)
        .await?;
    set_webhooks_tx(&mut tx, rule_id, upd.webhook_uuids).await?;

    tx.commit().await?;

    get_rule_by_uuid(pool, uuid)
        .await?
        .ok_or(RepoError::RuleNotFound)
        .map_err(CreateRuleError::Repo)
}

pub async fn delete_rule(pool: &SqlitePool, uuid: Uuid) -> Result<(), RepoError> {
    let rows = sqlx::query("DELETE FROM alert_rules WHERE uuid = ?1")
        .bind(uuid.as_bytes().to_vec())
        .execute(pool)
        .await?
        .rows_affected();
    if rows == 0 {
        return Err(RepoError::RuleNotFound);
    }
    Ok(())
}

async fn set_targets_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    rule_id: i64,
    targets: &[RuleTarget],
) -> Result<(), CreateRuleError> {
    // Dedup so repeated input doesn't blow up the PK.
    let mut seen: Vec<(TargetKind, Uuid)> = Vec::with_capacity(targets.len());
    for t in targets {
        if !seen.iter().any(|(k, u)| *k == t.kind && *u == t.uuid) {
            seen.push((t.kind, t.uuid));
        }
    }
    for (kind, uuid) in seen {
        let id_row: Option<(i64,)> = match kind {
            TargetKind::Host => {
                sqlx::query_as("SELECT id FROM hosts WHERE uuid = ?1")
                    .bind(uuid.as_bytes().to_vec())
                    .fetch_optional(&mut **tx)
                    .await?
            }
            TargetKind::Group => {
                sqlx::query_as("SELECT id FROM groups WHERE uuid = ?1")
                    .bind(uuid.as_bytes().to_vec())
                    .fetch_optional(&mut **tx)
                    .await?
            }
        };
        let Some((target_id,)) = id_row else {
            return Err(match kind {
                TargetKind::Host => CreateRuleError::HostNotFound(uuid),
                TargetKind::Group => CreateRuleError::GroupNotFound(uuid),
            });
        };
        sqlx::query(
            "INSERT INTO alert_rule_targets (rule_id, target_kind, target_id) \
             VALUES (?1, ?2, ?3)",
        )
        .bind(rule_id)
        .bind(kind.as_str())
        .bind(target_id)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

async fn set_webhooks_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    rule_id: i64,
    webhook_uuids: &[Uuid],
) -> Result<(), CreateRuleError> {
    let mut seen: Vec<Uuid> = Vec::with_capacity(webhook_uuids.len());
    for u in webhook_uuids {
        if !seen.contains(u) {
            seen.push(*u);
        }
    }
    for u in seen {
        let row: Option<(i64,)> = sqlx::query_as("SELECT id FROM webhooks WHERE uuid = ?1")
            .bind(u.as_bytes().to_vec())
            .fetch_optional(&mut **tx)
            .await?;
        let Some((webhook_id,)) = row else {
            return Err(CreateRuleError::WebhookNotFound(u));
        };
        sqlx::query("INSERT INTO alert_rule_webhooks (rule_id, webhook_id) VALUES (?1, ?2)")
            .bind(rule_id)
            .bind(webhook_id)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

// ─── Alert state ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertState {
    pub rule_id: i64,
    pub rule_uuid: Uuid,
    pub host_id: i64,
    pub host_uuid: Uuid,
    pub severity: Severity,
    pub since: i64,
    pub last_notified_at: Option<i64>,
    /// Value of `aggregation(metric)` at the last transition. Lets the UI
    /// surface *why* a rule is currently firing without re-evaluating.
    pub last_value: Option<f32>,
    /// Threshold the value was compared against (warning or critical,
    /// whichever drove the current severity).
    pub last_threshold: Option<f32>,
}

type StateRow = (
    i64,
    Vec<u8>,
    i64,
    Vec<u8>,
    String,
    i64,
    Option<i64>,
    Option<f64>,
    Option<f64>,
);

pub async fn list_states(pool: &SqlitePool) -> Result<Vec<AlertState>, RepoError> {
    let rows: Vec<StateRow> = sqlx::query_as(
        "SELECT s.rule_id, r.uuid, s.host_id, h.uuid, s.severity, s.since, \
                s.last_notified_at, s.last_value, s.last_threshold \
         FROM alert_state s \
         JOIN alert_rules r ON r.id = s.rule_id \
         JOIN hosts h ON h.id = s.host_id \
         ORDER BY r.name, h.display_name",
    )
    .fetch_all(pool)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for (
        rule_id,
        rule_uuid,
        host_id,
        host_uuid,
        severity,
        since,
        last_notified_at,
        last_value,
        last_threshold,
    ) in rows
    {
        out.push(AlertState {
            rule_id,
            rule_uuid: Uuid::from_slice(&rule_uuid)
                .map_err(|e| RepoError::Decode(format!("rule uuid: {e}")))?,
            host_id,
            host_uuid: Uuid::from_slice(&host_uuid)
                .map_err(|e| RepoError::Decode(format!("host uuid: {e}")))?,
            severity: Severity::parse(&severity)
                .ok_or_else(|| RepoError::Decode(format!("severity '{severity}'")))?,
            since,
            last_notified_at,
            last_value: last_value.map(|v| v as f32),
            last_threshold: last_threshold.map(|v| v as f32),
        });
    }
    Ok(out)
}

pub async fn current_state(
    pool: &SqlitePool,
    rule_id: i64,
    host_id: i64,
) -> Result<Severity, RepoError> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT severity FROM alert_state WHERE rule_id = ?1 AND host_id = ?2")
            .bind(rule_id)
            .bind(host_id)
            .fetch_optional(pool)
            .await?;
    match row {
        Some((s,)) => {
            Severity::parse(&s).ok_or_else(|| RepoError::Decode(format!("severity '{s}'")))
        }
        None => Ok(Severity::Ok),
    }
}

/// Slim projection of every persisted `(rule_id, host_id) → severity` pair.
///
/// Used at engine startup to hydrate the in-memory state cache so the
/// per-host eval loop never has to round-trip to `SQLite` for the prior
/// severity.
pub async fn load_state_cache(
    pool: &SqlitePool,
) -> Result<std::collections::HashMap<(i64, i64), Severity>, RepoError> {
    let rows: Vec<(i64, i64, String)> =
        sqlx::query_as("SELECT rule_id, host_id, severity FROM alert_state")
            .fetch_all(pool)
            .await?;
    let mut map = std::collections::HashMap::with_capacity(rows.len());
    for (rule_id, host_id, severity) in rows {
        let sev = Severity::parse(&severity)
            .ok_or_else(|| RepoError::Decode(format!("severity '{severity}'")))?;
        map.insert((rule_id, host_id), sev);
    }
    Ok(map)
}

/// Every `alert_state` row that is currently firing (severity != ok).
/// Used by the engine's reconciliation pass to find pairs that should be
/// resolved because the host no longer matches the rule.
pub async fn list_non_ok_state(pool: &SqlitePool) -> Result<Vec<AlertState>, RepoError> {
    let rows: Vec<StateRow> = sqlx::query_as(
        "SELECT s.rule_id, r.uuid, s.host_id, h.uuid, s.severity, s.since, \
                s.last_notified_at, s.last_value, s.last_threshold \
         FROM alert_state s \
         JOIN alert_rules r ON r.id = s.rule_id \
         JOIN hosts h ON h.id = s.host_id \
         WHERE s.severity != 'ok'",
    )
    .fetch_all(pool)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for (
        rule_id,
        rule_uuid,
        host_id,
        host_uuid,
        severity,
        since,
        last_notified_at,
        last_value,
        last_threshold,
    ) in rows
    {
        out.push(AlertState {
            rule_id,
            rule_uuid: Uuid::from_slice(&rule_uuid)
                .map_err(|e| RepoError::Decode(format!("rule uuid: {e}")))?,
            host_id,
            host_uuid: Uuid::from_slice(&host_uuid)
                .map_err(|e| RepoError::Decode(format!("host uuid: {e}")))?,
            severity: Severity::parse(&severity)
                .ok_or_else(|| RepoError::Decode(format!("severity '{severity}'")))?,
            since,
            last_notified_at,
            last_value: last_value.map(|v| v as f32),
            last_threshold: last_threshold.map(|v| v as f32),
        });
    }
    Ok(out)
}

/// Drop a state row entirely.
///
/// Used when the `(rule, host)` pairing has dissolved (host left a
/// targeted group, rule's targets edited, rule disabled, host disabled);
/// keeping a stale "ok" row around would just mean reconciliation
/// re-evaluates it forever.
pub async fn delete_state(pool: &SqlitePool, rule_id: i64, host_id: i64) -> Result<(), RepoError> {
    sqlx::query("DELETE FROM alert_state WHERE rule_id = ?1 AND host_id = ?2")
        .bind(rule_id)
        .bind(host_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Slim host metadata the engine needs to enrich webhook payloads.
///
/// `probe_config` stays as the stored JSON string - the caller decides
/// whether to parse it into a nested object or fall back to the raw form.
pub struct HostMeta {
    pub uuid: Uuid,
    pub display_name: String,
    pub probe_type: String,
    pub probe_config: String,
}

pub async fn host_meta_by_id(
    pool: &SqlitePool,
    host_id: i64,
) -> Result<Option<HostMeta>, RepoError> {
    let row: Option<(Vec<u8>, String, String, String)> = sqlx::query_as(
        "SELECT uuid, display_name, probe_type, probe_config \
         FROM hosts WHERE id = ?1",
    )
    .bind(host_id)
    .fetch_optional(pool)
    .await?;
    let Some((uuid_bytes, display_name, probe_type, probe_config)) = row else {
        return Ok(None);
    };
    Ok(Some(HostMeta {
        uuid: Uuid::from_slice(&uuid_bytes)
            .map_err(|e| RepoError::Decode(format!("host uuid: {e}")))?,
        display_name,
        probe_type,
        probe_config,
    }))
}

/// Load a rule (including disabled ones) shaped as `EnabledRule`.
///
/// `hosts` is left empty - only the metadata + `webhook_urls` are
/// needed. Used by reconciliation when a previously-firing rule has
/// been disabled or its targets edited; we still need to deliver the
/// resolve webhook against the rule's last-known wiring.
pub async fn load_rule_for_notify(
    pool: &SqlitePool,
    rule_id: i64,
) -> Result<Option<EnabledRule>, RepoError> {
    let row: Option<RuleRow> = sqlx::query_as(
        "SELECT id, uuid, name, enabled, metric, aggregation, direction, \
                warning_threshold, critical_threshold, window_secs, \
                created_at, updated_at \
         FROM alert_rules WHERE id = ?1",
    )
    .bind(rule_id)
    .fetch_optional(pool)
    .await?;
    let Some(r) = row else { return Ok(None) };
    let rule = hydrate_rule(pool, r).await?;
    let webhook_urls = fetch_webhook_urls(pool, rule.id).await?;
    Ok(Some(EnabledRule {
        id: rule.id,
        uuid: rule.uuid,
        name: rule.name,
        metric: rule.metric,
        aggregation: rule.aggregation,
        direction: rule.direction,
        warning_threshold: rule.warning_threshold,
        critical_threshold: rule.critical_threshold,
        window_secs: rule.window_secs,
        hosts: Vec::new(),
        webhook_urls,
    }))
}

pub async fn upsert_state(
    pool: &SqlitePool,
    rule_id: i64,
    host_id: i64,
    severity: Severity,
    value: Option<f32>,
    threshold: Option<f32>,
    now: i64,
) -> Result<(), RepoError> {
    sqlx::query(
        "INSERT INTO alert_state (rule_id, host_id, severity, since, \
                                  last_notified_at, last_value, last_threshold) \
         VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6) \
         ON CONFLICT(rule_id, host_id) DO UPDATE SET \
             severity = excluded.severity, \
             since = excluded.since, \
             last_notified_at = excluded.last_notified_at, \
             last_value = excluded.last_value, \
             last_threshold = excluded.last_threshold",
    )
    .bind(rule_id)
    .bind(host_id)
    .bind(severity.as_str())
    .bind(now)
    .bind(value.map(f64::from))
    .bind(threshold.map(f64::from))
    .execute(pool)
    .await?;
    Ok(())
}

// ─── Series snapshot ────────────────────────────────────────────────────────

/// Persist a per-host series snapshot. `samples_json` is the closed-form
/// JSON shape `[[ts, [min, p2_5, p25, median, p75, p97_5, loss_pct]], ...]`.
pub async fn upsert_series_snapshot(
    pool: &SqlitePool,
    host_id: i64,
    saved_at: i64,
    newest_ts: i64,
    samples_json: &str,
) -> Result<(), RepoError> {
    sqlx::query(
        "INSERT INTO alert_series_snapshot (host_id, saved_at, newest_ts, samples_json) \
         VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT(host_id) DO UPDATE SET \
             saved_at = excluded.saved_at, \
             newest_ts = excluded.newest_ts, \
             samples_json = excluded.samples_json",
    )
    .bind(host_id)
    .bind(saved_at)
    .bind(newest_ts)
    .bind(samples_json)
    .execute(pool)
    .await?;
    Ok(())
}

pub struct SnapshotRow {
    pub host_id: i64,
    pub host_uuid: Uuid,
    pub newest_ts: i64,
    pub samples_json: String,
}

pub async fn list_series_snapshots(pool: &SqlitePool) -> Result<Vec<SnapshotRow>, RepoError> {
    let rows: Vec<(i64, Vec<u8>, i64, String)> = sqlx::query_as(
        "SELECT s.host_id, h.uuid, s.newest_ts, s.samples_json \
         FROM alert_series_snapshot s \
         JOIN hosts h ON h.id = s.host_id",
    )
    .fetch_all(pool)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for (host_id, uuid, newest_ts, samples_json) in rows {
        out.push(SnapshotRow {
            host_id,
            host_uuid: Uuid::from_slice(&uuid)
                .map_err(|e| RepoError::Decode(format!("host uuid: {e}")))?,
            newest_ts,
            samples_json,
        });
    }
    Ok(out)
}

/// Resolve a host UUID to its DB id. Used by the engine to translate
/// series-store reads back into the integer keys the state table uses.
pub async fn host_id_for_uuid(pool: &SqlitePool, uuid: Uuid) -> Result<Option<i64>, RepoError> {
    let row: Option<(i64,)> = sqlx::query_as("SELECT id FROM hosts WHERE uuid = ?1")
        .bind(uuid.as_bytes().to_vec())
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|(id,)| id))
}
