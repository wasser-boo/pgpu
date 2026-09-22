//! Measurement orchestration. No workloads start unless explicitly configured.
use crate::{config::SlotCfg, db::{InstanceRow, PerformanceRow}, state::SharedApp};
use praxis_common::{node::{Command, RouterCommand}, performance::{BenchmarkSpec, BenchmarkReport, Metrics}};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PerformanceConfig {
    pub enabled: bool,
    pub collect_usage: bool,
    /// Operator-defined workload identity: model/quantization/context/runtime settings.
    pub profile: String,
    pub retention_days: u32,
    pub max_samples: usize,
    pub benchmark: BenchmarkConfig,
    pub score: praxis_policy::performance::ScoreConfig,
}
impl Default for PerformanceConfig {
    fn default() -> Self {
        Self { enabled:false,collect_usage:false,profile:String::new(),retention_days:90,max_samples:50000,
            benchmark:Default::default(),score:Default::default() }
    }
}
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BenchmarkConfig {
    pub enabled: bool,
    pub auto_when_idle: bool,
    pub min_interval_s: u64,
    pub idle_s: i64,
    pub spec: BenchmarkSpec,
}
impl Default for BenchmarkConfig {
    fn default() -> Self { Self { enabled:false,auto_when_idle:false,min_interval_s:86400,idle_s:120,spec:Default::default() } }
}
impl PerformanceConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.enabled && (self.profile.trim().is_empty() || self.profile.len() > 128) {
            return Err("performance.profile required (max 128 characters)".into());
        }
        if !(1..=3650).contains(&self.retention_days) || !(100..=1000000).contains(&self.max_samples)
            || !(600..=315360000).contains(&self.benchmark.min_interval_s) || self.benchmark.idle_s < 0 {
            return Err("invalid performance retention/benchmark interval".into());
        }
        if self.benchmark.enabled { self.benchmark.spec.validate()?; }
        if (self.collect_usage || self.benchmark.enabled || self.score.enabled) && !self.enabled {
            return Err("performance subfeatures require enabled=true".into());
        }
        if self.benchmark.auto_when_idle && !self.benchmark.enabled { return Err("auto benchmark requires benchmark.enabled".into()); }
        if self.score.enabled && self.benchmark.spec.model.is_empty() { return Err("score requires benchmark.spec.model".into()); }
        self.score.validate()
    }
}

pub fn runtime_key(slot: &SlotCfg, image: &str) -> String {
    let env: BTreeMap<_,_> = slot.env.iter().collect();
    hex::encode(Sha256::digest(serde_json::to_vec(&(image,env)).unwrap()))
}
pub fn workload_key(slot: &SlotCfg, image: &str) -> String {
    workload_with_runtime(slot,image,&runtime_key(slot,image))
}
fn workload_with_runtime(slot: &SlotCfg, image: &str, runtime: &str) -> String {
    // Excludes feature toggles/ranking weights. Actual launch environment is hashed
    // at rental time: a config reload must not relabel an old runtime as a new one.
    // Use immutable image digests; mutable tags cannot identify rebuilt binaries.
    let bytes = serde_json::to_vec(&(2,slot.id,slot.role,&slot.performance.profile,image,&slot.performance.benchmark.spec,runtime)).unwrap();
    hex::encode(Sha256::digest(bytes))
}
fn instance_workload_key(app: &SharedApp, slot: &SlotCfg, inst: &InstanceRow) -> anyhow::Result<String> {
    let runtime=app.db.rental_runtime_key(inst.vast_id)?.unwrap_or_else(||"unverified-legacy-runtime".into());
    Ok(workload_with_runtime(slot,&inst.image,&runtime))
}
pub fn allocation_key(o: &praxis_policy::OfferSnapshot, disk: i64) -> String {
    serde_json::to_string(&(praxis_policy::eligibility::gpu_key(&o.gpu_name),o.num_gpus,o.gpu_ram_gb,o.cpu_ram_gb,o.cpu_cores,disk)).unwrap_or_default()
}
fn row(slot: &SlotCfg, inst: &InstanceRow, allocation: String, source: &str, success: bool, metrics: Metrics) -> PerformanceRow {
    PerformanceRow { id:0,ts:chrono::Utc::now().timestamp(),slot_id:slot.id,instance_id:inst.vast_id,machine_id:inst.machine_id,
        gpu_name:inst.gpu_name.clone(),profile:slot.performance.profile.clone(),allocation,workload_key:workload_key(slot,&inst.image),image:inst.image.clone(),
        source:source.into(),success,price_usd_h:inst.storage_usd_h + if inst.mode == praxis_common::Mode::Interruptible {inst.bid_usd_h} else {inst.dph_total},metrics }
}
fn save(app: &SharedApp, slot: &SlotCfg, inst: &InstanceRow, source: &str, success: bool, metrics: Metrics) -> anyhow::Result<()> {
    let allocation=app.db.rental_facts(inst.vast_id)?.map(|o|allocation_key(&o,o.disk_gb as i64)).unwrap_or_default();
    let mut row=row(slot,inst,allocation,source,success,metrics);
    row.workload_key=instance_workload_key(app,slot,inst)?;
    app.db.record_performance(&row,slot.performance.retention_days,slot.performance.max_samples)
}

