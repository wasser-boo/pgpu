//! Webhook transport shared by automatic alerts and the authenticated test button.
//! No database/lifecycle effects. Never include a credential-bearing URL or a
//! remote response body in errors/logs, and never follow webhook redirects.
use crate::config::AlertsCfg;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    #[default]
    Auto,
    Json,
    Discord,
    Slack,
    Ntfy,
}

impl Format {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Json => "json",
            Self::Discord => "discord",
            Self::Slack => "slack",
            Self::Ntfy => "ntfy",
        }
    }
}

#[derive(Clone)]
pub struct Target {
    pub number: usize,
    pub url: String,
    pub format: Format,
}
impl Target {
    pub fn host(&self) -> String {
        reqwest::Url::parse(&self.url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
            .unwrap_or_default()
    }
}

#[derive(Debug, Serialize)]
pub struct Delivery {
    pub target: usize,
    pub format: Format,
    pub status: u16,
    pub host: String,
}
impl std::fmt::Display for Delivery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Webhook {} angenommen: {} · {} · HTTP {}",
            self.target,
            self.host,
            self.format.label(),
            self.status
        )
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct Failure {
    pub message: String,
    pub status: Option<u16>,
    pub retryable: bool,
    pub retry_after_s: Option<u64>,
}
impl Failure {
    fn config(message: &str) -> Self {
        Self {
            message: message.into(),
            status: None,
            retryable: false,
            retry_after_s: None,
        }
    }
    fn transport(error: reqwest::Error) -> Self {
        Self {
            message: format!("Webhook-Netzwerkfehler: {}", error.without_url()),
            status: None,
            retryable: true,
            retry_after_s: None,
        }
    }
}

fn url(target: &Target) -> Result<reqwest::Url, Failure> {
    let url = reqwest::Url::parse(target.url.trim()).map_err(|_| {
        Failure::config("alerts.webhook_url muss eine vollständige HTTP(S)-Webhook-URL sein")
    })?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(Failure::config(
            "alerts.webhook_url: nur HTTP(S), ohne URL-Benutzername/Passwort oder Fragment",
        ));
    }
    Ok(url)
}

/// Legacy single URL + new list, in config order. Normalize/deduplicate before
/// sending, including Discord URLs that differ only in the ignored wait flag.
pub fn targets(cfg: &AlertsCfg) -> Result<Vec<Target>, Failure> {
    let mut targets = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for raw in std::iter::once(&cfg.webhook_url)
        .filter(|u| !u.trim().is_empty())
        .chain(&cfg.webhook_urls)
    {
        let mut target = Target {
            number: targets.len() + 1,
            url: raw.trim().into(),
            format: cfg.webhook_format,
        };
        let mut parsed = url(&target)?;
        target.format = resolved_format(&target);
        if target.format == Format::Discord {
            discord_wait(&mut parsed);
        }
        target.url = parsed.to_string();
        if seen.insert(target.url.clone()) {
            targets.push(target);
        }
    }
    if targets.len() > 16 {
        return Err(Failure::config(
            "Maximal 16 verschiedene Webhook-Ziele sind erlaubt",
        ));
    }
    Ok(targets)
}

pub fn validate(cfg: &AlertsCfg) -> Result<(), Failure> {
    if cfg.spend_summary_interval_s!=0 && !(3600..=604800).contains(&cfg.spend_summary_interval_s) {
        return Err(Failure::config("alerts.spend_summary_interval_s: 0 (aus) oder 3600..604800 Sekunden"));
    }
    targets(cfg).map(|_| ())
}

pub fn resolved_format(target: &Target) -> Format {
    if target.format != Format::Auto {
        return target.format;
    }
    let Ok(url) = url(target) else {
        return Format::Json;
    };
    let host = url.host_str().unwrap_or("");
    if matches!(host, "discord.com" | "discordapp.com")
        || host.ends_with(".discord.com")
        || host.ends_with(".discordapp.com")
    {
        Format::Discord
    } else if matches!(host, "hooks.slack.com" | "hooks.slack-gov.com") {
        Format::Slack
    } else if host == "ntfy.sh" {
        Format::Ntfy
    } else {
        Format::Json
    }
}

