//! Typen für die Vast.ai API v0 (https://console.vast.ai/api/v0).
//!
//! Felder sind defensiv (Option/default): die genauen Antwort-Formen werden
//! in Phase 0 (Spike) gegen Live-Daten verifiziert; Fixes sind ein Zeiler.
//! Bekannt: Such-Prädikate (q) in GB, Feldwerte in MB (RAM/VRAM).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OfferType {
    #[default]
    Interruption,
    #[serde(alias = "on-demand")]
    OnDemand,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Offer {
    pub id: i64,
    #[serde(default)]
    pub gpu_name: String,
    #[serde(default)]
    pub gpu_ram: i64, // MB
    #[serde(default)]
    pub cpu_ram: i64, // MB
    #[serde(default)]
    pub disk_space: f64,
    #[serde(default)]
    pub disk_bw: f64,
    #[serde(default)]
    pub inet_down: f64,
    #[serde(default)]
    pub inet_down_cost: f64,
    #[serde(default)]
    pub inet_up_cost: f64,
    #[serde(default)]
    pub storage_cost: f64, // $/GB/Monat
    #[serde(default)]
    pub dph_total: f64,
    #[serde(default)]
    pub min_bid: f64,
    #[serde(default)]
    pub reliability2: f64,
    #[serde(default)]
    pub machine_id: i64,
    #[serde(default)]
    pub cuda_max_good: String,
    #[serde(default)]
    pub num_gpus: i64,
    #[serde(default)]
    pub geolocation: String,
    #[serde(default)]
    pub cpu_cores: i64,
    #[serde(default)]
    pub gpu_frac: f64,
}

impl Offer {
    pub fn gpu_ram_gb(&self) -> f64 {
        self.gpu_ram as f64 / 1024.0
    }
    pub fn cpu_ram_gb(&self) -> f64 {
        self.cpu_ram as f64 / 1024.0
    }
    /// Storage-Preis $/h für `disk_gb`.
    pub fn storage_usd_h(&self, disk_gb: i64) -> f64 {
        self.storage_cost * disk_gb as f64 / 30.0 / 24.0
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Instance {
    pub id: i64,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub machine_id: i64,
    #[serde(default)]
    pub gpu_name: String,
    #[serde(default)]
    pub image: String,
    /// vast: "running" | "loading" | "stopped" | "error" | ...
    #[serde(default)]
    pub actual_status: String,
    #[serde(default)]
    pub intended_status: String,
    #[serde(default)]
    pub cur_state: String,
    #[serde(default)]
    pub next_state: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub bid_value: f64,
    #[serde(default)]
    pub dph_total: f64,
    #[serde(default)]
    pub min_bid: f64,
    #[serde(default)]
    pub storage_total: f64,
    #[serde(default)]
    pub storage_cost: f64,
    #[serde(default)]
    pub inet_down_cost: f64,
    #[serde(default)]
    pub extra_env: serde_json::Value,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CurrentUser {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub credit: f64,
    #[serde(default)]
    pub balance: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateInstanceParams<'a> {
    pub image: &'a str,
    pub price: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template_hash_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub onstart_cmd: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtype: Option<&'a str>, // "on-demand" | "interruptible"
    pub env: serde_json::Value,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct LogEntry {
    #[serde(default)]
    pub timestamp: f64,
    #[serde(default)]
    pub log: String,
}