/// Host/GPU scores use only matching, recent, complete benchmark records. Real-user
/// request shapes are deliberately NOT ranked as if they were controlled experiments.
pub fn host_scores(app: &SharedApp, slot: &SlotCfg) -> anyhow::Result<HashMap<(i64,String), f64>> {
    let key = workload_key(slot,&slot.image);
    let cutoff = chrono::Utc::now().timestamp() - i64::from(slot.performance.score.max_age_days) * 86400;
    let rows = app.db.benchmark_score_samples(slot.id, &key, cutoff)?;
    let mut scores: HashMap<(i64,String), Vec<f64>> = HashMap::new();
    for (machine, allocation, metrics) in rows {
        if metrics.model != slot.performance.benchmark.spec.model || allocation.is_empty() { continue; }
        if let Some(score) = slot.performance.score.score(&metrics) {
            scores.entry((machine,allocation)).or_default().push(score);
        }
    }
    Ok(scores.into_iter().filter_map(|(k,mut v)| {
        if v.len() < slot.performance.score.min_samples { return None; }
        v.sort_by(f64::total_cmp);
        let mid = v.len()/2;
        let median = if v.len()%2 == 0 {(v[mid-1]+v[mid])/2.0} else {v[mid]};
        Some((k,median))
    }).collect())
}

