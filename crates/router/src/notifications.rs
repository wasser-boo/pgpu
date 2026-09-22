//! Alert/test orchestration. Transport and payload formatting live in webhook.
use crate::{state::SharedApp, webhook};
use futures::StreamExt;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct TargetResult {
    pub target: usize,
    pub host: String,
    pub format: webhook::Format,
    pub ok: bool,
    pub status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TestReport {
    pub ok: bool,
    pub sent: usize,
    pub failed: usize,
    pub results: Vec<TargetResult>,
}
impl std::fmt::Display for TestReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Testnachricht: {} von {} Webhooks angenommen",
            self.sent,
            self.results.len()
        )?;
        for result in &self.results {
            write!(f, "; Ziel {} ({}): ", result.target, result.host)?;
            if let Some(error) = &result.error {
                write!(f, "{error}")?;
            } else {
                write!(f, "HTTP {} ✓", result.status.unwrap_or(0))?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TestError {
    #[error("Kein Webhook konfiguriert. [alerts].webhook_urls setzen, speichern und dann testen.")]
    NotConfigured,
    #[error("Bitte mindestens 10 Sekunden zwischen Webhook-Tests warten.")]
    RateLimited,
    #[error("Webhook-Test konnte nicht reserviert werden (Datenbankfehler).")]
    Storage,
    #[error(transparent)]
    Configuration(#[from] webhook::Failure),
}
impl TestError {
    pub fn status(&self) -> axum::http::StatusCode {
        use axum::http::StatusCode;
        match self {
            Self::NotConfigured => StatusCode::BAD_REQUEST,
            Self::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            Self::Storage => StatusCode::INTERNAL_SERVER_ERROR,
            Self::Configuration(_) => StatusCode::BAD_REQUEST,
        }
    }
}

/// One harmless test per deduplicated SAVED endpoint. A failed target cannot
/// block another; no URL/message override is accepted. Callers authenticate.
pub async fn test(app: &SharedApp) -> Result<TestReport, TestError> {
    let targets = webhook::targets(&app.cfg().alerts)?;
    if targets.is_empty() {
        return Err(TestError::NotConfigured);
    }
    if !app
        .db
        .claim_interval(
            "webhook_test_last_attempt",
            chrono::Utc::now().timestamp(),
            10,
        )
        .map_err(|_| TestError::Storage)?
    {
        return Err(TestError::RateLimited);
    }
    let mut results: Vec<TargetResult> = futures::stream::iter(targets).map(|target| async move {
        let host = target.host();
        let result = webhook::send(&target, "webhook_test",
            "✅ PGPU-Testnachricht: Die gespeicherte Webhook-Konfiguration funktioniert.\nDies ist nur ein Verbindungstest — keine GPU wurde gestartet, gestoppt oder gemietet.").await;
        let (ok, status, error) = match result {
            Ok(delivery) => (true, Some(delivery.status), None),
            Err(error) => (false, error.status, Some(error.to_string())),
        };
        let report = TargetResult { target: target.number, host, format: target.format, ok, status, error };
        app.events.emit(&app.db, if ok { "webhook_test_sent" } else { "webhook_test_failed" }, None, None,
            &format!("Test-Ziel {} ({}): {}", report.target, report.host,
                report.error.as_deref().unwrap_or("angenommen")), &serde_json::json!({"result":report}));
        report
    }).buffer_unordered(4).collect().await;
    results.sort_by_key(|r| r.target);
    let sent = results.iter().filter(|r| r.ok).count();
    let report = TestReport {
        ok: sent == results.len(),
        sent,
        failed: results.len() - sent,
        results,
    };
    app.events.emit(
        &app.db,
        "webhook_test_finished",
        None,
        None,
        &report.to_string(),
        &serde_json::json!({"report":report}),
    );
    Ok(report)
}

/// Do not block the lifecycle controller on an external notification service.
/// Bounded retries for transient errors only; failures are visible in Events.
pub fn alert(app: &SharedApp, kind: &str, message: &str) {
    let targets = match webhook::targets(&app.cfg().alerts) {
        Ok(targets) => targets,
        Err(error) => {
            app.events.emit(
                &app.db,
                "webhook_failed",
                None,
                None,
                &error.to_string(),
                &serde_json::json!({"alert_kind":kind}),
            );
            return;
        }
    };
    // Each destination owns its retry budget; a broken endpoint never cancels its siblings.
    for target in targets {
        spawn_target(app.clone(), target, kind.into(), message.into());
    }
}

/// Creation caller invokes this only after provider success and durable ownership.
/// Only explicitly selected public facts are formatted, never node tokens/env/config.
pub fn rented(
    app: &SharedApp,
    slot: &crate::config::SlotCfg,
    row: &crate::db::InstanceRow,
    offer: &praxis_policy::OfferSnapshot,
    disk_gb: i64,
    download_usd: f64,
) {
    let details = instance_details(slot, row, Some(offer));
    let message=format!("🆕 Instanz gemietet — startet noch, NICHT einsatzbereit.\n{details}\nDisk: {disk_gb} GB · geschätzter initialer Download: {download_usd:.3} USD\nKosten sind Angebotswerte; Traffic zusätzlich. Eine separate Nachricht folgt nach Health-Checks und Router-Freigabe.\nZeitpunkt: {}",row.created_at);
    alert(app, "instance_rented", &message);
}

fn instance_details(
    slot: &crate::config::SlotCfg,
    row: &crate::db::InstanceRow,
    offer: Option<&praxis_policy::OfferSnapshot>,
) -> String {
    let short = |s: &str| s.chars().take(160).collect::<String>();
    let hardware = offer
        .map(|o| {
            format!(
                " · {:.1} GiB VRAM/GPU · {:.1} GiB RAM · {} GPU(s)",
                o.gpu_ram_gb, o.cpu_ram_gb, o.num_gpus
            )
        })
        .unwrap_or_default();
    let location = offer
        .and_then(|o| o.geolocation.as_deref())
        .map(short)
        .unwrap_or_else(|| "unbekannt".into());
    let rate = row.compute_usd_h();
    let storage = row.storage_usd_h;
    format!("Slot {} ({}) · Instanz {} · Host {}\nGPU: {}{hardware}\nStandort: {location} · Modus: {}\nMiete: {rate:.4} USD/h · Speicher: {storage:.4} USD/h · zusammen: {:.4} USD/h (ohne Traffic)",slot.id,short(&slot.name),row.vast_id,row.machine_id,short(&row.gpu_name),row.mode,rate+storage)
}

mod lifecycle;
pub use lifecycle::run;

#[cfg(test)]
mod tests_lifecycle;

fn backend_ready(app: &SharedApp, row: &crate::db::InstanceRow) -> bool {
    backend_ready_at(app, row, chrono::Utc::now().timestamp())
}
fn backend_ready_at(app: &SharedApp, row: &crate::db::InstanceRow, now: i64) -> bool {
    let cfg = app.cfg();
    let Some(slot) = cfg.slot(row.slot_id) else {
        return false;
    };
    // No probes means legacy agents can optimistically report healthy.
    if !slot
        .services
        .values()
        .any(|s| s.health.as_ref().is_some_and(|h| !h.is_empty()))
    {
        return false;
    }
    if row.role != slot.role
        || row.destroyed_at.is_some()
        || !row.healthy
        || row.state != "healthy"
        || row.actual_status != "running"
        || row.intended_status != "running"
    {
        return false;
    }
    if !app
        .pool_routes
        .healthy(row.slot_id)
        .iter()
        .any(|(id, _)| *id == row.vast_id)
    {
        return false;
    }
    let Some(hb) = app.hub.heartbeat(row.vast_id) else {
        return false;
    };
    let age = now - app.hub.last_seen(row.vast_id).unwrap_or(0);
    hb.health_json == serde_json::json!("healthy") && (0..=60).contains(&age)
}

/// Called after pool publication; a DB flag or agent connection alone is not readiness.
pub fn ready(app: &SharedApp, row: &crate::db::InstanceRow) {
    if app.shutting_down.load(std::sync::atomic::Ordering::Relaxed)
        || !webhook::targets(&app.cfg().alerts).is_ok_and(|targets| !targets.is_empty())
        || !backend_ready(app, row)
    {
        return;
    }
    let cfg = app.cfg();
    let Some(slot) = cfg.slot(row.slot_id) else {
        return;
    };
    let claimed = app
        .db
        .claim_once(&format!("instance_ready_notified:{}", row.vast_id));
    match claimed {
        Ok(true) => {}
        Ok(false) => return,
        Err(_) => {
            tracing::warn!(
                instance_id = row.vast_id,
                "ready notification deferred: claim storage unavailable"
            );
            return;
        }
    }
    let offer = app.db.rental_facts(row.vast_id).ok().flatten();
    let details = instance_details(slot, row, offer.as_ref());
    let message=format!("✅ Instanz einsatzbereit: Service-Healthchecks grün und im Router als Backend freigegeben.\n{details}\nBereitschaft geprüft, kein Inferenz-Benchmark ausgeführt.\nZeitpunkt: {}",crate::db::now_iso());
    app.events.emit(
        &app.db,
        "instance_ready",
        Some(row.slot_id),
        Some(row.vast_id),
        "Service-Healthchecks grün und Backend freigegeben",
        &serde_json::json!({"compute_usd_h":row.compute_usd_h(),"storage_usd_h":row.storage_usd_h}),
    );
    alert(app, "instance_ready", &message);
}

fn spawn_target(app: SharedApp, target: webhook::Target, kind: String, message: String) {
    tokio::spawn(async move {
        for attempt in 1..=3 {
            let cfg = app.cfg();
            let category_enabled = (!matches!(
                kind.as_str(),
                "instance_state_changed" | "slot_state_changed" | "slot_backend_changed"
            ) || cfg.alerts.state_changes)
                && (kind != "spend_summary" || cfg.alerts.spend_summary_interval_s > 0);
            let still_enabled = category_enabled
                && webhook::targets(&cfg.alerts).is_ok_and(|targets| {
                    targets
                        .iter()
                        .any(|t| t.url == target.url && t.format == target.format)
                });
            if !still_enabled || app.shutting_down.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            match webhook::send(&target, &kind, &message).await {
                Ok(delivery) => {
                    app.events.emit(&app.db, "webhook_sent", None, None, &format!("{kind}: {delivery}"),
                        &serde_json::json!({"alert_kind":kind,"attempt":attempt,"delivery":delivery}));
                    return;
                }
                Err(error) => {
                    let delay = error.retry_after_s.unwrap_or(2_u64.pow(attempt));
                    let retry = error.retryable && attempt < 3 && delay <= 300;
                    app.events.emit(&app.db, if retry { "webhook_retry" } else { "webhook_failed" }, None, None,
                        &format!("{kind}, Webhook {}: {error}", target.number),
                        &serde_json::json!({"alert_kind":kind,"target":target.number,"attempt":attempt,"upstream_status":error.status,"retry":retry}));
                    if !retry {
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(delay.max(1))).await;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        state::AppCtx,
        test_support::{MockProvider, TestApp},
    };
    use axum::{body::Body, extract::Request, http::StatusCode};

    fn configure(t: &TestApp, p: &MockProvider) {
        let mut cfg = (*t.app.cfg()).clone();
        cfg.alerts.webhook_url = format!("{}/webhook/SECRET", p.server.url());
        cfg.alerts.webhook_format = webhook::Format::Discord;
        t.app.cfg_swap(cfg);
    }

    #[tokio::test]
    async fn test_endpoint_is_authenticated_sends_once_and_rate_limits() {
        let t = TestApp::new();
        let p = MockProvider::new().await;
        configure(&t, &p);
        let request = Request::builder().body(Body::empty()).unwrap();
        assert_eq!(
            crate::api::webhook_test(AppCtx(t.app.clone()), request)
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert!(p.calls.lock().unwrap().is_empty());
        assert_eq!(
            crate::api::webhook_test(AppCtx(t.app.clone()), t.request("{}"))
                .await
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            crate::api::webhook_test(AppCtx(t.app.clone()), t.request("{}"))
                .await
                .status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(p.calls.lock().unwrap().len(), 1);
        assert!(t.app.db.last_event_of_kind("webhook_test_sent").is_some());
        assert!(t.app.db.instances(true).is_empty());
    }

    #[tokio::test]
    async fn missing_configuration_and_discord_errors_are_visible() {
        let t = TestApp::new();
        let p = MockProvider::new().await;
        assert_eq!(
            crate::api::webhook_test(AppCtx(t.app.clone()), t.request("{}"))
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
        configure(&t, &p);
        p.respond(404, "SECRET");
        let response = crate::api::webhook_test(AppCtx(t.app.clone()), t.request("{}")).await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = axum::body::to_bytes(response.into_body(), 8192)
            .await
            .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("404"));
        assert!(!body.contains("SECRET"));
        assert!(t.app.db.last_event_of_kind("webhook_test_failed").is_some());
    }

    #[tokio::test]
    async fn dashboard_test_requires_session_and_uses_same_delivery_path() {
        let t = TestApp::new();
        let p = MockProvider::new().await;
        configure(&t, &p);
        let unauthorized =
            crate::dashboard::do_webhook_test(AppCtx(t.app.clone()), Default::default()).await;
        assert_eq!(unauthorized.headers()["location"], "/login");
        assert!(p.calls.lock().unwrap().is_empty());
        let headers = t.request("").headers().clone();
        let sent = crate::dashboard::do_webhook_test(AppCtx(t.app.clone()), headers).await;
        assert!(sent.headers()["location"]
            .to_str()
            .unwrap()
            .contains("Testnachricht"));
        assert_eq!(p.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn automatic_alerts_report_success_and_permanent_failure() {
        for status in [200, 404] {
            let t = TestApp::new();
            let p = MockProvider::new().await;
            configure(&t, &p);
            p.respond(status, "{}");
            alert(&t.app, "budget_hard", "Limit erreicht");
            let expected = if status == 200 {
                "webhook_sent"
            } else {
                "webhook_failed"
            };
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                while t.app.db.last_event_of_kind(expected).is_none() {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            assert_eq!(p.calls.lock().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn multiple_destinations_report_partial_failure_and_do_not_double_send_duplicates() {
        let t = TestApp::new();
        let good = MockProvider::new().await;
        let bad = MockProvider::new().await;
        bad.respond(404, "SECRET");
        let mut cfg = (*t.app.cfg()).clone();
        cfg.alerts.webhook_url = format!("{}/first", good.server.url());
        cfg.alerts.webhook_urls = vec![
            cfg.alerts.webhook_url.clone(),
            format!("{}/bad", bad.server.url()),
        ];
        cfg.alerts.webhook_format = webhook::Format::Discord;
        t.app.cfg_swap(cfg);
        let response = crate::api::webhook_test(AppCtx(t.app.clone()), t.request("{}")).await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let bytes = axum::body::to_bytes(response.into_body(), 8192)
            .await
            .unwrap();
        let report: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(report["ok"], false);
        assert_eq!(report["sent"], 1);
        assert_eq!(report["failed"], 1);
        assert_eq!(report["results"][0]["status"], 200);
        assert_eq!(report["results"][1]["status"], 404);
        assert_eq!(good.calls.lock().unwrap().len(), 1);
        assert_eq!(bad.calls.lock().unwrap().len(), 1);
        assert!(!String::from_utf8(bytes.to_vec())
            .unwrap()
            .contains("SECRET"));
    }

    #[tokio::test]
    async fn automatic_alert_reaches_all_list_targets_without_a_legacy_url() {
        let t = TestApp::new();
        let good = MockProvider::new().await;
        let bad = MockProvider::new().await;
        bad.respond(403, "{}");
        let mut cfg = (*t.app.cfg()).clone();
        cfg.alerts.webhook_urls = vec![bad.server.url(), good.server.url()];
        t.app.cfg_swap(cfg);
        alert(&t.app, "budget_hard", "Limit erreicht");
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while t.app.db.last_event_of_kind("webhook_sent").is_none()
                || t.app.db.last_event_of_kind("webhook_failed").is_none()
            {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(good.calls.lock().unwrap().len(), 1);
        assert_eq!(bad.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn retries_respect_retry_after_and_stop_when_the_saved_target_is_removed() {
        use std::sync::{Arc, Mutex};
        for remove in [false, true] {
            let t = TestApp::new();
            let calls = Arc::new(Mutex::new(Vec::new()));
            let recorded = calls.clone();
            let server =
                crate::test_support::LocalServer::new(axum::Router::new().fallback(move || {
                    let calls = recorded.clone();
                    async move {
                        let mut calls = calls.lock().unwrap();
                        calls.push(std::time::Instant::now());
                        axum::response::Response::builder()
                            .status(if calls.len() == 1 { 429 } else { 200 })
                            .header("retry-after", "1")
                            .body(axum::body::Body::empty())
                            .unwrap()
                    }
                }))
                .await;
            let mut cfg = (*t.app.cfg()).clone();
            cfg.alerts.webhook_url = server.url();
            t.app.cfg_swap(cfg);
            alert(&t.app, "budget_hard", "test");
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                while t.app.db.last_event_of_kind("webhook_retry").is_none() {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            if remove {
                let mut cfg = (*t.app.cfg()).clone();
                cfg.alerts.webhook_url.clear();
                t.app.cfg_swap(cfg);
                tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
                assert_eq!(calls.lock().unwrap().len(), 1);
                assert!(t.app.db.last_event_of_kind("webhook_sent").is_none());
            } else {
                tokio::time::timeout(std::time::Duration::from_secs(3), async {
                    while t.app.db.last_event_of_kind("webhook_sent").is_none() {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                })
                .await
                .unwrap();
                let calls = calls.lock().unwrap();
                assert_eq!(calls.len(), 2);
                assert!(calls[1].duration_since(calls[0]) >= std::time::Duration::from_millis(900));
            }
        }
    }

    #[test]
    fn interval_reservations_are_durable_and_handle_backwards_clocks() {
        let t = TestApp::new();
        assert!(t.app.db.claim_interval("test", 100, 10).unwrap());
        assert!(!t.app.db.claim_interval("test", 101, 10).unwrap());
        assert!(t.app.db.claim_interval("test", 110, 10).unwrap());
        assert!(t.app.db.claim_interval("test", 90, 10).unwrap());
        assert!(!t.app.db.claim_interval("test", 95, 10).unwrap());
    }
}
