//! Shared, read-only availability for the dashboard and agent health endpoint.
use super::config::Config;
use crate::db::{free_router_store::Quota, Db};
use serde::Serialize;

#[derive(Serialize)]
pub struct ProviderStatus {
    pub id: String,
    pub enabled: bool,
    pub key_configured: bool,
    pub stored_key: bool,
    pub quota: Option<Quota>,
}
#[derive(Serialize)]
pub struct Status {
    /// No master switch: configured/enabled providers automatically activate it.
    pub active: bool,
    pub available: bool,
    pub retry_after_s: u64,
    pub providers: Vec<ProviderStatus>,
}

pub fn snapshot(db: &Db, cfg: &Config, now: i64) -> Status {
    let providers: Vec<_> = cfg.providers.iter().map(|p| {
        let key = p.key();
        ProviderStatus {
            id: p.id.clone(), enabled: p.enabled, key_configured: !key.is_empty(), stored_key: !p.api_key.is_empty(),
            quota: db.free_router_quota(&p.bucket(&key), p, p.buffer(cfg), 1, now).ok(),
        }
    }).collect();
    let eligible = || providers.iter().filter(|p| p.enabled && p.key_configured);
    let active = eligible().next().is_some();
    let available = eligible().any(|p| p.quota.as_ref().is_some_and(|q| q.retry_after_s == 0));
    let retry_after_s = if available { 0 } else {
        eligible().map(|p| p.quota.as_ref().map_or(10, |q| q.retry_after_s.max(1))).min().unwrap_or(60)
    };
    Status { active, available, retry_after_s, providers }
}
