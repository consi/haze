//! Alert evaluation loop.
//!
//! Single tokio task that ticks every `EVAL_INTERVAL` and runs every
//! enabled rule. Each `(rule, host)` evaluation:
//!   1. Slices the in-memory series for the rule's window.
//!   2. Extracts the chosen Metric field per sample (dropping NaN).
//!   3. Reduces the values with the chosen Aggregation.
//!   4. Classifies the result into Ok/Warning/Critical with the rule's
//!      direction + thresholds.
//!   5. Compares against the persisted state; on any transition, upsert
//!      the new state and fire every wired webhook.
//!
//! Before fanning out per-host evaluations, each cycle also runs a
//! **reconciliation pass**: any `alert_state` row still firing for a
//! `(rule, host)` pair that no longer matches the rule (host removed
//! from a targeted group, rule's targets edited, rule disabled, host
//! disabled) gets a synthetic resolve webhook with
//! `reason: "match_lost"` and the state row is deleted. Host *deletion*
//! is not covered — `ON DELETE CASCADE` removes the state row before we
//! can see it; intentional limitation for the simpler design.
//!
//! All disk I/O is sqlx; no HZC reads happen on the hot path — the
//! probes feed the `SeriesStore`, and the engine reads only from RAM.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use futures::StreamExt;
use haze_store::{SeriesStore, repo::settings};
use parking_lot::RwLock;
use sqlx::SqlitePool;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::{
    repo::{self, EnabledRule},
    types::{ResolveReason, Severity, classify},
    webhooks::WebhookClient,
};

/// Extra seconds added past the longest rule window when sizing the
/// in-memory buffer. Gives the evaluator some slack so a rule with a
/// 5-minute window can read 5 minutes of data even after a slow tick.
const SERIES_WINDOW_GRACE: i64 = 60;

pub struct AlertEngine {
    pool: SqlitePool,
    series: Arc<SeriesStore>,
    eval_concurrency: usize,
    /// Wrapped in `RwLock` so we can swap the underlying `reqwest::Client`
    /// when the operator changes `webhook_timeout_secs` in `/settings`,
    /// without rebuilding the whole engine.
    http: RwLock<WebhookClient>,
    /// Mirrors `alert_state` so the per-host eval loop reads the prior
    /// severity from RAM instead of round-tripping to `SQLite` for every
    /// (rule, host) pair every cycle. Engine is the sole writer of the
    /// underlying table, so this map stays authoritative as long as
    /// every `upsert_state` / `delete_state` is paired with a cache
    /// update. Cache miss is treated as `Severity::Ok`, matching
    /// `repo::current_state`'s "no row" semantics.
    state_cache: RwLock<HashMap<(i64, i64), Severity>>,
}

impl AlertEngine {
    pub async fn new(
        pool: SqlitePool,
        series: Arc<SeriesStore>,
        eval_concurrency: usize,
    ) -> Result<Self, repo::RepoError> {
        let state_cache = repo::load_state_cache(&pool).await?;
        Ok(Self {
            pool,
            series,
            eval_concurrency: eval_concurrency.max(1),
            http: RwLock::new(WebhookClient::new()),
            state_cache: RwLock::new(state_cache),
        })
    }

    /// Spawn-and-forget loop. Sleeps for `alerting.eval_interval_secs`
    /// between cycles — value is re-read each iteration so changes in
    /// /settings/alerting take effect on the next tick without a restart.
    pub fn run(self) -> tokio::task::JoinHandle<()> {
        let engine = Arc::new(self);
        tokio::spawn(async move {
            loop {
                let cfg = settings::alerting_settings(&engine.pool)
                    .await
                    .unwrap_or_else(|_| settings::default_alerting_settings());

                // Rebuild the webhook client when the operator changes
                // the timeout. Cheap (reqwest::Client is Arc-backed); we
                // only swap when the value changed to keep connection
                // pooling effective in steady state.
                let cur_timeout = engine.http.read().timeout_secs();
                if cur_timeout != cfg.webhook_timeout_secs {
                    *engine.http.write() = WebhookClient::with_timeout(cfg.webhook_timeout_secs);
                }

                tokio::time::sleep(Duration::from_secs(u64::from(
                    cfg.eval_interval_secs.max(1),
                )))
                .await;

                if let Err(e) = engine.evaluate_cycle().await {
                    warn!(error = ?e, "alert evaluation cycle failed");
                }
            }
        })
    }