/// Discord counts UTF-16 units. Preserve valid Unicode and keep room for an ellipsis.
fn content(kind: &str, message: &str) -> String {
    let text = format!("[PGPU] {kind}\n{message}");
    if text.encode_utf16().count() <= 2000 {
        return text;
    }
    let mut units = 0;
    let mut result: String = text
        .chars()
        .take_while(|c| {
            units += c.len_utf16();
            units <= 1999
        })
        .collect();
    result.push('…');
    result
}

fn discord_wait(url: &mut reqwest::Url) {
    // Discord explicitly warns that wait=false can hide unsaved messages.
    let query: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(key, _)| key != "wait")
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    url.set_query(None);
    url.query_pairs_mut()
        .extend_pairs(query)
        .append_pair("wait", "true");
}

pub async fn send(target: &Target, kind: &str, message: &str) -> Result<Delivery, Failure> {
    let mut url = url(target)?;
    let format = resolved_format(target);
    if format == Format::Discord {
        discord_wait(&mut url);
    }
    let host = url.host_str().unwrap_or("").to_string();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(Failure::transport)?;
    let request = client.post(url);
    let request = match format {
        Format::Discord => request.json(&serde_json::json!({
            "content": content(kind, message), "allowed_mentions": {"parse": []}
        })),
        Format::Slack => request.json(&serde_json::json!({"text": content(kind, message)})),
        Format::Ntfy => request
            .header("content-type", "text/plain; charset=utf-8")
            .body(content(kind, message)),
        Format::Auto | Format::Json => {
            request.json(&serde_json::json!({"kind":kind, "message":message}))
        }
    };
    let response = request.send().await.map_err(Failure::transport)?;
    let status = response.status();
    if !status.is_success() {
        // Do not echo arbitrary remote bodies: these can reflect secret URLs/tokens.
        let detail = match status.as_u16() {
            400 => "Datenformat/Parameter prüfen",
            401 | 403 => "Webhook-Token oder Berechtigungen prüfen",
            404 => "Webhook nicht gefunden (URL prüfen; eventuell gelöscht)",
            429 => "Rate-Limit; später erneut versuchen",
            300..=399 => "Weiterleitungen werden zum Schutz des Webhook-Tokens nicht verfolgt",
            _ => "Webhook-Dienst hat die Nachricht abgelehnt",
        };
        let retry_after_s = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v >= 0.0)
            .map(|v| v.ceil().min(u64::MAX as f64) as u64);
        return Err(Failure {
            message: format!("Webhook HTTP {}: {detail}", status.as_u16()),
            status: Some(status.as_u16()),
            retryable: status.is_server_error() || status.as_u16() == 429,
            retry_after_s,
        });
    }
    Ok(Delivery {
        target: target.number,
        format,
        status: status.as_u16(),
        host,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{LocalServer, MockProvider};

    fn cfg(url: String, format: Format) -> Target {
        Target {
            number: 1,
            url,
            format,
        }
    }

    #[test]
    fn multiple_urls_preserve_legacy_deduplicate_and_validate_all_targets() {
        let cfg = AlertsCfg {
            webhook_url: " https://discord.com/api/webhooks/id/SECRET?wait=false ".into(),
            webhook_urls: vec![
                "https://discord.com/api/webhooks/id/SECRET?wait=true".into(),
                "https://ntfy.sh/private-topic".into(),
            ],
            webhook_format: Format::Auto,
            ..Default::default()
        };
        let selected = targets(&cfg).unwrap();
        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].format, Format::Discord);
        assert_eq!(selected[1].format, Format::Ntfy);
        assert_eq!(selected[1].number, 2);
        let mut invalid = cfg.clone();
        invalid.webhook_urls.push(String::new());
        assert!(validate(&invalid).is_err());
        let excessive = AlertsCfg {
            webhook_urls: (0..17)
                .map(|i| format!("https://example.org/{i}"))
                .collect(),
            ..Default::default()
        };
        assert!(validate(&excessive).is_err());
    }

    #[test]
    fn malformed_config_errors_do_not_echo_webhook_secrets() {
        for raw in [
            "[alerts]\nwebhook_url = \"https://discord.com/api/webhooks/SECRET",
            "[alerts]\nwebhook_format = \"SECRET\"\n",
        ] {
            let error = crate::config::Config::load_str(raw).unwrap_err();
            assert!(!format!("{error:#}").contains("SECRET"));
            assert!(error.to_string().contains("line"));
        }
    }

    #[test]
    fn automatic_formats_and_safe_unicode() {
        for (host, expected) in [
            ("discord.com", Format::Discord),
            ("canary.discord.com", Format::Discord),
            ("discordapp.com", Format::Discord),
            ("hooks.slack.com", Format::Slack),
            ("ntfy.sh", Format::Ntfy),
            ("discord.com.example.org", Format::Json),
        ] {
            assert_eq!(
                resolved_format(&cfg(format!("https://{host}/example"), Format::Auto)),
                expected
            );
        }
        let long = content("test", &"🦀".repeat(2000));
        assert!(long.encode_utf16().count() <= 2000);
        assert!(long.ends_with('…'));
        for bad in [
            "file:///etc/passwd",
            "ftp://example.org/secret",
            "https://user:secret@example.org/",
            "https://example.org/#secret",
        ] {
            let error = url(&cfg(bad.into(), Format::Auto)).unwrap_err();
            assert!(!error.to_string().contains("secret"));
        }
    }

    #[tokio::test]
    async fn discord_uses_content_disables_mentions_and_waits_for_confirmation() {
        let p = MockProvider::new().await;
        p.respond(200, r#"{"id":"example-message-id"}"#);
        let c = cfg(
            format!(
                "{}/api/webhooks/id/SECRET?thread_id=123&wait=false",
                p.server.url()
            ),
            Format::Discord,
        );
        let sent = send(&c, "webhook_test", "✅ Verbindung funktioniert. @everyone")
            .await
            .unwrap();
        assert_eq!(sent.status, 200);
        let calls = p.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "POST");
        assert_eq!(
            calls[0].1,
            "/api/webhooks/id/SECRET?thread_id=123&wait=true"
        );
        assert!(calls[0].2["content"]
            .as_str()
            .unwrap()
            .contains("✅ Verbindung funktioniert."));
        assert_eq!(
            calls[0].2["allowed_mentions"]["parse"],
            serde_json::json!([])
        );
        assert!(calls[0].2.get("kind").is_none());
        assert!(!sent.to_string().contains("SECRET"));
    }

    #[tokio::test]
    async fn generic_json_and_slack_remain_supported() {
        let p = MockProvider::new().await;
        for (format, key) in [(Format::Json, "message"), (Format::Slack, "text")] {
            send(
                &cfg(p.server.url(), format),
                "budget_hard",
                "Limit erreicht",
            )
            .await
            .unwrap();
            assert!(p.calls.lock().unwrap().last().unwrap().2[key]
                .as_str()
                .unwrap()
                .contains("Limit erreicht"));
        }
    }

    #[tokio::test]
    async fn ntfy_uses_plain_text() {
        use axum::{extract::Request, response::IntoResponse};
        let server = LocalServer::new(axum::Router::new().fallback(|req: Request| async {
            assert_eq!(req.headers()["content-type"], "text/plain; charset=utf-8");
            let body = axum::body::to_bytes(req.into_body(), 8192).await.unwrap();
            assert_eq!(&body[..], b"[PGPU] test\nhello");
            axum::http::StatusCode::OK.into_response()
        }))
        .await;
        send(&cfg(server.url(), Format::Ntfy), "test", "hello")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn upstream_failures_are_not_silent_and_do_not_leak_urls() {
        let p = MockProvider::new().await;
        for status in [400, 401, 403, 404, 429, 500] {
            p.respond(status, "sensitive remote error: SECRET");
            let err = send(
                &cfg(format!("{}/SECRET", p.server.url()), Format::Discord),
                "test",
                "test",
            )
            .await
            .unwrap_err();
            assert_eq!(err.status, Some(status));
            assert_eq!(err.retryable, status == 429 || status >= 500);
            assert!(!err.to_string().contains("SECRET"));
        }
    }

    #[tokio::test]
    async fn redirects_are_not_followed_and_network_errors_are_redacted() {
        let target = MockProvider::new().await;
        let location = format!("{}/should-not-arrive", target.server.url());
        let server = LocalServer::new(axum::Router::new().fallback(move || {
            let location = location.clone();
            async move { axum::response::Redirect::temporary(&location) }
        }))
        .await;
        let c = cfg(format!("{}/SECRET", server.url()), Format::Discord);
        assert_eq!(
            send(&c, "test", "test").await.unwrap_err().status,
            Some(307)
        );
        assert!(target.calls.lock().unwrap().is_empty());
        drop(server);
        let error = send(&c, "test", "test").await.unwrap_err();
        assert!(error.retryable);
        assert!(!error.to_string().contains("SECRET"));
    }
}
