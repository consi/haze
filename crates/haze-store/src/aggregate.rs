//! Per-period aggregation: a probe's N raw latency observations → one `Slot`
//! of seven percentile fields.

use crate::slot::Slot;

/// One latency observation from a single probe attempt.
#[derive(Debug, Clone, Copy)]
pub enum Observation {
    /// Successful probe with the given latency in milliseconds.
    Latency(f32),
    /// Probe completed but reported a non-success outcome (timeout, error, etc.).
    Loss,
}

/// Compute the seven percentile fields from N raw observations.
///
/// If all observations are losses, the percentile fields are NaN and
/// `loss_pct` = 100.0. If `obs` is empty, the entire slot is NaN.
pub fn aggregate(obs: &[Observation]) -> Slot {
    if obs.is_empty() {
        return Slot::NAN;
    }
    let total = obs.len();
    let mut latencies: Vec<f32> = obs
        .iter()
        .filter_map(|o| {
            if let Observation::Latency(v) = o {
                Some(*v)
            } else {
                None
            }
        })
        .collect();
    let losses = total - latencies.len();
    let loss_pct = (losses as f32) * 100.0 / (total as f32);

    if latencies.is_empty() {
        return Slot {
            min: f32::NAN,
            p2_5: f32::NAN,
            p25: f32::NAN,
            median: f32::NAN,
            p75: f32::NAN,
            p97_5: f32::NAN,
            loss_pct,
        };
    }
    latencies.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    Slot {
        min: latencies[0],
        p2_5: percentile(&latencies, 0.025),
        p25: percentile(&latencies, 0.25),
        median: percentile(&latencies, 0.50),
        p75: percentile(&latencies, 0.75),
        p97_5: percentile(&latencies, 0.975),
        loss_pct,
    }
}

/// Linear-interpolated percentile of a sorted slice (q in 0..=1).
fn percentile(sorted: &[f32], q: f64) -> f32 {
    debug_assert!(!sorted.is_empty());
    let n = sorted.len();
    if n == 1 {
        return sorted[0];
    }
    let idx = q * (n as f64 - 1.0);
    let lo = idx.floor() as usize;
    let hi = idx.ceil() as usize;
    if lo == hi {
        return sorted[lo];
    }
    let frac = (idx - lo as f64) as f32;
    (sorted[hi] - sorted[lo]).mul_add(frac, sorted[lo])
}

/// NaN-aware mean (returns NaN if all inputs are NaN).
pub fn mean_nan_aware(values: &[f32]) -> f32 {
    let mut sum = 0.0_f64;
    let mut count = 0_u32;
    for v in values {
        if !v.is_nan() {
            sum += *v as f64;
            count += 1;
        }
    }
    if count == 0 {
        f32::NAN
    } else {
        (sum / count as f64) as f32
    }
}

/// NaN-aware min.
pub fn min_nan_aware(values: &[f32]) -> f32 {
    let mut best = f32::NAN;
    for v in values {
        if !v.is_nan() && (best.is_nan() || *v < best) {
            best = *v;
        }
    }
    best
}

/// Consolidate N slots from a finer tier into one slot for a coarser tier.
///
/// Mean of each percentile field across the source slots, min for the min
/// field, mean for loss. Empty `slots` yields all-NaN; sequences of all-NaN
/// slots also yield all-NaN (NaN-propagating).
pub fn consolidate(slots: &[Slot]) -> Slot {
    let mins: Vec<f32> = slots.iter().map(|s| s.min).collect();
    let p2_5s: Vec<f32> = slots.iter().map(|s| s.p2_5).collect();
    let p25s: Vec<f32> = slots.iter().map(|s| s.p25).collect();
    let medians: Vec<f32> = slots.iter().map(|s| s.median).collect();
    let p75s: Vec<f32> = slots.iter().map(|s| s.p75).collect();
    let p97_5s: Vec<f32> = slots.iter().map(|s| s.p97_5).collect();
    let losses: Vec<f32> = slots.iter().map(|s| s.loss_pct).collect();

    Slot {
        min: min_nan_aware(&mins),
        p2_5: mean_nan_aware(&p2_5s),
        p25: mean_nan_aware(&p25s),
        median: mean_nan_aware(&medians),
        p75: mean_nan_aware(&p75s),
        p97_5: mean_nan_aware(&p97_5s),
        loss_pct: mean_nan_aware(&losses),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lat(ms: f32) -> Observation {
        Observation::Latency(ms)
    }
    const LOSS: Observation = Observation::Loss;

    #[test]
    fn empty_is_nan() {
        let s = aggregate(&[]);
        assert!(s.is_nan());
    }

    #[test]
    fn single_observation() {
        let s = aggregate(&[lat(10.0)]);
        assert_eq!(s.min, 10.0);
        assert_eq!(s.median, 10.0);
        assert_eq!(s.p97_5, 10.0);
        assert_eq!(s.loss_pct, 0.0);
    }

    #[test]
    fn all_losses() {
        let s = aggregate(&[LOSS, LOSS, LOSS]);
        assert!(s.min.is_nan());
        assert!(s.median.is_nan());
        assert_eq!(s.loss_pct, 100.0);
    }

    #[test]
    fn mixed_losses() {
        let obs: Vec<Observation> = (1..=8).map(|i| lat(i as f32)).chain([LOSS, LOSS]).collect();
        let s = aggregate(&obs);
        assert_eq!(s.min, 1.0);
        assert_eq!(s.loss_pct, 20.0); // 2 / 10
        // median of [1..=8] is 4.5
        assert!((s.median - 4.5).abs() < 0.001);
    }

    #[test]
    fn percentile_lookup() {
        let obs: Vec<Observation> = (1..=100).map(|i| lat(i as f32)).collect();
        let s = aggregate(&obs);
        // p25 is at index 24.75 → linear interp between 25 and 26 → 25.75
        assert!((s.p25 - 25.75).abs() < 0.01, "p25 = {}", s.p25);
        // p75 at 74.25 → between 75 and 76 → 75.25
        assert!((s.p75 - 75.25).abs() < 0.01, "p75 = {}", s.p75);
    }

    #[test]
    fn consolidate_mean_per_percentile() {
        let a = aggregate(&[lat(1.0), lat(2.0), lat(3.0)]);
        let b = aggregate(&[lat(4.0), lat(5.0), lat(6.0)]);
        let c = consolidate(&[a, b]);
        assert_eq!(c.min, 1.0);
        assert!((c.median - 3.5).abs() < 0.01, "median {}", c.median);
        assert_eq!(c.loss_pct, 0.0);
    }

    #[test]
    fn consolidate_skips_nans() {
        let a = aggregate(&[lat(10.0), lat(20.0)]);
        let nan = Slot::NAN;
        let c = consolidate(&[a, nan]);
        assert_eq!(c.min, 10.0);
        assert!((c.median - 15.0).abs() < 0.01);
    }

    #[test]
    fn consolidate_all_nan_stays_nan() {
        let c = consolidate(&[Slot::NAN, Slot::NAN]);
        assert!(c.is_nan());
    }
}