    /// One evaluation pass. Reload rules, size the series buffer to fit
    /// the longest window, run reconciliation against persisted state,
    /// then fan out per-host evaluations on the `alert_eval` semaphore.
    pub async fn evaluate_cycle(self: &Arc<Self>) -> anyhow::Result<()> {
        let rules = repo::load_enabled_rules(&self.pool).await?;

        // Buffer cap: longest active window + grace, so a rule with a
        // 10-min window has 11 minutes of headroom in memory.
        let max_window = rules.iter().map(|r| r.window_secs).max().unwrap_or(0);
        let cap = max_window + SERIES_WINDOW_GRACE;
        self.series.set_max_age_secs(cap.max(SERIES_WINDOW_GRACE));

        // Reconcile *before* fanning out: any (rule, host) pair persisted
        // as firing but no longer present in the current match set gets
        // a `match_lost` resolve. Runs even when `rules` is empty so a
        // rule whose every target was just removed still drains.
        let match_set: HashSet<(i64, i64)> = rules
            .iter()
            .flat_map(|r| r.hosts.iter().map(move |(hid, _)| (r.id, *hid)))
            .collect();
        if let Err(e) = self.reconcile_lost_matches(&match_set).await {
            warn!(error = ?e, "alert reconciliation pass failed");
        }

        if rules.is_empty() {
            debug!("alert evaluator: no enabled rules");
            return Ok(());
        }

        // Flatten (rule, host_id, host_uuid) triples, then drive them
        // through `for_each_concurrent`. At M × N pairs this saves M × N
        // task allocations per cycle vs the old `tokio::spawn`-per-pair
        // pattern; the concurrency cap stays the same.
        let pairs: Vec<(Arc<EnabledRule>, i64, Uuid)> = rules
            .into_iter()
            .flat_map(|rule| {
                let hosts = rule.hosts.clone();
                let rule = Arc::new(rule);
                hosts
                    .into_iter()
                    .map(move |(hid, hu)| (rule.clone(), hid, hu))
            })
            .collect();

        let engine = self.clone();
        futures::stream::iter(pairs)
            .for_each_concurrent(self.eval_concurrency, |(rule, host_id, host_uuid)| {
                let engine = engine.clone();
                async move {
                    if let Err(e) = engine.evaluate_one(&rule, host_id, host_uuid).await {
                        warn!(
                            rule_uuid = %rule.uuid,
                            host_uuid = %host_uuid,
                            error = ?e,
                            "alert rule evaluation failed"
                        );
                    }
                }
            })
            .await;
        Ok(())
    }

    async fn evaluate_one(
        &self,
        rule: &EnabledRule,
        host_id: i64,
        host_uuid: Uuid,
    ) -> anyhow::Result<()> {
        let now = chrono::Utc::now().timestamp();
        let from = now - rule.window_secs;
        let samples = self.series.slice(host_uuid, from, now);
        if samples.is_empty() {
            // Missing-data policy: leave state alone. The probe might
            // still be ramping up, or the host might have been started
            // mid-window. Firing "no data" alerts is a future feature.
            debug!(
                rule_uuid = %rule.uuid,
                host_uuid = %host_uuid,
                "no samples in window, skipping evaluation"
            );
            return Ok(());
        }

        let values: Vec<f32> = samples
            .iter()
            .map(|(_, slot)| rule.metric.extract(slot))
            .filter(|v| !v.is_nan())
            .collect();
        let Some(value) = rule.aggregation.apply(&values) else {
            // Every sample was NaN. Same policy as empty: leave state alone.
            return Ok(());
        };

        let new_severity = classify(
            value,
            rule.direction,
            rule.warning_threshold,
            rule.critical_threshold,
        );
        let current = self
            .state_cache
            .read()
            .get(&(rule.id, host_id))
            .copied()
            .unwrap_or(Severity::Ok);

        if new_severity == current {
            return Ok(());
        }

        // Snapshot the threshold that drove the new severity for display
        // ("p95(median) = 312 ≥ 250 critical"). When resolving back to Ok
        // we still keep the most recently relevant threshold so the UI
        // can show what the rule is now under.
        let threshold = match new_severity {
            Severity::Critical => rule.critical_threshold,
            Severity::Warning => rule.warning_threshold,
            Severity::Ok => rule.warning_threshold.or(rule.critical_threshold),
        };

        repo::upsert_state(
            &self.pool,
            rule.id,
            host_id,
            new_severity,
            Some(value),
            threshold,
            now,
        )
        .await?;
        self.state_cache
            .write()
            .insert((rule.id, host_id), new_severity);
        info!(
            rule_uuid = %rule.uuid,
            host_uuid = %host_uuid,
            from = current.as_str(),
            to = new_severity.as_str(),
            value,
            "alert state transition"
        );

        self.notify(
            rule,
            host_id,
            host_uuid,
            current,
            new_severity,
            Some(value),
            ResolveReason::Threshold,
            now,
        )
        .await;
        Ok(())
    }