pub async fn start_benchmark(app: &SharedApp, id: i64) -> anyhow::Result<()> {
    let _management = app.management.lock().await;
    anyhow::ensure!(!app.shutting_down.load(std::sync::atomic::Ordering::Relaxed),"router shutting down");
    let inst = app.db.instance(id).ok_or_else(|| anyhow::anyhow!("unknown instance"))?;
    let slot = app.cfg().slot(inst.slot_id).cloned().ok_or_else(|| anyhow::anyhow!("unknown slot"))?;
    let p = &slot.performance;
    anyhow::ensure!(p.enabled && p.benchmark.enabled,"benchmarks disabled in config");
    let rate=if inst.mode==praxis_common::Mode::Interruptible {inst.bid_usd_h} else {inst.dph_total};
    crate::operations::check_admission(app,inst.slot_id,rate,0.0,Some(id))?;
    anyhow::ensure!(inst.destroyed_at.is_none() && inst.state == "healthy" && inst.actual_status == "running" && !inst.busy,"instance must be healthy and idle");
    anyhow::ensure!(!app.db.pending_operations()?.iter().any(|r|r.0 == id),"pending provider operation");
    let now = chrono::Utc::now().timestamp();
    anyhow::ensure!(app.hub.last_seen(id).is_some_and(|ts|now-ts <= 15),"agent heartbeat stale");
    anyhow::ensure!(app.hub.heartbeat(id).is_some_and(|h| !h.busy && h.health_json == serde_json::json!("healthy")),"agent not ready/idle");
    anyhow::ensure!(now-app.traffic.snapshot(slot.id).last_request >= p.benchmark.idle_s,"idle grace not elapsed");
    anyhow::ensure!(app.jobs.active(slot.id).is_none(),"slot has an active batch");
    let key = instance_workload_key(app,&slot,&inst)?;
    anyhow::ensure!(app.db.benchmark_due(id,&key,p.benchmark.min_interval_s)?,"benchmark cooldown active");
    let guard = app.traffic.try_benchmark(slot.id).ok_or_else(|| anyhow::anyhow!("slot has traffic/benchmark"))?;
    // Persist ATTEMPT before sending: crashes/timeouts must not cause a benchmark storm.
    app.db.mark_benchmark_attempt(id,&key)?;
    let app = app.clone();
    tokio::spawn(async move {
        let _guard = guard; // gates new slot traffic and all idle lifecycle decisions
        let spec = slot.performance.benchmark.spec.clone();
        let timeout = std::time::Duration::from_secs(spec.timeout_s + 10);
        let result = app.hub.command_timeout(id, |id|RouterCommand::Cmd {id,command:Command::Benchmark {spec}}, timeout).await;
        let result = result.map_err(anyhow::Error::msg).and_then(|value| {
            let report: BenchmarkReport = serde_json::from_value(value)?;
            anyhow::ensure!(report.schema == 1 && report.samples.len() == slot.performance.benchmark.spec.requests as usize,"invalid benchmark report");
            for m in &report.samples { m.validate().map_err(anyhow::Error::msg)?; }
            Ok(report)
        });
        match result {
            Ok(report) => {
                let mut saved = true;
                for m in report.samples {
                    if let Err(e) = save(&app,&slot,&inst,"benchmark",true,m) {
                        saved = false;
                        tracing::error!(%e,id,"benchmark persistence failed");
                    }
                }
                app.events.emit(&app.db,if saved {"benchmark_complete"} else {"benchmark_storage_failed"},Some(slot.id),Some(id),
                    "Benchmark beendet; Messwerte im GPU-Katalog", &serde_json::json!({"workload_key":key,"saved":saved}));
            }
            Err(e) => {
                let m = Metrics {model:slot.performance.benchmark.spec.model.clone(),..Default::default()};
                if let Err(error) = save(&app,&slot,&inst,"benchmark",false,m) {tracing::error!(%error,"benchmark failure persistence failed");}
                // Do not copy command output/agent-provided text into durable history.
                tracing::warn!(id,%e,"benchmark failed");
                app.events.emit(&app.db,"benchmark_failed",Some(slot.id),Some(id),"Benchmark fehlgeschlagen (Agent-Version/Freigabe/Service/Timeout prüfen)",&serde_json::json!({"workload_key":key}));
            }
        }
    });
    Ok(())
}

pub async fn auto_benchmarks(app: &SharedApp) {
    let pins=app.db.slot_pins();
    for inst in app.db.instances(false) {
        if inst.pinned || pins.iter().any(|(slot,pin)|*slot==inst.slot_id && pin.is_some()) {continue;}
        let enabled = app.cfg().slot(inst.slot_id).is_some_and(|s| s.performance.enabled && s.performance.benchmark.enabled && s.performance.benchmark.auto_when_idle);
        if enabled { let _ = start_benchmark(app,inst.vast_id).await; }
    }
}

