//! Vast.ai API-Client (v0). Poll-Intervall ≥ 30 s, Aktionen mit einfachem
//! Backoff (Ratelimit-Fehler → Retry).

mod query;
mod types;

pub use types::{CreateInstanceParams, CurrentUser, Instance, LogEntry, Offer, OfferType};

use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use serde::de::DeserializeOwned;
use std::time::Duration;

const BASE: &str = "https://console.vast.ai/api/v0";
const BASE_V1: &str = "https://console.vast.ai/api/v1";

#[derive(Clone)]
pub struct Vast {
    key: String,
    http: Client,
    /// Vast Rate-Limit ~5 req/s (429 Too Many Requests): mind. 350 ms Abstand.
    last: std::sync::Arc<std::sync::Mutex<std::time::Instant>>,
}

impl Vast {
    pub fn new(key: impl Into<String>) -> Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("http client")?;
        Ok(Self {
            key: key.into(),
            http,
            last: std::sync::Arc::new(std::sync::Mutex::new(
                std::time::Instant::now() - Duration::from_secs(10),
            )),
        })
    }

    /// Mindestabstand zwischen Requests (429-Prophylaxe).
    async fn throttle(&self) {
        const MIN_GAP: Duration = Duration::from_millis(350);
        loop {
            let wait = {
                let mut l = self.last.lock().unwrap();
                let now = std::time::Instant::now();
                match now.checked_duration_since(*l) {
                    Some(elapsed) if elapsed >= MIN_GAP => {
                        *l = now;
                        return;
                    }
                    Some(elapsed) => MIN_GAP - elapsed,
                    None => Duration::from_millis(50),
                }
            };
            tokio::time::sleep(wait).await;
        }
    }

    /// Auf 429 mit retry_after warten (für den Wiederholungsversuch).
    async fn wait_429(&self, retry_after: f64) {
        tokio::time::sleep(Duration::from_millis((retry_after * 1000.0 + 300.0) as u64)).await;
    }

    /// `path`: v0-relativ ("/instances/…") oder absolut ("/api/v1/…").
    fn url_for(&self, path: &str) -> String {
        const ROOT: &str = "https://console.vast.ai";
        if path.starts_with("/api/") {
            format!("{ROOT}{path}")
        } else {
            format!("{ROOT}{BASE}{path}")
        }
    }

    async fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, &str)]) -> Result<T> {
        let url = self.url_for(path);
        let mut q: Vec<(&str, &str)> = vec![];
        if !query.is_empty() {
            q.extend_from_slice(query);
        }
        for attempt in 0..3 {
            self.throttle().await;
            let resp = self
                .http
                .get(url.clone())
                .bearer_auth(&self.key)
                .query(&q)
                .send()
                .await
                .inspect_err(|e| tracing::warn!(%e, url = %url, "vast GET send fehlgeschlagen"))
                .context("GET request")?;
            if resp.status().as_u16() == 429 && attempt < 2 {
                let ra: f64 = resp
                    .json::<serde_json::Value>()
                    .await
                    .ok()
                    .and_then(|v| v.get("retry_after").and_then(|x| x.as_f64()))
                    .unwrap_or(3.0);
                tracing::warn!(%path, ra, "vast 429 — warte und retry");
                self.wait_429(ra).await;
                continue;
            }
            return self.parse(resp, path).await;
        }
        anyhow::bail!("vast GET {path}: zu viele Versuche")
    }

    async fn put<T: DeserializeOwned>(&self, path: &str, body: &impl serde::Serialize) -> Result<T> {
        let url = format!("{BASE}{path}");
        for attempt in 0..3 {
            self.throttle().await;
            let resp = self.http.put(url.clone()).bearer_auth(&self.key).json(body).send().await.context("PUT request")?;
            if resp.status().as_u16() == 429 && attempt < 2 {
                let ra: f64 = resp
                    .json::<serde_json::Value>()
                    .await
                    .ok()
                    .and_then(|v| v.get("retry_after").and_then(|x| x.as_f64()))
                    .unwrap_or(3.0);
                tracing::warn!(%path, ra, "vast 429 — warte und retry");
                self.wait_429(ra).await;
                continue;
            }
            return self.parse(resp, path).await;
        }
        anyhow::bail!("vast PUT {path}: zu viele Versuche")
    }

    async fn put_ok(&self, path: &str, body: &impl serde::Serialize) -> Result<()> {
        let url = format!("{BASE}{path}");
        for attempt in 0..3 {
            self.throttle().await;
            let resp = self.http.put(url.clone()).bearer_auth(&self.key).json(body).send().await.context("PUT request")?;
            if resp.status().as_u16() == 429 && attempt < 2 {
                let ra: f64 = resp
                    .json::<serde_json::Value>()
                    .await
                    .ok()
                    .and_then(|v| v.get("retry_after").and_then(|x| x.as_f64()))
                    .unwrap_or(3.0);
                tracing::warn!(%path, ra, "vast 429 — warte und retry");
                self.wait_429(ra).await;
                continue;
            }
            return self.expect_success(resp, path).await;
        }
        anyhow::bail!("vast PUT {path}: zu viele Versuche")
    }

    async fn delete_ok(&self, path: &str) -> Result<()> {
        let url = format!("{BASE}{path}");
        for attempt in 0..3 {
            self.throttle().await;
            let resp = self.http.delete(url.clone()).bearer_auth(&self.key).send().await.context("DELETE request")?;
            if resp.status().as_u16() == 429 && attempt < 2 {
                let ra: f64 = resp
                    .json::<serde_json::Value>()
                    .await
                    .ok()
                    .and_then(|v| v.get("retry_after").and_then(|x| x.as_f64()))
                    .unwrap_or(3.0);
                tracing::warn!(%path, ra, "vast 429 — warte und retry");
                self.wait_429(ra).await;
                continue;
            }
            return self.expect_success(resp, path).await;
        }
        anyhow::bail!("vast DELETE {path}: zu viele Versuche")
    }

    async fn expect_success(&self, resp: reqwest::Response, what: &str) -> Result<()> {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("vast {what}: {status} {text}");
        }
        Ok(())
    }

    async fn parse<T: DeserializeOwned>(&self, resp: reqwest::Response, what: &str) -> Result<T> {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("vast {what}: {status} {text}");
        }
        serde_json::from_str(&text).with_context(|| format!("vast {what}: decode {text:.400}"))
    }

    /// Angebote suchen (`POST /bundles/` mit JSON-Body, wie das vastai-SDK).
    /// `interruptible` → type=bid (Preis: min_bid), sonst on-demand (dph_total).
    /// `allocated_storage` fließt ins Pricing (Storage-Kosten im Score).
    pub async fn search(
        &self,
        query: &str,
        interruptible: bool,
        allocated_storage_gb: f64,
    ) -> Result<Vec<Offer>> {
        #[derive(serde::Deserialize)]
        struct Bundles {
            offers: Vec<Offer>,
        }
        let mut q = serde_json::json!({
            "verified": {"eq": true},
            "external": {"eq": false},
            "rentable": {"eq": true},
            "rented": {"eq": false},
            "order": [["score", "desc"]],
            "limit": 64,
            "allocated_storage": allocated_storage_gb,
        });
        for (k, v) in query::parse_query(query) {
            q[&k] = v;
        }
        q["type"] = if interruptible { "bid".into() } else { "on-demand".into() };
        let url = format!("{BASE}/bundles/");
        // Wie alle anderen Calls durch den Throttle + 429-Retry — sonst wirft
        // der Reconciler-Burst (2 Slots × 2 Modi) die Suche gegen das
        // Vast-Ratelimit (5 req/s) und Create-Actions sterben an 429.
        let mut resp = None;
        for attempt in 0..3 {
            self.throttle().await;
            let r = self
                .http
                .post(url.clone())
                .bearer_auth(&self.key)
                .json(&q)
                .send()
                .await
                .context("POST bundles")?;
            if r.status().as_u16() == 429 && attempt < 2 {
                let ra: f64 = r
                    .json::<serde_json::Value>()
                    .await
                    .ok()
                    .and_then(|v| v.get("retry_after").and_then(|x| x.as_f64()))
                    .unwrap_or(3.0);
                tracing::warn!(ra, "vast search 429 — warte und retry");
                self.wait_429(ra).await;
                continue;
            }
            resp = Some(r);
            break;
        }
        let resp = resp.ok_or_else(|| anyhow!("vast search: zu viele Versuche"))?;
        let b: Bundles = self.parse(resp, "/bundles/").await?;
        Ok(b.offers)
    }

    /// Alle Instanzen (`GET /api/v1/instances/`, paginiert).
    pub async fn instances(&self) -> Result<Vec<Instance>> {
        #[derive(serde::Deserialize)]
        struct V1 {
            #[serde(default)]
            instances: Vec<Instance>,
            #[serde(default)]
            next_token: Option<String>,
        }
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        for _ in 0..10 {
            let args: Vec<(String, String)> = match &token {
                Some(t) => vec![("next_token".into(), t.clone())],
                None => vec![],
            };
            let refs: Vec<(&str, &str)> = args.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
            let page: V1 = self.get("/api/v1/instances/", &refs).await?;
            out.extend(page.instances);
            match page.next_token {
                Some(t) if !t.is_empty() => token = Some(t),
                _ => break,
            }
        }
        Ok(out)
    }

    /// Instanz mieten (`PUT /asks/{offer_id}/`).
    /// Antwortformat variiert → ID aus allen bekannten Feldern ziehen,
    /// Fallback: eigene Instanzliste nach Label durchsuchen.
    pub async fn create(&self, offer_id: i64, params: &CreateInstanceParams<'_>) -> Result<i64> {
        let url = format!("{BASE}/asks/{offer_id}/");
        let mut resp = None;
        for attempt in 0..3 {
            self.throttle().await;
            let r = self
                .http
                .put(url.clone())
                .bearer_auth(&self.key)
                .json(params)
                .send()
                .await
                .context("PUT asks")?;
            if r.status().as_u16() == 429 && attempt < 2 {
                let ra: f64 = r
                    .json::<serde_json::Value>()
                    .await
                    .ok()
                    .and_then(|v| v.get("retry_after").and_then(|x| x.as_f64()))
                    .unwrap_or(3.0);
                tracing::warn!(offer_id, ra, "vast create 429 — warte und retry");
                self.wait_429(ra).await;
                continue;
            }
            resp = Some(r);
            break;
        }
        let resp = resp.ok_or_else(|| anyhow!("vast create {}: zu viele Versuche", offer_id))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("vast create {}: {status} {text:.400}", offer_id);
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
            for key in ["new_instance", "instance", "new_contract", "contract", "id"] {
                if let Some(id) = v.get(key).and_then(|x| x.as_i64()) {
                    return Ok(id);
                }
                if let Some(id) = v.get(key).and_then(|x| x.get("id")).and_then(|x| x.as_i64()) {
                    return Ok(id);
                }
            }
        }
        // Fallback: frisch gemietete Instanz über das Label finden.
        if let Some(label) = params.label {
            let list = self.instances().await.unwrap_or_default();
            if let Some(found) = list.into_iter().find(|i| i.label.as_deref() == Some(label)) {
                tracing::info!(id = found.id, "create: Instanz über Label gefunden");
                return Ok(found.id);
            }
        }
        anyhow::bail!("create: keine Instanz-ID in Antwort: {text:.400}")
    }

    /// Stoppen/Starten (`PUT /instances/{id}/` mit {"state": ...}, Docs: manage-instance).
    pub async fn set_status(&self, id: i64, running: bool) -> Result<()> {
        let state = if running { "running" } else { "stopped" };
        self.put_ok(&format!("/instances/{id}/"), &serde_json::json!({ "state": state }))
            .await
    }

    /// Gebot ändern (`PUT /instances/bid_price/{id}/`, Docs: change-bid).
    pub async fn set_bid(&self, id: i64, price_usd_h: f64) -> Result<()> {
        self.put_ok(
            &format!("/instances/bid_price/{id}/"),
            &serde_json::json!({ "client_id": "me", "price": price_usd_h }),
        )
        .await
    }

    /// Destroy (`DELETE /instances/{id}/`).
    pub async fn destroy(&self, id: i64) -> Result<()> {
        self.delete_ok(&format!("/instances/{id}/")).await
    }

    /// Kontostand (`GET /users/current/`) — Ground Truth fürs Metering.
    pub async fn current_user(&self) -> Result<CurrentUser> {
        self.get("/users/current/", &[]).await
    }

    /// Instanz-Logs (`PUT /instances/request_logs/{id}/`, dann result_url pollen).
    pub async fn request_logs(&self, id: i64) -> Result<Vec<LogEntry>> {
        #[derive(serde::Deserialize)]
        struct First {
            #[serde(default)]
            result_url: Option<String>,
            #[serde(default)]
            logs: Vec<LogEntry>,
            #[serde(default)]
            results: Vec<LogEntry>,
        }
        let f: First = self
            .put(&format!("/instances/request_logs/{id}/"), &serde_json::json!({ "tail": 1000 }))
            .await?;
        let mut out = if f.logs.is_empty() { f.results } else { f.logs };
        if let Some(url) = f.result_url {
            for _ in 0..10 {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                let resp = self
                    .http
                    .get(&url)
                    .bearer_auth(&self.key)
                    .send()
                    .await
                    .context("poll result_url")?;
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                if !status.is_success() {
                    anyhow::bail!("logs poll: {status} {text:.200}");
                }
                let v: serde_json::Value = serde_json::from_str(&text).with_context(|| format!("logs poll decode: {text:.200}"))?;
                // result_url liefert je nach Stand {"logs": [...]} oder ein Array.
                if let Ok(mut l) = serde_json::from_value::<Vec<LogEntry>>(v.clone()) {
                    out.append(&mut l);
                    break;
                }
                if let Some(logs) = v.get("logs") {
                    if let Ok(mut l) = serde_json::from_value::<Vec<LogEntry>>(logs.clone()) {
                        out.append(&mut l);
                        break;
                    }
                }
                if v.get("pending") == Some(&serde_json::Value::Bool(true)) {
                    continue;
                }
                break;
            }
        }
        Ok(out)
    }
}