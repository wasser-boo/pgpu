//! Conservative, retryable cleanup of peers belonging to retired pgpu rentals.
//! Offline is NOT unused: any contract still in Vast (including sleep/stopped)
//! and every live local ownership record protects its peer. Unknown peers stay.
use crate::{db::InstanceRow, state::SharedApp};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::collections::HashSet;

const INTERVAL_S: i64 = 300;
const GRACE_S: i64 = 300;
const LAST_ATTEMPT: &str = "netbird_peer_cleanup_last_attempt";

#[derive(Debug, Deserialize)]
struct Peer {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    hostname: Option<String>,
    #[serde(default)]
    dns_label: Option<String>,
    #[serde(default)]
    connected: Option<bool>,
}

fn hostname(row: &InstanceRow) -> Option<String> {
    let token = row.node_token.get(..8)?;
    if !token.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("gpu-{}-{}", row.role, token.to_ascii_lowercase()))
}

fn label_hostname(label: &str) -> Option<String> {
    crate::slot_labels::Label::parse(label).map(|label| label.hostname())
}

fn matches(peer: &Peer, expected: &str) -> bool {
    if [&peer.name, &peer.hostname]
        .into_iter()
        .flatten()
        .any(|v| v.eq_ignore_ascii_case(expected))
    {
        return true;
    }
    let dns = peer.dns_label.as_deref().unwrap_or("").to_ascii_lowercase();
    let first = dns.split('.').next().unwrap_or("");
    first == expected
        || first
            .strip_prefix(&format!("{expected}-"))
            .is_some_and(|suffix| !suffix.is_empty() && suffix.bytes().all(|c| c.is_ascii_digit()))
}

/// Only call with a successfully fetched, complete Vast inventory. The tick
/// invokes this after synchronization; API/network/pagination failures skip it.
pub async fn reconcile(
    app: &SharedApp,
    inventory: &[praxis_vast::Instance],
) -> anyhow::Result<usize> {
    let now = Utc::now();
    if !app
        .db
        .claim_interval(LAST_ATTEMPT, now.timestamp(), INTERVAL_S)?
    {
        return Ok(0);
    }
    cleanup(app, inventory, now).await
}

pub(crate) async fn cleanup(
    app: &SharedApp,
    inventory: &[praxis_vast::Instance],
    now: DateTime<Utc>,
) -> anyhow::Result<usize> {
    // No create/start/delete race with ownership checks or peer deletion.
    let _lock = app.management.lock().await;
    let cfg = app.cfg();
    let token = cfg.netbird_api_token();
    if !cfg.netbird.cleanup_unused_peers || token.is_empty() {
        return Ok(0);
    }
    let present: HashSet<i64> = inventory.iter().map(|v| v.id).collect();
    let rows = app.db.try_instances(true)?;
    let mut protected: Vec<String> = rows
        .iter()
        .filter(|r| {
            r.state != "destroyed" || r.destroyed_at.is_none() || present.contains(&r.vast_id)
        })
        .filter_map(hostname)
        .collect();
    // Also protect same-prefix peers of live rentals missing from this DB.
    protected.extend(
        inventory
            .iter()
            .filter_map(|v| v.label.as_deref())
            .filter_map(label_hostname),
    );
    let retired: Vec<_> = rows
        .iter()
        .filter(|r| {
            r.state == "destroyed"
                && !present.contains(&r.vast_id)
                && r.destroyed_at
                    .as_deref()
                    .and_then(crate::db::parse_iso)
                    .is_some_and(|at| now.signed_duration_since(at).num_seconds() >= GRACE_S)
        })
        .filter_map(|r| hostname(r).map(|name| (r, name)))
        .collect();
    if retired.is_empty() {
        return Ok(0);
    }

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(15))
        .build()?;
    let base = format!("{}/api/peers", cfg.netbird.api_url.trim_end_matches('/'));
    let peers: Vec<Peer> = client
        .get(&base)
        .bearer_auth(&token)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let mut deleted = 0;
    for peer in peers {
        if deleted >= 32 {
            break;
        } // Bound work per tick; remaining peers are retried.
        if peer.connected != Some(false) || protected.iter().any(|name| matches(&peer, name)) {
            continue;
        }
        let Some((row, name)) = retired.iter().find(|(_, name)| matches(&peer, name)) else {
            continue;
        };
        anyhow::ensure!(
            !peer.id.is_empty()
                && peer
                    .id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
            "invalid NetBird peer ID"
        );
        let url = format!("{base}/{}", peer.id);
        // Refetch just before deleting: a renamed/reconnected/reused peer is not ours to remove.
        let response = client.get(&url).bearer_auth(&token).send().await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            continue;
        }
        let current: Peer = response.error_for_status()?.json().await?;
        if current.id != peer.id
            || current.connected != Some(false)
            || !matches(&current, name)
            || protected.iter().any(|name| matches(&current, name))
        {
            continue;
        }
        let response = client.delete(&url).bearer_auth(&token).send().await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            continue;
        }
        response.error_for_status()?;
        deleted += 1;
        app.events.emit(
            &app.db,
            "netbird_peer_deleted",
            Some(row.slot_id),
            Some(row.vast_id),
            "NetBird-Peer einer entfernten GPU bereinigt (Vast-Abgleich bestätigt)",
            &serde_json::json!({"peer_id":peer.id,"hostname":name}),
        );
    }
    Ok(deleted)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod name_tests {
    use super::*;

    fn peer(name: &str, dns: &str) -> Peer {
        Peer {
            id: "test".into(),
            name: Some(name.into()),
            hostname: None,
            dns_label: Some(dns.into()),
            connected: Some(false),
        }
    }

    #[test]
    fn peer_names_require_ownership_boundaries() {
        let expected = "gpu-llm-deadbeef";
        for p in [
            peer("GPU-LLM-DEADBEEF", ""),
            peer("", "gpu-llm-deadbeef.netbird.selfhosted"),
            peer("", "gpu-llm-deadbeef-2.netbird.selfhosted"),
        ] {
            assert!(matches(&p, expected));
        }
        for dns in [
            "gpu-llm-deadbeef0.example",
            "gpu-llm-deadbeef-other.example",
            "other-gpu-llm-deadbeef.example",
        ] {
            assert!(!matches(&peer("", dns), expected));
        }
        assert_eq!(
            label_hostname("praxis-llm-s1-deadbeef").as_deref(),
            Some(expected)
        );
        assert!(label_hostname("someone-else-s1-deadbeef").is_none());
    }
}
