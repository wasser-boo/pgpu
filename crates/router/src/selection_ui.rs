//! Authenticated selection preview/save adapters; all rules are shared with admission.
use crate::{
    selection::{self, Location},
    state::AppCtx,
};
use anyhow::{Context, Result};
use axum::{
    extract::{Form, Request},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
    Json,
};
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub slot_id: i64,
    pub location: Location,
    #[serde(default)]
    pub gpu_names: Vec<String>,
}
impl Input {
    fn slot(&self, cfg: &crate::config::Config) -> Result<crate::config::SlotCfg> {
        let mut slot = cfg.slot(self.slot_id).context("unknown slot")?.clone();
        slot.location = self.location.clone();
        slot.requirements.gpu_names = self.gpu_names.clone();
        slot.requirements.validate().map_err(anyhow::Error::msg)?;
        selection::query(&slot, false)?;
        selection::query(&slot, true)?;
        Ok(slot)
    }
    fn form(form: &HashMap<String, String>) -> Result<Self> {
        let get = |key: &str| form.get(key).map(String::as_str).unwrap_or("");
        let list = |key| {
            get(key)
                .split(|c: char| c == ',' || c.is_whitespace())
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        };
        let number = |key| -> Result<Option<f64>> {
            if get(key).trim().is_empty() {
                Ok(None)
            } else {
                Ok(Some(
                    get(key)
                        .parse()
                        .context("invalid numeric selection field")?,
                ))
            }
        };
        Ok(Self {
            slot_id: get("slot_id").parse().context("invalid slot ID")?,
            location: Location {
                countries: list("countries"),
                excluded_countries: list("excluded_countries"),
                origin_country: get("origin_country").to_string(),
                radius_km: number("radius_km")?,
                latitude: number("latitude")?,
                longitude: number("longitude")?,
            },
            gpu_names: get("gpu_names")
                .lines()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect(),
        })
    }
}

pub async fn preview(app: AppCtx, req: Request) -> Response {
    if !crate::dashboard::session_ok(&app.0, req.headers()) {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    let result = async {
        let body = axum::body::to_bytes(req.into_body(), 64 * 1024).await?;
        let input: Input = serde_json::from_slice(&body)?;
        let slot = input.slot(&app.cfg())?;
        let bid = selection::query(&slot, false)?;
        let on_demand = selection::query(&slot, true)?;
        Ok::<_, anyhow::Error>(serde_json::json!({
            "bid_countries":bid.get("geolocation").and_then(|g|g.get("in")),
            "on_demand_countries":on_demand.get("geolocation").and_then(|g|g.get("in")),
            "origin":slot.location.origin()?, "radius_km":slot.location.radius_km,
            "bid_query":bid,"on_demand_query":on_demand,
        }))
    }
    .await;
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":error.to_string()})),
        )
            .into_response(),
    }
}

/// Surgical TOML editing keeps unrelated limits, secrets, runtime env and comments.
fn patch(raw: &str, input: &Input) -> Result<String> {
    let cfg = crate::config::Config::load_str(raw)?;
    input.slot(&cfg)?;
    let mut doc: toml_edit::DocumentMut = raw.parse()?;
    let slots = doc
        .get_mut("slots")
        .and_then(toml_edit::Item::as_array_of_tables_mut)
        .context("slots must be tables")?;
    let slot = slots
        .iter_mut()
        .find(|s| s.get("id").and_then(toml_edit::Item::as_integer) == Some(input.slot_id))
        .context("unknown slot")?;
    let array = |values: &[String]| {
        let mut a = toml_edit::Array::new();
        for value in values {
            a.push(value.as_str());
        }
        toml_edit::value(a)
    };
    let mut location = toml_edit::Table::new();
    location["countries"] = array(&input.location.countries);
    location["excluded_countries"] = array(&input.location.excluded_countries);
    location["origin_country"] =
        toml_edit::value(input.location.origin_country.trim().to_ascii_uppercase());
    for (key, value) in [
        ("radius_km", input.location.radius_km),
        ("latitude", input.location.latitude),
        ("longitude", input.location.longitude),
    ] {
        if let Some(value) = value {
            location[key] = toml_edit::value(value);
        }
    }
    slot["location"] = toml_edit::Item::Table(location);
    if !slot.contains_key("requirements") {
        slot["requirements"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    slot.get_mut("requirements")
        .and_then(toml_edit::Item::as_table_like_mut)
        .context("invalid requirements table")?
        .insert("gpu_names", array(&input.gpu_names));
    let output = doc.to_string();
    crate::config::Config::load_str(&output)?;
    Ok(output)
}

pub async fn save(
    app: AppCtx,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    if !crate::dashboard::session_ok(&app.0, &headers) {
        return Redirect::to("/login").into_response();
    }
    let result = async {
        let input = Input::form(&form)?;
        let raw = patch(&app.cfg_raw(), &input)?;
        crate::api::apply_config(&app.0, &raw).await
    }
    .await;
    let message=match result { Ok(_)=>"Länder-/GPU-Auswahl gespeichert. Die automatische Policy verwendet sie beim nächsten Abgleich; kein direkter Instanzumbau.".to_string(), Err(error)=>format!("Auswahl nicht gespeichert: {error}") };
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("msg", &message)
        .finish();
    Redirect::to(&format!("/settings?{query}")).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestApp;

    #[test]
    fn patch_preserves_unrelated_settings_comments_and_budgets() {
        let raw = include_str!("../../../config.example.toml");
        let before: toml::Value = toml::from_str(raw).unwrap();
        let input = Input {
            slot_id: 1,
            location: Location {
                countries: vec!["DE".into(), "AT".into()],
                ..Default::default()
            },
            gpu_names: vec!["RTX A4000".into(), "RTX 4060 Ti".into()],
        };
        let output = patch(raw, &input).unwrap();
        let after: toml::Value = toml::from_str(&output).unwrap();
        for key in ["router", "budget", "limits", "netbird", "alerts", "vast"] {
            assert_eq!(before.get(key), after.get(key));
        }
        assert_eq!(before["slots"][1], after["slots"][1]);
        assert_eq!(before["slots"][0]["env"], after["slots"][0]["env"]);
        assert!(output.contains("# Discord:"));
        assert_eq!(
            after["slots"][0]["location"]["origin_country"].as_str(),
            Some("DE")
        );
        assert_eq!(
            after["slots"][0]["requirements"]["gpu_names"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn preview_is_authenticated_and_uses_same_exclusions_as_admission() {
        let t = TestApp::new();
        let unauthorized = preview(
            AppCtx(t.app.clone()),
            Request::new(axum::body::Body::empty()),
        )
        .await;
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
        let body = r#"{"slot_id":1,"location":{"countries":["DE","AT"],"excluded_countries":["DE"]},"gpu_names":[]}"#;
        let response = preview(AppCtx(t.app.clone()), t.request(body)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 8192)
            .await
            .unwrap();
        let result: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(result["bid_countries"], serde_json::json!(["AT"]));
        assert!(t.app.db.instances(true).is_empty());
    }
}
