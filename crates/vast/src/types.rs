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
#[serde(default)]
pub struct Offer {
    pub id: i64,
    pub gpu_name: String,
    pub gpu_ram: i64, // MB
    pub cpu_ram: i64, // MB
    pub disk_space: f64,
    pub disk_bw: f64,
    pub inet_down: f64,
    // Vast liefert je nach Suchmodus (bid/on-demand) null:
    pub inet_down_cost: Option<f64>,
    pub inet_up_cost: Option<f64>,
    pub storage_cost: Option<f64>, // $/GB/Monat
    pub dph_total: Option<f64>,
    pub min_bid: Option<f64>,
    pub reliability2: Option<f64>,
    pub machine_id: i64,
    pub cuda_max_good: Option<f64>,
    pub num_gpus: i64,
    pub geolocation: Option<String>,
    pub cpu_cores: Option<f64>,
    pub gpu_frac: Option<f64>,
}

impl Offer {
    pub fn gpu_ram_gb(&self) -> f64 {
        self.gpu_ram as f64 / 1024.0
    }
    pub fn cpu_ram_gb(&self) -> f64 {
        self.cpu_ram as f64 / 1024.0
    }
    pub fn min_bid_or(&self, alt: f64) -> f64 {
        self.min_bid.unwrap_or(alt)
    }
    pub fn dph_or(&self, alt: f64) -> f64 {
        self.dph_total.unwrap_or(alt)
    }
    pub fn storage_or(&self, alt: f64) -> f64 {
        self.storage_cost.unwrap_or(alt)
    }
    /// Storage-Preis $/h für `disk_gb`.
    pub fn storage_usd_h(&self, disk_gb: i64) -> f64 {
        self.storage_or(0.0) * disk_gb as f64 / 30.0 / 24.0
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Instance {
    pub id: i64,
    pub label: Option<String>,
    pub machine_id: Option<i64>,
    pub gpu_name: Option<String>,
    pub image: Option<String>,
    /// vast: "running" | "loading" | "stopped" | "error" | ... — null während Provisioning.
    pub actual_status: Option<String>,
    pub intended_status: Option<String>,
    pub cur_state: Option<String>,
    pub next_state: Option<String>,
    pub status: Option<String>,
    pub bid_value: Option<f64>,
    pub dph_total: Option<f64>,
    pub min_bid: Option<f64>,
    pub storage_total: Option<f64>,
    pub storage_cost: Option<f64>,
    pub inet_down_cost: Option<f64>,
    pub extra_env: serde_json::Value,
}

impl Instance {
    pub fn actual_or(&self, alt: &str) -> String {
        self.actual_status.clone().unwrap_or_else(|| alt.to_string())
    }
    pub fn intended_or(&self, alt: &str) -> String {
        self.intended_status.clone().unwrap_or_else(|| alt.to_string())
    }
    pub fn label_or<'a>(&'a self, alt: &'a str) -> &'a str {
        self.label.as_deref().unwrap_or(alt)
    }
    pub fn gpu_or<'a>(&'a self, alt: &'a str) -> &'a str {
        self.gpu_name.as_deref().unwrap_or(alt)
    }
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
    #[serde(rename = "client_id")]
    pub client: &'a str, // "me"
    pub image: &'a str,
    pub price: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template_hash_id: Option<&'a str>,
    #[serde(rename = "onstart", skip_serializing_if = "Option::is_none")]
    pub onstart_cmd: Option<&'a str>,
    /// "args" = Plain-Docker-Run des Image-Entrypoints (supervisord).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtype: Option<&'a str>,
    pub env: serde_json::Value,
    pub extra: Option<String>,
    pub image_login: Option<String>,
    pub python_utf8: bool,
    pub lang_utf8: bool,
    pub use_jupyter_lab: bool,
    pub jupyter_dir: Option<String>,
    pub force: bool,
    pub cancel_unavail: bool,
    pub user: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct LogEntry {
    #[serde(default)]
    pub timestamp: f64,
    #[serde(default)]
    pub log: String,
}