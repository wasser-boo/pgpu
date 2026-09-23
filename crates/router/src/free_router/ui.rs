//! Authenticated settings adapter. Keys are write-only here; the existing
//! administrator TOML editor remains the explicit full-secret interface.
use super::config::Config;
use crate::state::AppCtx;
use axum::{
    extract::Request,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};

fn revision(raw: &str) -> String {
    hex::encode(Sha256::digest(raw))
}
fn json(status: StatusCode, value: serde_json::Value) -> Response {
    let mut response = (status, Json(value)).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}

pub async fn get(app: AppCtx, req: Request) -> Response {
    if !crate::dashboard::session_ok(&app.0, req.headers()) {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    let (cfg, current_revision) = {
        let _management = app.management.lock().await;
        (app.cfg(), revision(&app.cfg_raw()))
    };
    let free = &cfg.free_router;
    let now = chrono::Utc::now().timestamp_millis();
    let status = super::status::snapshot(&app.db, free, now);
    let mut config = free.clone();
    for p in &mut config.providers {
        p.api_key.clear();
    }
    json(
        StatusCode::OK,
        serde_json::json!({"config":config,"statuses":status.providers,"active":status.active,
        "available":status.available,"retry_after_s":status.retry_after_s,"revision":current_revision,
        "catalog":serde_json::from_str::<serde_json::Value>(include_str!("catalog.json")).expect("bundled provider catalog")}),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    config: Config,
    revision: String,
    #[serde(default)]
    clear_keys: Vec<String>,
}
fn patch(raw: &str, mut input: Input) -> anyhow::Result<String> {
    anyhow::ensure!(
        revision(raw) == input.revision,
        "Configuration changed; reload settings before saving"
    );
    let old = crate::config::Config::load_str(raw)?;
    for p in &mut input.config.providers {
        if input.clear_keys.contains(&p.id) {
            p.api_key.clear();
        } else if p.api_key.is_empty() {
            if let Some(old) = old.free_router.providers.iter().find(|v| v.id == p.id) {
                // Never transplant a stored secret onto a different destination.
                anyhow::ensure!(
                    old.api_key.is_empty()
                        || old.base_url.trim_end_matches('/') == p.base_url.trim_end_matches('/'),
                    "Re-enter or clear API key when changing provider URL"
                );
                p.api_key = old.api_key.clone();
            }
        }
    }
    input.config.validate()?;
    let mut doc: toml_edit::DocumentMut = raw
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid existing TOML"))?;
    let block = toml::to_string(&input.config)?;
    let block: toml_edit::DocumentMut = block
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid free-router configuration"))?;
    doc["free_router"] = toml_edit::Item::Table(block.as_table().clone());
    let output = doc.to_string();
    crate::config::Config::load_str(&output)?;
    Ok(output)
}

pub async fn save(app: AppCtx, req: Request) -> Response {
    if !crate::dashboard::session_ok(&app.0, req.headers()) {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    let Ok(body) = axum::body::to_bytes(req.into_body(), 256 * 1024).await else {
        return json(
            StatusCode::PAYLOAD_TOO_LARGE,
            serde_json::json!({"error":"Settings body too large"}),
        );
    };
    let Ok(input) = serde_json::from_slice::<Input>(&body) else {
        // serde errors can echo user-supplied secrets.
        return json(
            StatusCode::BAD_REQUEST,
            serde_json::json!({"error":"Invalid free-router settings schema"}),
        );
    };
    match crate::api::patch_config(&app.0, |raw| patch(raw, input)).await {
        Ok(_) => json(StatusCode::OK, serde_json::json!({"ok":true})),
        Err(error) => json(
            StatusCode::BAD_REQUEST,
            serde_json::json!({"error":error.to_string()}),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn patch_preserves_secrets_order_comments_and_unrelated_settings() {
        let raw = include_str!("../../../../config.example.toml")
            .replace("api_key_env = \"GROQ_API_KEY\"", "api_key = \"PRIVATE\"");
        let mut config = crate::config::Config::load_str(&raw).unwrap().free_router;
        config.providers[0].api_key.clear();
        config.jumper = true;
        let patched = patch(
            &raw,
            Input {
                config: config.clone(),
                revision: revision(&raw),
                clear_keys: vec![],
            },
        )
        .unwrap();
        let cfg = crate::config::Config::load_str(&patched).unwrap();
        assert!(cfg.free_router.jumper);
        assert_eq!(cfg.free_router.providers[0].api_key, "PRIVATE");
        assert!(patched.contains("# Router-Token:"));
        assert_eq!(
            toml::from_str::<toml::Value>(&raw).unwrap()["slots"],
            toml::from_str::<toml::Value>(&patched).unwrap()["slots"]
        );
        config.providers[0].base_url = "https://other.example/v1".into();
        assert!(patch(
            &raw,
            Input {
                config: config.clone(),
                revision: revision(&raw),
                clear_keys: vec![]
            }
        )
        .is_err());
        assert!(patch(
            &raw,
            Input {
                config,
                revision: "stale".into(),
                clear_keys: vec![]
            }
        )
        .is_err());
    }
}
