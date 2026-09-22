//! Actual usage charges, NOT payment invoices or account balance deltas.
//! https://docs.vast.ai/api-reference/billing/show-charges
use crate::Vast;
use anyhow::{ensure, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;

#[derive(Debug, Clone, Deserialize)]
pub struct Charge {
    #[serde(rename = "type")]
    pub kind: String,
    pub source: String,
    pub amount: f64,
    #[serde(default)]
    pub metadata: Option<ChargeMetadata>,
}
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChargeMetadata {
    #[serde(default)]
    pub label: Option<String>,
}
impl Charge {
    pub fn instance_id(&self) -> Option<i64> {
        if self.kind != "instance" {
            return None;
        }
        self.source
            .strip_prefix("instance-")?
            .parse::<i64>()
            .ok()
            .filter(|id| *id > 0)
    }
}
#[derive(Deserialize)]
struct Page {
    success: bool,
    count: usize,
    total: usize,
    // Value deliberately requires the field; missing isn't proof of a complete page.
    next_token: Value,
    results: Vec<Charge>,
}
impl Vast {
    /// Complete immutable result or an error. Never return half an invoice window.
    pub async fn charges(&self, from_unix: i64, through_unix: i64) -> Result<Vec<Charge>> {
        ensure!(
            from_unix >= 0 && through_unix >= from_unix,
            "invalid charge date range"
        );
        let filters =
            json!({"day":{"gte":from_unix,"lte":through_unix},"type":{"in":["instance"]}})
                .to_string();
        let mut cursor: Option<String> = None;
        let mut tokens = HashSet::new();
        let mut sources = HashSet::new();
        let mut expected = None;
        let mut result = Vec::new();
        for _ in 0..32 {
            // This endpoint advertises a stricter ~1 req/s limit than instance/search calls.
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let mut params = vec![
                ("select_filters", filters.as_str()),
                ("format", "table"),
                ("limit", "500"),
            ];
            if let Some(token) = &cursor {
                params.push(("after_token", token));
            }
            let page: Page = self.get("/charges", &params).await?;
            ensure!(
                page.success && page.count == page.results.len(),
                "Vast charges: unsuccessful/incomplete page"
            );
            if let Some(total) = expected {
                ensure!(
                    page.total == total,
                    "Vast charges: inventory changed during pagination"
                );
            } else {
                expected = Some(page.total);
            }
            for row in &page.results {
                ensure!(
                    sources.insert(row.source.clone()),
                    "Vast charges: duplicate source; refusing partial result"
                );
                if row.kind == "instance" {
                    ensure!(
                        row.instance_id().is_some() && row.amount.is_finite() && row.amount >= 0.0,
                        "invalid instance charge"
                    );
                }
            }
            result.extend(page.results);
            ensure!(
                result.len() <= page.total,
                "Vast charges: result count exceeds total"
            );
            match page.next_token {
                Value::Null => {
                    ensure!(
                        result.len() == page.total,
                        "Vast charges: missing rows; refusing partial result"
                    );
                    return Ok(result);
                }
                Value::String(token) if !token.is_empty() => {
                    ensure!(
                        page.count > 0 && tokens.insert(token.clone()),
                        "Vast charges: non-progressing pagination"
                    );
                    cursor = Some(token);
                }
                _ => anyhow::bail!("Vast charges: invalid pagination token"),
            }
        }
        anyhow::bail!("Vast charges: pagination limit exceeded; refusing partial result")
    }
}