/// Bounded response observer; never changes bodies/requests and never persists content.
/// SSE: at most one 64-KiB line. JSON: at most 256 KiB; oversized responses still get latency.
pub struct UsageObserver {
    app: SharedApp, slot: SlotCfg, inst: InstanceRow, started: std::time::Instant,
    metrics: Metrics, buffer: Vec<u8>, sse: bool, overflow: bool, failed: bool, http_ok: bool, stream_done: bool,
}
impl UsageObserver {
    pub fn new(app: &SharedApp, id: i64, path: &str, method: &axum::http::Method) -> Option<Self> {
        if method != axum::http::Method::POST || !["/v1/chat/completions","/v1/completions","/completion"].contains(&path.split('?').next()?) {return None;}
        let inst=app.db.instance(id)?;
        let slot=app.cfg().slot(inst.slot_id)?.clone();
        if !slot.performance.enabled || !slot.performance.collect_usage {return None;}
        Some(Self { app:app.clone(),slot,inst,started:std::time::Instant::now(),metrics:Metrics::default(),buffer:Vec::new(),sse:false,overflow:false,failed:false,http_ok:false,stream_done:false })
    }
    pub fn response(&mut self, status: axum::http::StatusCode, headers: &axum::http::HeaderMap) {
        self.http_ok=status.is_success();
        self.sse=headers.get("content-type").and_then(|v|v.to_str().ok()).is_some_and(|v|v.starts_with("text/event-stream"));
    }
    pub fn feed(&mut self, data: &[u8]) {
        if !self.http_ok {return;}
        if self.sse {
            for chunk in data.split_inclusive(|b|*b==b'\n') {
                if self.buffer.len()+chunk.len() > 65536 {self.buffer.clear();self.overflow=true;}
                if !self.overflow {self.buffer.extend_from_slice(chunk);}
                if chunk.last()==Some(&b'\n') {
                    if !self.overflow {
                        let line=std::mem::take(&mut self.buffer);
                        if let Some(json)=line.strip_prefix(b"data:") {
                            if json.trim_ascii()==b"[DONE]" {self.stream_done=true;}
                            else if let Ok(v)=serde_json::from_slice(json) {self.observe(v);}
                        }
                    }
                    self.overflow=false;
                }
            }
        } else if !self.overflow {
            if self.buffer.len()+data.len()>256*1024 {self.buffer.clear();self.overflow=true;}
            else {self.buffer.extend_from_slice(data);}
        }
    }
    fn observe(&mut self, v: serde_json::Value) {
        if v.get("error").is_some() {self.failed=true;}
        if v["stop"].as_bool()==Some(true) {self.stream_done=true;}
        if let Some(model)=v.get("model").and_then(|v|v.as_str()) {self.metrics.model=model.chars().take(128).collect();}
        let has_token = v["content"].as_str().is_some_and(|s|!s.is_empty()) || v.get("choices").and_then(|v|v.as_array()).is_some_and(|choices|choices.iter().any(|c| {
            let d=&c["delta"];
            c["text"].as_str().is_some_and(|s|!s.is_empty()) || ["content","reasoning_content","reasoning"].iter().any(|k| d[*k].as_str().is_some_and(|s|!s.is_empty())) || d["tool_calls"].as_array().is_some_and(|a|!a.is_empty())
        }));
        if self.sse && self.metrics.ttft_ms.is_none() && has_token { self.metrics.ttft_ms=Some(self.started.elapsed().as_secs_f64()*1000.0); }
        if let Some(n)=v["usage"]["prompt_tokens"].as_u64().or_else(||v["timings"]["prompt_n"].as_u64()) {self.metrics.prompt_tokens=Some(n);}
        if let Some(n)=v["usage"]["completion_tokens"].as_u64().or_else(||v["timings"]["predicted_n"].as_u64()) {self.metrics.output_tokens=Some(n);}
        let positive=|v:&serde_json::Value|v.as_f64().filter(|v|v.is_finite() && *v>=0.0);
        if let Some(v)=positive(&v["timings"]["predicted_per_second"]) {self.metrics.decode_tps=Some(v);}
        if let Some(v)=positive(&v["timings"]["prompt_per_second"]) {self.metrics.prefill_tps=Some(v);}
    }
    pub fn finish(mut self, complete: bool) {
        if !self.sse && !self.overflow {
            if let Ok(v)=serde_json::from_slice(&self.buffer) {self.observe(v);}
        }
        self.buffer.clear();
        self.metrics.elapsed_ms=self.started.elapsed().as_secs_f64()*1000.0;
        if self.app.hub.last_seen(self.inst.vast_id).is_some_and(|ts|chrono::Utc::now().timestamp()-ts<=15) {
            if let Some(h)=self.app.hub.heartbeat(self.inst.vast_id) {
                // Legacy nvidia-smi failures produce all-zero defaults: unknown, not measured zero.
                if let Some(total)=h.gpu_json["mem_total_mb"].as_f64().filter(|n|n.is_finite() && *n>0.0) {
                    self.metrics.gpu_util_pct=h.gpu_json["util_pct"].as_f64().filter(|n|n.is_finite() && (0.0..=100.0).contains(n));
                    self.metrics.vram_used_mb=h.gpu_json["mem_used_mb"].as_f64().filter(|n|n.is_finite() && (0.0..=total).contains(n));
                    self.metrics.vram_total_mb=Some(total);
                }
            }
        }
        if let Err(e)=save(&self.app,&self.slot,&self.inst,"usage",complete && self.http_ok && !self.failed && (!self.sse || self.stream_done),self.metrics) {tracing::warn!(%e,"usage measurement persistence failed");}
    }
}
