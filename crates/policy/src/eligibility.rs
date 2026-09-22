//! Local admission rules. Provider-side search predicates are not a safety boundary.
//! A whitelist expresses trust, never permission to bypass hardware/cost limits.
use crate::{BidConfig, OfferSnapshot};
use praxis_common::Mode;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Requirements {
    /// Optional additional machine-ID allowlist (independent of host trust switches).
    pub machine_ids: Vec<i64>,
    /// Exact model names, case/whitespace/underscore insensitive; no substring match.
    pub gpu_names: Vec<String>,
    pub min_gpu_ram_gb: f64,
    pub min_cpu_ram_gb: f64,
    pub min_disk_bw: f64,
    pub min_inet_down: f64,
    pub min_cuda: f64,
    pub min_reliability: f64,
    pub num_gpus: Option<u32>,
    /// Model-specific ceilings apply IN ADDITION to the slot ceiling.
    pub gpu_price_ceiling_usd_h: HashMap<String, f64>,
    pub max_storage_usd_h: Option<f64>,
    pub max_download_usd_gb: Option<f64>,
    pub max_effective_usd_h: Option<f64>,
}

pub fn gpu_key(name: &str) -> String {
    let name = name.replace('_', " ").split_whitespace().collect::<Vec<_>>().join(" ").to_ascii_lowercase();
    name.strip_prefix("nvidia ").unwrap_or(&name).to_string()
}

impl Requirements {
    pub fn validate(&self) -> Result<(), String> {
        let finite = |n: f64| n.is_finite() && n >= 0.0;
        if ![self.min_gpu_ram_gb, self.min_cpu_ram_gb, self.min_disk_bw, self.min_inet_down,
            self.min_cuda, self.min_reliability].into_iter().all(finite)
            || self.min_reliability > 1.0 {
            return Err("hardware minimums must be finite/nonnegative; reliability <= 1".into());
        }
        if self.num_gpus == Some(0) || self.machine_ids.iter().any(|id| *id <= 0)
            || self.gpu_names.iter().any(|n| gpu_key(n).is_empty()) {
            return Err("invalid GPU count, machine ID or GPU name".into());
        }
        let mut names = std::collections::HashSet::new();
        for (name, ceiling) in &self.gpu_price_ceiling_usd_h {
            if gpu_key(name).is_empty() || !names.insert(gpu_key(name)) || !finite(*ceiling) || *ceiling == 0.0 {
                return Err("invalid/duplicate model price ceiling".into());
            }
        }
        if [self.max_storage_usd_h, self.max_download_usd_gb, self.max_effective_usd_h]
            .into_iter().flatten().any(|n| !finite(n)) {
            return Err("cost ceilings must be finite and nonnegative".into());
        }
        Ok(())
    }

    pub fn hardware_rejections(&self, offer: &OfferSnapshot, whitelisted: bool, blacklisted: bool, whitelist_required: bool) -> Vec<String> {
        let mut reasons = Vec::new();
        if blacklisted { reasons.push("Host ist blacklisted (hat Vorrang vor Whitelist)".into()); }
        if whitelist_required && !whitelisted { reasons.push("Host nicht auf der Whitelist".into()); }
        if !self.machine_ids.is_empty() && !self.machine_ids.contains(&offer.machine_id) {
            reasons.push("machine_id nicht in der Slot-Allowlist".into());
        }
        if !self.gpu_names.is_empty() && !self.gpu_names.iter().any(|n| gpu_key(n) == gpu_key(&offer.gpu_name)) {
            reasons.push(format!("GPU-Modell '{}' nicht erlaubt", offer.gpu_name));
        }
        for (label, actual, minimum) in [
            ("VRAM/GPU (GiB)", offer.gpu_ram_gb, self.min_gpu_ram_gb),
            ("CPU-RAM (GiB)", offer.cpu_ram_gb, self.min_cpu_ram_gb),
            ("Disk-Bandbreite (MB/s)", offer.disk_bw, self.min_disk_bw),
            ("Download (Mbit/s)", offer.inet_down, self.min_inet_down),
            ("CUDA-Version", offer.cuda_max_good.unwrap_or(0.0), self.min_cuda),
            ("Reliability", offer.reliability2, self.min_reliability),
        ] {
            if !actual.is_finite() || actual < 0.0 || actual < minimum {
                reasons.push(format!("{label}: {actual:.2} < {minimum:.2} oder unbekannt/ungültig"));
            }
        }
        if self.num_gpus.is_some_and(|n| offer.num_gpus != n as i64) {
            reasons.push(format!("GPU-Anzahl {} passt nicht zu {:?}", offer.num_gpus, self.num_gpus));
        }
        reasons
    }