    /// Drain `alert_state` rows whose `(rule, host)` pair is no longer
    /// in the current match set. Each drained pair gets a `match_lost`
    /// resolve webhook and the state row is deleted so the UI clears
    /// and we don't re-evaluate it next cycle.
    async fn reconcile_lost_matches(&self, match_set: &HashSet<(i64, i64)>) -> anyhow::Result<()> {
        let firing = repo::list_non_ok_state(&self.pool).await?;
        if firing.is_empty() {
            return Ok(());
        }
        let now = chrono::Utc::now().timestamp();
        for state in firing {
            if match_set.contains(&(state.rule_id, state.host_id)) {
                continue;
            }
            // Deleted rules can't surface here (ON DELETE CASCADE wiped
            // the state row already); disabled rules still load, so the
            // resolve webhook fires through their configured webhooks.
            if let Some(rule) = repo::load_rule_for_notify(&self.pool, state.rule_id).await? {
                info!(
                    rule_uuid = %rule.uuid,
                    host_uuid = %state.host_uuid,
                    from = state.severity.as_str(),
                    "alert match lost — sending resolve"
                );
                self.notify(
                    &rule,
                    state.host_id,
                    state.host_uuid,
                    state.severity,
                    Severity::Ok,
                    state.last_value,
                    ResolveReason::MatchLost,
                    now,
                )
                .await;
            }
            if let Err(e) = repo::delete_state(&self.pool, state.rule_id, state.host_id).await {
                warn!(
                    rule_id = state.rule_id,
                    host_id = state.host_id,
                    error = ?e,
                    "failed to delete reconciled alert_state row"
                );
            } else {
                self.state_cache
                    .write()
                    .remove(&(state.rule_id, state.host_id));
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn notify(
        &self,
        rule: &EnabledRule,
        host_id: i64,
        host_uuid: Uuid,
        from: Severity,
        to: Severity,
        value: Option<f32>,
        reason: ResolveReason,
        now: i64,
    ) {
        // Pick the threshold that drove the new severity. When `to == Ok`
        // there's no threshold being crossed - report the warning level
        // if it exists for reference (or critical otherwise).
        let threshold = match to {
            Severity::Critical => rule.critical_threshold,
            Severity::Warning => rule.warning_threshold,
            Severity::Ok => rule.warning_threshold.or(rule.critical_threshold),
        };

        // Best-effort host enrichment. If the row vanished between
        // evaluation and notify (host deleted mid-cycle) we still send
        // the webhook with just the uuid we already have.
        let host_meta = match repo::host_meta_by_id(&self.pool, host_id).await {
            Ok(m) => m,
            Err(e) => {
                warn!(host_id, error = ?e, "host_meta_by_id failed; sending webhook without host details");
                None
            }
        };
        let host_block = match &host_meta {
            Some(m) => {
                // probe_config is a JSON string in the DB; parse it so
                // consumers see a nested object. Fall back to the raw
                // string under the same key if parsing fails — better
                // than dropping the field.
                let probe_config: serde_json::Value = serde_json::from_str(&m.probe_config)
                    .unwrap_or_else(|_| serde_json::Value::String(m.probe_config.clone()));
                serde_json::json!({
                    "uuid": m.uuid,
                    "name": m.display_name,
                    "probe_type": m.probe_type,
                    "probe_config": probe_config,
                })
            }
            None => serde_json::json!({ "uuid": host_uuid }),
        };

        let payload = serde_json::json!({
            "rule_uuid": rule.uuid,
            "rule_name": rule.name,
            "host_uuid": host_uuid,
            "host": host_block,
            "from": from.as_str(),
            "to": to.as_str(),
            "reason": reason.as_str(),
            "metric": rule.metric.as_str(),
            "aggregation": rule.aggregation.as_str(),
            "direction": rule.direction.as_str(),
            "value": value,
            "threshold": threshold,
            "warning_threshold": rule.warning_threshold,
            "critical_threshold": rule.critical_threshold,
            "window_secs": rule.window_secs,
            "ts": now,
        });
        // Clone the client out of the lock for the duration of the
        // sends — held read locks across awaits would otherwise stop the
        // loop from swapping the client when settings change.
        let client = self.http.read().clone();
        for (_webhook_uuid, url, headers) in &rule.webhook_urls {
            client.post(url, headers, &payload).await;
        }
    }
}
