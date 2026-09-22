//! Workload-relative score, not a universal GPU rating. Ranking is opt-in.
use praxis_common::performance::Metrics;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScoreConfig {
    pub enabled: bool,
    /// Maximum price-ranking adjustment at score 0/100; unknown stays neutral.
    pub preference_weight: f64,
    pub min_samples: usize,
    pub max_age_days: u32,
    pub decode_target_tps: f64,
    pub prefill_target_tps: f64,
    pub ttft_target_ms: f64,
    pub decode_weight: f64,
    pub prefill_weight: f64,
    pub ttft_weight: f64,
}
impl Default for ScoreConfig {
    fn default() -> Self {
        Self { enabled: false, preference_weight: 0.25, min_samples: 3, max_age_days: 30,
            decode_target_tps: 40.0, prefill_target_tps: 500.0, ttft_target_ms: 1000.0,
            decode_weight: 0.7, prefill_weight: 0.2, ttft_weight: 0.1 }
    }
}
impl ScoreConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !self.preference_weight.is_finite() || !(0.0..=0.5).contains(&self.preference_weight)
            || !(1..=1000).contains(&self.min_samples) || !(1..=3650).contains(&self.max_age_days)
            || [self.decode_target_tps, self.prefill_target_tps, self.ttft_target_ms]
                .into_iter().any(|n| !n.is_finite() || n <= 0.0)
            || [self.decode_weight, self.prefill_weight, self.ttft_weight].into_iter().any(|n| !n.is_finite() || !(0.0..=1.0).contains(&n))
            || self.decode_weight + self.prefill_weight + self.ttft_weight <= 0.0 {
            return Err("invalid score weights, targets, sample count or age".into());
        }
        Ok(())
    }
    pub fn score(&self, m: &Metrics) -> Option<f64> {
        m.validate().ok()?;
        let mut sum = 0.0;
        let mut weights = 0.0;
        for (actual, target, weight, higher_better) in [
            (m.decode_tps, self.decode_target_tps, self.decode_weight, true),
            (m.prefill_tps, self.prefill_target_tps, self.prefill_weight, true),
            (m.ttft_ms, self.ttft_target_ms, self.ttft_weight, false),
        ] {
            if weight == 0.0 { continue; }
            let actual = actual?;
            // Target = 50 points, double throughput (or half latency) = 66.7.
            // Smooth/asymptotic, no arbitrary saturation above target.
            let component = if higher_better { actual / (actual + target) } else { target / (actual + target) };
            sum += weight * component;
            weights += weight;
        }
        (weights > 0.0).then_some(100.0 * sum / weights)
    }
    pub fn rank_cost(&self, cost: f64, score: Option<f64>) -> f64 {
        if !self.enabled { return cost; }
        let Some(score) = score.filter(|s| s.is_finite() && (0.0..=100.0).contains(s)) else { return cost; };
        cost * (1.0 - self.preference_weight * (score - 50.0) / 50.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn score_requires_every_weighted_metric_and_is_monotonic() {
        let c = ScoreConfig::default();
        let mut m = Metrics { elapsed_ms: 1000.0, decode_tps: Some(40.0), prefill_tps: Some(500.0), ..Default::default() };
        assert!(c.score(&m).is_none());
        m.ttft_ms = Some(1000.0);
        assert!((c.score(&m).unwrap() - 50.0).abs() < 1e-8);
        m.decode_tps = Some(80.0);
        assert!(c.score(&m).unwrap() > 50.0);
        m.disk_read_bytes = Some(u64::MAX); // more I/O is NOT rewarded
        assert!(c.score(&m).unwrap() < 70.0);
    }
    #[test]
    fn preference_is_opt_in_bounded_and_unknown_is_neutral() {
        let mut c = ScoreConfig::default();
        assert_eq!(c.rank_cost(1.0, Some(100.0)), 1.0);
        c.enabled = true;
        assert_eq!(c.rank_cost(1.0, None), 1.0);
        assert_eq!(c.rank_cost(1.0, Some(100.0)), 0.75);
        assert_eq!(c.rank_cost(1.0, Some(0.0)), 1.25);
    }
}