    pub fn gpu_ceiling(&self, name: &str) -> Option<f64> {
        self.gpu_price_ceiling_usd_h.iter().find(|(n, _)| gpu_key(n) == gpu_key(name)).map(|(_, price)| *price)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Assessment {
    pub eligible: bool,
    pub reasons: Vec<String>,
    pub gpu_rate_usd_h: f64,
    pub storage_usd_h: f64,
    pub effective_usd_h: f64,
    pub startup_traffic_usd: f64,
    /// Conservative estimate, not a provider invoice; warmup may exceed 30 min.
    pub estimated_first_30m_usd: f64,
}

pub fn rental_price(offer: &OfferSnapshot, mode: Mode, bid: &BidConfig) -> f64 {
    if mode == Mode::Interruptible { (offer.min_bid * (1.0 + bid.margin)).min(bid.ceiling_usd_h) }
    else { offer.dph_total }
}

pub fn assess(
    req: &Requirements, bid: &BidConfig, offer: &OfferSnapshot, mode: Mode,
    requested_price: f64, disk_gb: i64, traffic_gb: f64, whitelisted: bool, blacklisted: bool, whitelist_required: bool,
) -> Assessment {
    let mut reasons = req.hardware_rejections(offer, whitelisted, blacklisted, whitelist_required);
    let rate = if mode == Mode::Interruptible { requested_price } else { offer.dph_total };
    if !rate.is_finite() || rate <= 0.0 || rate < bid.rent_min_usd_h || rate > bid.ceiling_usd_h {
        reasons.push(format!("GPU-Preis {rate:.4} außerhalb Slot-Fenster [{:.4}, {:.4}] $/h", bid.rent_min_usd_h, bid.ceiling_usd_h));
    }
    if mode == Mode::Interruptible && (!offer.min_bid.is_finite() || offer.min_bid <= 0.0 || rate < offer.min_bid) {
        reasons.push("Kein gültiges Interruptible-Angebot / Gebot unter min_bid".into());
    }
    if let Some(max) = req.gpu_ceiling(&offer.gpu_name) {
        if rate > max { reasons.push(format!("GPU-Modell-Ceiling überschritten: {rate:.4} > {max:.4} $/h")); }
    }
    if disk_gb <= 0 || !offer.disk_gb.is_finite() || offer.disk_gb < disk_gb as f64 {
        reasons.push(format!("Disk zu klein/unbekannt: {:.1} GB angeboten, {disk_gb} GB benötigt", offer.disk_gb));
    }
    for (name, value) in [("Storage-Preis", offer.storage_cost), ("Download-Preis", offer.inet_down_cost), ("Download-GB", traffic_gb)] {
        if !value.is_finite() || value < 0.0 { reasons.push(format!("{name} ungültig")); }
    }
    let storage = offer.storage_cost * disk_gb as f64 / 720.0;
    let traffic = offer.inet_down_cost * traffic_gb;
    if !storage.is_finite() || !traffic.is_finite() || !(rate + storage).is_finite() {
        reasons.push("Kostenberechnung nicht endlich".into());
    }
    for (name, actual, limit) in [
        ("Storage $/h", storage, req.max_storage_usd_h),
        ("Download $/GB", offer.inet_down_cost, req.max_download_usd_gb),
        ("GPU + Storage $/h", rate + storage, req.max_effective_usd_h),
    ] {
        if let Some(max) = limit { if actual > max { reasons.push(format!("{name}: {actual:.4} > {max:.4}")); } }
    }
    Assessment { eligible: reasons.is_empty(), reasons, gpu_rate_usd_h: rate,
        storage_usd_h: storage, effective_usd_h: rate + storage, startup_traffic_usd: traffic,
        estimated_first_30m_usd: (rate + storage) * 0.5 + traffic }
}
