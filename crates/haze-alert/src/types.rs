//! Public types for the alerting subsystem.
//!
//! Kept in their own module so the API crate can pull them in without
//! grabbing the engine + repo internals.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// One of the seven Slot fields a rule can be aggregated against.
/// Matches `haze_store::Slot`'s field names verbatim so serde and the DB
/// CHECK constraint agree on spelling.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Metric {
    Min,
    P2_5,
    P25,
    Median,
    P75,
    P97_5,
    LossPct,
}

impl Metric {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Min => "min",
            Self::P2_5 => "p2_5",
            Self::P25 => "p25",
            Self::Median => "median",
            Self::P75 => "p75",
            Self::P97_5 => "p97_5",
            Self::LossPct => "loss_pct",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "min" => Self::Min,
            "p2_5" => Self::P2_5,
            "p25" => Self::P25,
            "median" => Self::Median,
            "p75" => Self::P75,
            "p97_5" => Self::P97_5,
            "loss_pct" => Self::LossPct,
            _ => return None,
        })
    }

    /// Extract this field from a Slot.
    pub fn extract(self, slot: &haze_store::Slot) -> f32 {
        match self {
            Self::Min => slot.min,
            Self::P2_5 => slot.p2_5,
            Self::P25 => slot.p25,
            Self::Median => slot.median,
            Self::P75 => slot.p75,
            Self::P97_5 => slot.p97_5,
            Self::LossPct => slot.loss_pct,
        }
    }
}

/// Aggregation applied to the chosen metric across every sample in the
/// rule's sliding window.
///
/// `max`/`avg`/`min` are arithmetic; the `p*` variants sort a temporary
/// copy (windows are small so this is fine).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Aggregation {
    Max,
    Avg,
    Min,
    P50,
    P75,
    P90,
    P95,
    P99,
}

impl Aggregation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Max => "max",
            Self::Avg => "avg",
            Self::Min => "min",
            Self::P50 => "p50",
            Self::P75 => "p75",
            Self::P90 => "p90",
            Self::P95 => "p95",
            Self::P99 => "p99",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "max" => Self::Max,
            "avg" => Self::Avg,
            "min" => Self::Min,
            "p50" => Self::P50,
            "p75" => Self::P75,
            "p90" => Self::P90,
            "p95" => Self::P95,
            "p99" => Self::P99,
            _ => return None,
        })
    }

    /// Compute the aggregation over `values`. Returns None for an empty
    /// input; callers treat that as "no data, leave state alone".
    pub fn apply(self, values: &[f32]) -> Option<f32> {
        if values.is_empty() {
            return None;
        }
        Some(match self {
            Self::Max => values.iter().copied().fold(f32::NEG_INFINITY, f32::max),
            Self::Min => values.iter().copied().fold(f32::INFINITY, f32::min),
            Self::Avg => {
                let sum: f32 = values.iter().copied().sum();
                sum / values.len() as f32
            }
            Self::P50 => percentile(values, 0.50),
            Self::P75 => percentile(values, 0.75),
            Self::P90 => percentile(values, 0.90),
            Self::P95 => percentile(values, 0.95),
            Self::P99 => percentile(values, 0.99),
        })
    }
}

fn percentile(values: &[f32], q: f32) -> f32 {
    let mut sorted: Vec<f32> = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    if sorted.len() == 1 {
        return sorted[0];
    }
    // Nearest-rank percentile: rank = ceil(q * n); index = rank - 1.
    let rank = (q * sorted.len() as f32).ceil() as usize;
    let idx = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[idx]
}

/// Threshold comparison direction.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// Fire when the aggregated value is at or above the threshold.
    Above,
    /// Fire when the aggregated value is at or below the threshold.
    Below,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Above => "above",
            Self::Below => "below",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "above" => Self::Above,
            "below" => Self::Below,
            _ => return None,
        })
    }
}

/// Alert severity. `Ok` is the steady state.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Ok,
    Warning,
    Critical,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warning => "warning",
            Self::Critical => "critical",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "ok" => Self::Ok,
            "warning" => Self::Warning,
            "critical" => Self::Critical,
            _ => return None,
        })
    }
}

