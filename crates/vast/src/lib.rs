//! Vast.ai API-Client (v0). Poll-Intervall ≥ 30 s, Aktionen mit einfachem
//! Backoff (Ratelimit-Fehler → Retry).

mod types;

pub use types::{CreateInstanceParams, CurrentUser, Instance, LogEntry, Offer, OfferType};

use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use serde::de::DeserializeOwned;
use std::time::Duration;

const BASE: &str = "https://console.vast.ai/api/v0";

#[derive(Clone)]
pub struct Vast {
    key: String,
    http: Client,
}

impl Vast {
    pub fn new(key: impl Into<String>) -> Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("http client")?;
        Ok(Self { key: key.into(), http })
    }

    async fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, &str)]) -> Result<T> {
        let url = format!("{BASE}{path}");
        let mut q: Vec<(&str, &str)> = vec![];
        if !query.is_empty() {
            q.extend_from_slice(query);
        }
        let resp = self
            .http
            .get(url)
            .bearer_auth(&self.key)
            .query(&q)
            .send()
            .await
            .context("GET request")?;
        self.parse(resp, path).await
    }

    async fn put<T: DeserializeOwned>(&self, path: &str, body: &impl serde::Serialize) -> Result<T> {
        let url = format!("{BASE}{path}");
        let resp = self.http.put(url).bearer_auth(&self.key).json(body).send().await.context("PUT request")?;
        self.parse(resp, path).await
    }

    async fn put_ok(&self, path: &str, body: &impl serde::Serialize) -> Result<()> {
        let url = format!("{BASE}{path}");
        let resp = self.http.put(url).bearer_auth(&self.key).json(body).send().await.context("PUT request")?;
        self.expect_success(resp, path).await
    }

    async fn delete_ok(&self, path: &str) -> Result<()> {
        let url = format!("{BASE}{path}");
        let resp = self.http.delete(url).bearer_auth(&self.key).send().await.context("DELETE request")?;
        self.expect_success(resp, path).await
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

    /// Angebote suchen (`GET /bundles/?q=...&type=on-demand|interruptible`).
    pub async fn search(&self, query: &str, offer_type: Option<OfferType>) -> Result<Vec<Offer>> {
        let ty = match offer_type {
            None | Some(OfferType::Interruption) => "on-demand",
            Some(OfferType::OnDemand) => "on-demand",
        };
        #[derive(serde::Deserialize)]
        struct Bundles {
            offers: Vec<Offer>,
        }
        let b: Bundles = self
            .get(
                "/bundles/",
                &[("q", query), ("type", ty), ("order", "-score"), ("limit", "64")],
            )
            .await?;
        Ok(b.offers)
    }

    /// Alle Instanzen (`GET /instances/`).
    pub async fn instances(&self) -> Result<Vec<Instance>> {
        #[derive(serde::Deserialize)]
        struct Instances {
            instances: Vec<Instance>,
        }
        let i: Instances = self.get("/instances/", &[]).await?;
        Ok(i.instances)
    }

    /// Instanz mieten (`PUT /asks/{offer_id}/`).
    pub async fn create(&self, offer_id: i64, params: &CreateInstanceParams<'_>) -> Result<Instance> {
        #[derive(serde::Deserialize)]
        struct New {
            #[serde(default)]
            new_instance: Option<Instance>,
            #[serde(default)]
            instance: Option<Instance>,
        }
        let n: New = self.put(&format!("/asks/{offer_id}/"), params).await?;
        n.new_instance.or(n.instance).ok_or_else(|| anyhow!("create: keine Instanz in Antwort"))
    }

    /// Stoppen/Starten (`PUT /instances/{id}/` mit actual_status).
    pub async fn set_status(&self, id: i64, running: bool) -> Result<()> {
        let status = if running { "running" } else { "stopped" };
        self.put_ok(&format!("/instances/{id}/"), &serde_json::json!({ "actual_status": status }))
            .await
    }

    /// Gebot ändern (`PUT /instances/bid_price/{id}/`).
    pub async fn set_bid(&self, id: i64, price_usd_h: f64) -> Result<()> {
        self.put_ok(&format!("/instances/bid_price/{id}/"), &serde_json::json!({ "price": price_usd_h }))
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

    /// Instanz-Logs (`GET /instances/request_logs/{id}/`).
    pub async fn request_logs(&self, id: i64) -> Result<Vec<LogEntry>> {
        #[derive(serde::Deserialize)]
        struct Logs {
            #[serde(default)]
            logs: Vec<LogEntry>,
            #[serde(default)]
            results: Vec<LogEntry>,
        }
        let l: Logs = self.get(&format!("/instances/request_logs/{id}/"), &[]).await?;
        Ok(if l.logs.is_empty() { l.results } else { l.logs })
    }
}