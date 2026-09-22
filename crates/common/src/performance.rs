//! Versioned benchmark wire format. Mirrored in praxis-gpu-agent; no user prompts/results.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BenchmarkSpec {
    pub port: u16,
    pub model: String,
    pub requests: u32,
    pub prompt_repetitions: u32,
    pub max_tokens: u32,
    pub timeout_s: u64,
    /// Read-only O_DIRECT test of an existing GGUF under PRAXIS_AGENT_MODEL_DIR.
    /// Zero disables it. Never download, write, drop caches or restart a service.
    pub disk_read_mb: u32,
}
impl Default for BenchmarkSpec {
    fn default() -> Self {
        Self { port: 11434, model: String::new(), requests: 3, prompt_repetitions: 64,
            max_tokens: 128, timeout_s: 120, disk_read_mb: 0 }
    }
}
impl BenchmarkSpec {
    pub fn validate(&self) -> Result<(), String> {
        if self.port == 0 || self.model.trim().is_empty() || self.model.len() > 128
            || !(1..=10).contains(&self.requests) || !(1..=256).contains(&self.prompt_repetitions)
            || !(16..=512).contains(&self.max_tokens) || !(10..=600).contains(&self.timeout_s)
            || self.disk_read_mb > 256 {
            return Err("invalid benchmark: port/model, requests 1..10, prompt_repetitions 1..256, max_tokens 16..512, timeout_s 10..600, disk_read_mb 0..256".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Metrics {
    pub model: String,
    pub elapsed_ms: f64,
    /// Observed first non-empty content/tool/reasoning delta, NOT first HTTP byte.
    pub ttft_ms: Option<f64>,
    pub prompt_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// Reported by the inference server; never estimate tokens from chunk counts.
    pub decode_tps: Option<f64>,
    pub prefill_tps: Option<f64>,
    pub gpu_util_pct: Option<f64>,
    pub vram_used_mb: Option<f64>,
    pub vram_total_mb: Option<f64>,
    /// Process-level physical I/O delta during inference, not GPU memory traffic.
    pub disk_read_bytes: Option<u64>,
    pub disk_write_bytes: Option<u64>,
    /// Separate O_DIRECT read test, NOT inference read_bytes/elapsed.
    pub disk_read_mbps: Option<f64>,
}
impl Metrics {
    pub fn validate(&self) -> Result<(), String> {
        if self.model.chars().count() > 128 || !self.elapsed_ms.is_finite() || self.elapsed_ms < 0.0
            || [self.ttft_ms, self.decode_tps, self.prefill_tps, self.gpu_util_pct,
                self.vram_used_mb, self.vram_total_mb, self.disk_read_mbps]
                .into_iter().flatten().any(|v| !v.is_finite() || v < 0.0)
            || self.gpu_util_pct.is_some_and(|v| v > 100.0) {
            return Err("invalid measurement".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkReport {
    pub schema: u32,
    pub samples: Vec<Metrics>,
    /// Closed diagnostic codes only, no command output, prompts or generated text.
    pub disk_status: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn benchmark_limits_and_unknown_fields_fail_closed() {
        let mut s=BenchmarkSpec {model:"test".into(),..Default::default()};
        assert!(s.validate().is_ok());
        s.requests=1000;assert!(s.validate().is_err());s.requests=3;
        s.disk_read_mb=257;assert!(s.validate().is_err());s.disk_read_mb=0;
        s.timeout_s=601;assert!(s.validate().is_err());
        assert!(serde_json::from_str::<BenchmarkSpec>(r#"{"host":"external.example"}"#).is_err());
    }
    #[test]
    fn missing_metrics_remain_unknown_and_invalid_numbers_reject() {
        let mut m=Metrics::default();
        assert_eq!(m.decode_tps,None);assert!(m.validate().is_ok());
        m.decode_tps=Some(f64::NAN);assert!(m.validate().is_err());m.decode_tps=None;
        m.gpu_util_pct=Some(101.0);assert!(m.validate().is_err());
    }
}