/// Why a webhook is being delivered.
///
/// `Threshold` is the normal case (value crossed warning/critical, or
/// returned to ok). `MatchLost` means the host-rule pairing no longer
/// exists — host was removed from a targeted group, the rule's targets
/// were edited, or the rule was disabled — so any firing state for that
/// pair is being cleared.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResolveReason {
    Threshold,
    MatchLost,
}

impl ResolveReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Threshold => "threshold",
            Self::MatchLost => "match_lost",
        }
    }
}

/// What kind of entity a rule target points at.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    Host,
    Group,
}

impl TargetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Group => "group",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "host" => Self::Host,
            "group" => Self::Group,
            _ => return None,
        })
    }
}

/// Decide the severity for a value under the rule's direction + thresholds.
///
/// `Above`: value >= critical -> Critical, value >= warning -> Warning, else Ok.
/// `Below`: value <= critical -> Critical, value <= warning -> Warning, else Ok.
/// Either threshold may be `None`; that level is simply unreachable.
pub fn classify(
    value: f32,
    direction: Direction,
    warning: Option<f32>,
    critical: Option<f32>,
) -> Severity {
    if value.is_nan() {
        return Severity::Ok;
    }
    let exceeds = |threshold: f32| match direction {
        Direction::Above => value >= threshold,
        Direction::Below => value <= threshold,
    };
    if critical.is_some_and(exceeds) {
        Severity::Critical
    } else if warning.is_some_and(exceeds) {
        Severity::Warning
    } else {
        Severity::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregation_max_avg_min() {
        let v = [10.0, 20.0, 30.0];
        assert_eq!(Aggregation::Max.apply(&v), Some(30.0));
        assert_eq!(Aggregation::Min.apply(&v), Some(10.0));
        assert_eq!(Aggregation::Avg.apply(&v), Some(20.0));
    }

    #[test]
    fn aggregation_percentiles() {
        let v: Vec<f32> = (1..=100).map(|i| i as f32).collect();
        assert_eq!(Aggregation::P50.apply(&v), Some(50.0));
        assert_eq!(Aggregation::P95.apply(&v), Some(95.0));
        assert_eq!(Aggregation::P99.apply(&v), Some(99.0));
    }

    #[test]
    fn aggregation_empty() {
        assert_eq!(Aggregation::Max.apply(&[]), None);
    }

    #[test]
    fn classify_above_two_thresholds() {
        let warn = Some(100.0);
        let crit = Some(200.0);
        assert_eq!(classify(50.0, Direction::Above, warn, crit), Severity::Ok);
        assert_eq!(
            classify(150.0, Direction::Above, warn, crit),
            Severity::Warning
        );
        assert_eq!(
            classify(250.0, Direction::Above, warn, crit),
            Severity::Critical
        );
        assert_eq!(
            classify(200.0, Direction::Above, warn, crit),
            Severity::Critical
        );
    }

    #[test]
    fn classify_below_two_thresholds() {
        let warn = Some(100.0);
        let crit = Some(50.0);
        assert_eq!(classify(150.0, Direction::Below, warn, crit), Severity::Ok);
        assert_eq!(
            classify(80.0, Direction::Below, warn, crit),
            Severity::Warning
        );
        assert_eq!(
            classify(40.0, Direction::Below, warn, crit),
            Severity::Critical
        );
    }

    #[test]
    fn classify_only_warning() {
        let warn = Some(100.0);
        assert_eq!(
            classify(150.0, Direction::Above, warn, None),
            Severity::Warning
        );
        assert_eq!(classify(50.0, Direction::Above, warn, None), Severity::Ok);
    }

    #[test]
    fn classify_only_critical() {
        let crit = Some(200.0);
        assert_eq!(classify(150.0, Direction::Above, None, crit), Severity::Ok);
        assert_eq!(
            classify(250.0, Direction::Above, None, crit),
            Severity::Critical
        );
    }

    #[test]
    fn classify_nan_is_ok() {
        assert_eq!(
            classify(f32::NAN, Direction::Above, Some(10.0), Some(20.0)),
            Severity::Ok
        );
    }
}
