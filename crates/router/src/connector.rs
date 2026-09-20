//! Agent-Connector: der ROUTER wählt sich aktiv in die gpu-agents ein
//! (Agent lauscht auf :9100; Outbound der Box bräuchte eBPF, das es auf
//! unprivilegierten Vast-Containern nicht gibt — Inbound funktioniert
//! über NetBird local forwarding zuverlässig).
//!
//! Loop (10 s): NetBird-Peers nach Hostnamen `gpu-<role>-<token8>` der
//! bekannten Instanzen durchsuchen → nb_ip in DB pflegen → WS-Client
//! `ws://<nb_ip>:9100` aufbauen → Hello prüfen → Session wie /api/v1/node.

use crate::state::SharedApp;
use anyhow::Result;
use futures::{SinkExt, StreamExt};
use praxis_common::node::NodeMessage;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

/// Laufende Connector-Tasks pro Instanz.
static CONNECTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub async fn run(app: SharedApp) {
    loop {
        if let Err(e) = tick(&app).await {
            tracing::debug!(%e, "connector tick");
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

async fn tick(app: &SharedApp) -> Result<()> {
    let active: Vec<crate::db::InstanceRow> = app
        .db
        .instances(false)
        .into_iter()
        .filter(|r| {
            matches!(
                r.state.as_str(),
                "requested" | "provisioning" | "booting" | "agent_connected" | "healthy" | "unreachable" | "preempted"
            )
        })
        .collect();
    if active.is_empty() {
        return Ok(());
    }
    let peers = fetch_peers(app).await.unwrap_or_default();
    for inst in active {
        // Bereits verbunden? (Heartbeat vorhanden)
        if app.hub.heartbeat(inst.vast_id).is_some() {
            continue;
        }
        let hostname = format!("gpu-{}-{}", crate::api::role_str(inst.role), &inst.node_token[..8]);
        // Peer über Name ODER hostname-Feld ODER dns_label finden: --hostname
        // setzt `name`, aber ältere Enrollments/umbenannte Peers können auch
        // nur in einem der Felder auftauchen (dns_label ggf. mit -xx-yy-Suffix
        // dedupliziert). Vast-Boxen ohne den Fork-Patch enrollen unter
        // Container-ID → dann bleibt nur der nb_ip-Fallback.
        let ip = peers
            .iter()
            .find(|p| p.matches(&hostname))
            .map(|p| p.ip.clone())
            .or_else(|| inst.nb_ip.clone());
        let Some(ip) = ip else {
            continue;
        };
        if inst.nb_ip.as_deref() != Some(ip.as_str()) {
            let _ = app.db.update_instance_agent(inst.vast_id, Some(&ip), false, "booting");
        }
        tracing::info!(vast_id = inst.vast_id, %ip, "verbinde mit gpu-agent :9100");
        let app2 = app.clone();
        let tok = inst.node_token.clone();
        let ip2 = ip.clone();
        CONNECTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tokio::spawn(async move {
            if let Err(e) = dial_session(&app2, &ip2, &tok).await {
                tracing::debug!(%e, "agent-session beendet");
            }
            CONNECTED.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        });
        // pro Takt höchstens 3 neue Wählversuche (Rate-Gnade).
    }
    Ok(())
}

/// NetBird-Peers via Management-API (name/hostname/dns_label → ip).
#[derive(Debug, Clone)]
struct PeerInfo {
    name: String,
    hostname: String,
    dns_label: String,
    ip: String,
}

impl PeerInfo {
    /// Entspricht der erwarteten Router-seitigen Hostname-Konvention
    /// `gpu-<role>-<tok8>` in jedem der drei Namensfelder.
    fn matches(&self, expected: &str) -> bool {
        let exp = expected.to_ascii_lowercase();
        if self.name.eq_ignore_ascii_case(expected) || self.hostname.eq_ignore_ascii_case(expected) {
            return true;
        }
        // dns_label ist lowercased; Suffixe: ".<domain>" oder dedup "-xx-yy".
        let dl = self.dns_label.as_str();
        dl == exp || dl.starts_with(&format!("{exp}.")) || dl.starts_with(&format!("{exp}-"))
    }
}

async fn fetch_peers(app: &SharedApp) -> Result<Vec<PeerInfo>> {
    let token = app.cfg.netbird.api_token.clone();
    if token.is_empty() {
        return Ok(vec![]);
    }
    let base = app.cfg.netbird.api_url.trim_end_matches('/');
    let resp = reqwest::Client::new()
        .get(format!("{base}/api/peers"))
        .bearer_auth(&token)
        .timeout(Duration::from_secs(15))
        .send()
        .await?;
    let peers: Vec<serde_json::Value> = resp.json().await.unwrap_or_default();
    Ok(peers
        .iter()
        .filter_map(|p| {
            let name = p.get("name").and_then(|n| n.as_str())?;
            let ip = p.get("ip").and_then(|i| i.as_str())?;
            let hostname = p.get("hostname").and_then(|h| h.as_str()).unwrap_or("");
            let dns_label = p.get("dns_label").and_then(|d| d.as_str()).unwrap_or("");
            Some(PeerInfo { name: name.to_string(), hostname: hostname.to_string(), dns_label: dns_label.to_string(), ip: ip.to_string() })
        })
        .collect())
}

/// WS-Session zum Agent: Hello → Auth → gleiche Loop wie node.rs.
async fn dial_session(app: &SharedApp, ip: &str, expect_token: &str) -> Result<()> {
    let url = format!("ws://{ip}:9100");
    let (mut sink, mut stream) = tokio_tungstenite::connect_async(&url).await?.0.split();

    // Hello abwarten (Agent sendet zuerst).
    let first = tokio::time::timeout(Duration::from_secs(15), stream.next()).await?;
    let raw = match first {
        Some(Ok(Message::Text(t))) => t.to_string(),
        Some(Ok(_)) => anyhow::bail!("kein Hello-Frame"),
        Some(Err(e)) => anyhow::bail!("WS-Fehler beim Hello: {e}"),
        None => anyhow::bail!("Verbindung vor Hello geschlossen"),
    };
    let msg: NodeMessage = serde_json::from_str(&raw)?;
    let NodeMessage::Hello { token, role, agent_version, nb_ip, hostname, services } = msg else {
        anyhow::bail!("erstes Frame war kein Hello");
    };
    if token != expect_token {
        anyhow::bail!("Token-Mismatch von {hostname:?}");
    }
    let Some(inst) = app.db.instance_by_token(&token) else {
        anyhow::bail!("Token unbekannt");
    };
    let vast_id = inst.vast_id;
    tracing::info!(vast_id, role, %agent_version, "agent-session (router-dial) etabliert");
    app.events.emit(
        &app.db,
        "agent_connected",
        Some(inst.slot_id),
        Some(vast_id),
        &format!("Router→Agent {hostname:?} ({role}) verbunden"),
        &serde_json::json!({"dial": true}),
    );
    let _ = app.db.update_instance_agent(vast_id, nb_ip.as_deref(), false, "agent_connected");
    let mut rx = app.hub.register(vast_id, inst.slot_id, nb_ip.clone(), services.clone());
    app.reconcile_now.notify_one();

    // Asset-Push nach Hello (Outbound-Pull der Boxen ist auf Vast unzuverlässlich
    // — der Router schiebt Manifest + Bytes über genau diese Session).
    {
        let app2 = app.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::assets::push_assets(&app2, vast_id).await {
                tracing::warn!(%e, vast_id, "asset-push fehlgeschlagen");
            }
        });
    }

    let mut last_health: Option<serde_json::Value> = None;
    loop {
        tokio::select! {
            inbound = stream.next() => {
                match inbound {
                    Some(Ok(Message::Text(raw))) => {
                        match serde_json::from_str::<NodeMessage>(&raw) {
                            Ok(NodeMessage::Hello { .. }) => {}
                            Ok(NodeMessage::Heartbeat { health, busy, busy_reason, gpu, progress, disk_free_gb, .. }) => {
                                let health_json = serde_json::to_value(&health).unwrap_or_default();
                                let hb = crate::hub::HeartbeatData {
                                    health_json: health_json.clone(),
                                    busy,
                                    busy_reason: busy_reason.clone(),
                                    gpu_json: serde_json::to_value(&gpu).unwrap_or_default(),
                                    progress_json: serde_json::to_value(&progress).unwrap_or_default(),
                                    disk_free_gb,
                                };
                                let health_changed = last_health.as_ref() != Some(&health_json);
                                last_health = Some(health_json);
                                app.hub.record_heartbeat(vast_id, hb, None);
                                let healthy = matches!(health, praxis_common::AgentHealth::Healthy);
                                let state = if healthy { "healthy" } else { "booting" };
                                let _ = app.db.update_instance_agent(vast_id, None, healthy, state);
                                if health_changed {
                                    app.reconcile_now.notify_one();
                                }
                            }
                            Ok(NodeMessage::Event { kind, payload }) => {
                                app.events.emit(&app.db, &kind, Some(inst.slot_id), Some(vast_id), &format!("{kind} von Agent {vast_id}"), &payload);
                            }
                            Ok(NodeMessage::CmdResult { id, ok, data }) => {
                                app.hub.resolve(id, if ok { Ok(data) } else { Err(data.get("error").and_then(|e| e.as_str()).unwrap_or("command failed").to_string()) });
                            }
                            Ok(NodeMessage::Term { .. }) | Ok(NodeMessage::TermEnd { .. }) => {
                                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
                                    app.hub.relay_term(v);
                                }
                            }
                            Err(e) => tracing::debug!(%e, "bad agent frame"),
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => anyhow::bail!("Agent getrennt"),
                    Some(Ok(_)) => {}
                    Some(Err(e)) => anyhow::bail!("WS-Fehler: {e}"),
                }
            }
            from_hub = async {
                rx.recv().await
            } => {
                if let Some(cmd) = from_hub {
                    let json = serde_json::to_string(&cmd)?;
                    if sink.send(Message::Text(json.into())).await.is_err() {
                        anyhow::bail!("send failed");
                    }
                }
            }
        }
    }
}