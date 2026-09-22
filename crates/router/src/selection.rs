//! Shared country/radius/GPU selection for config validation, provider search,
//! local admission and UI previews. Geographic selection is a COUNTRY approximation:
//! Natural Earth reference points, never a promise about individual host distance.
use crate::config::SlotCfg;
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::{collections::BTreeSet, sync::OnceLock};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Country {
    pub code: String,
    pub name: String,
    /// Longitude, latitude of Natural Earth's cartographic label point.
    pub center: [f64; 2],
}

pub fn countries() -> &'static [Country] {
    static COUNTRIES: OnceLock<Vec<Country>> = OnceLock::new();
    COUNTRIES.get_or_init(|| {
        serde_json::from_str(include_str!("../static/countries.json"))
            .expect("bundled country data")
    })
}

fn country(code: &str) -> Result<&'static Country> {
    let code = code.trim().to_ascii_uppercase();
    countries()
        .iter()
        .find(|c| c.code == code)
        .context("country code not available in bundled map (use e.g. DE)")
}
fn codes(values: &[String]) -> Result<BTreeSet<String>> {
    values
        .iter()
        .map(|v| country(v).map(|c| c.code.clone()))
        .collect()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Location {
    /// Empty allowlist means all countries before applying radius/exclusions.
    pub countries: Vec<String>,
    pub excluded_countries: Vec<String>,
    pub origin_country: String,
    pub radius_km: Option<f64>,
    /// Optional exact origin instead of the selected country's reference point.
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
}
impl Default for Location {
    fn default() -> Self {
        Self {
            countries: vec![],
            excluded_countries: vec![],
            origin_country: "DE".into(),
            radius_km: None,
            latitude: None,
            longitude: None,
        }
    }
}
impl Location {
    pub fn origin(&self) -> Result<[f64; 2]> {
        let default = country(&self.origin_country)?.center;
        match (self.longitude, self.latitude) {
            (None, None) => Ok(default),
            (Some(lon), Some(lat))
                if lon.is_finite()
                    && lat.is_finite()
                    && (-180.0..=180.0).contains(&lon)
                    && (-90.0..=90.0).contains(&lat) =>
            {
                Ok([lon, lat])
            }
            _ => {
                anyhow::bail!("location latitude/longitude must both be set and within ±90°/±180°")
            }
        }
    }
    pub fn validate(&self) -> Result<()> {
        self.origin()?;
        codes(&self.countries)?;
        codes(&self.excluded_countries)?;
        ensure!(
            self.radius_km
                .is_none_or(|r| r.is_finite() && (0.0..=20_020.0).contains(&r)),
            "location.radius_km must be 0..20020 km"
        );
        Ok(())
    }
    pub fn active(&self) -> bool {
        !self.countries.is_empty()
            || !self.excluded_countries.is_empty()
            || self.radius_km.is_some()
    }
}

pub fn distance_km(a: [f64; 2], b: [f64; 2]) -> f64 {
    let (lat1, lat2) = (a[1].to_radians(), b[1].to_radians());
    let dlat = (lat2 - lat1) / 2.0;
    let dlon = (b[0] - a[0]).to_radians() / 2.0;
    12_742.017_6
        * (dlat.sin().powi(2) + lat1.cos() * lat2.cos() * dlon.sin().powi(2))
            .clamp(0.0, 1.0)
            .sqrt()
            .asin()
}

fn matches_text(
    filter: &Value,
    candidate: &str,
    normalize: impl Fn(&str) -> String,
) -> Result<bool> {
    let obj = filter
        .as_object()
        .context("invalid text search predicate")?;
    let candidate = normalize(candidate);
    let mut allowed = true;
    for (op, value) in obj {
        allowed &= match op.as_str() {
            "eq" | "neq" => {
                let equal = normalize(
                    value
                        .as_str()
                        .context("country/GPU comparison requires text")?,
                ) == candidate;
                if op == "eq" {
                    equal
                } else {
                    !equal
                }
            }
            "in" | "notin" => {
                let values = value.as_array().context("country/GPU list expected")?;
                let mut contains = false;
                for value in values {
                    contains |=
                        normalize(value.as_str().context("country/GPU list requires text")?)
                            == candidate;
                }
                if op == "in" {
                    contains
                } else {
                    !contains
                }
            }
            _ => anyhow::bail!("country/GPU filters support only =, !=, in and notin"),
        };
    }
    Ok(allowed)
}

pub fn selected_countries(
    location: &Location,
    filter: Option<&Value>,
) -> Result<Option<Vec<String>>> {
    location.validate()?;
    if !location.active() && filter.is_none() {
        return Ok(None);
    }
    // Validate every explicit code, including exclusions, before comparing any offer.
    if let Some(filter) = filter {
        for value in filter
            .as_object()
            .context("invalid geolocation filter")?
            .values()
        {
            let values = value
                .as_array()
                .cloned()
                .unwrap_or_else(|| vec![value.clone()]);
            for value in values {
                country(
                    value
                        .as_str()
                        .context("geolocation requires ISO country codes")?,
                )?;
            }
        }
    }
    let allow = codes(&location.countries)?;
    let exclude = codes(&location.excluded_countries)?;
    let origin = location.origin()?;
    let mut selected = Vec::new();
    for c in countries() {
        if (!allow.is_empty() && !allow.contains(&c.code)) || exclude.contains(&c.code) {
            continue;
        }
        if location
            .radius_km
            .is_some_and(|r| distance_km(origin, c.center) > r + 1e-8)
        {
            continue;
        }
        if let Some(filter) = filter {
            if !matches_text(filter, &c.code, |s| s.trim().to_ascii_uppercase())? {
                continue;
            }
        }
        selected.push(c.code.clone());
    }
    ensure!(
        !selected.is_empty(),
        "country/radius/exclusion filters leave no allowed country"
    );
    Ok(Some(selected))
}

/// No serialization back to query text: RAM is already converted to API MB.
pub fn query(slot: &SlotCfg, on_demand: bool) -> Result<Map<String, Value>> {
    let raw = if on_demand {
        slot.search_query_on_demand
            .as_deref()
            .unwrap_or(&slot.search_query)
    } else {
        &slot.search_query
    };
    let mut parsed = praxis_vast::query::parse_query(raw)?;
    if let Some(countries) = selected_countries(&slot.location, parsed.get("geolocation"))? {
        parsed.insert("geolocation".into(), json!({"in":countries}));
    }
    if let Some(filter) = parsed.get("gpu_name") {
        // Validate raw model predicates even without a structured allowlist.
        matches_text(filter, "", praxis_policy::eligibility::gpu_key)?;
    }
    if !slot.requirements.gpu_names.is_empty() {
        let mut names = BTreeSet::new();
        for name in &slot.requirements.gpu_names {
            if let Some(filter) = parsed.get("gpu_name") {
                if !matches_text(filter, name, praxis_policy::eligibility::gpu_key)? {
                    continue;
                }
            }
            let name = name.trim().replace('_', " ");
            let name = if name
                .get(..7)
                .is_some_and(|s| s.eq_ignore_ascii_case("nvidia "))
            {
                &name[7..]
            } else {
                &name
            };
            names.insert(name.to_string());
        }
        ensure!(
            !names.is_empty(),
            "GPU allowlist contradicts search_query GPU filter"
        );
        parsed.insert("gpu_name".into(), json!({"in":names}));
    }
    Ok(parsed)
}

/// Provider geolocation is either an ISO code or a display string ending with one.
fn offer_country(location: &str) -> Option<&'static Country> {
    country(location)
        .ok()
        .or_else(|| country(location.rsplit(',').next()?.trim()).ok())
        .or_else(|| {
            countries().iter().find(|c| {
                c.name.eq_ignore_ascii_case(location.trim())
                    || c.name
                        .eq_ignore_ascii_case(location.rsplit(',').next().unwrap_or("").trim())
            })
        })
}

pub fn rejection(
    slot: &SlotCfg,
    location: Option<&str>,
    gpu_name: &str,
    on_demand: bool,
) -> Result<Option<String>> {
    let query = query(slot, on_demand)?;
    if let Some(filter) = query.get("gpu_name") {
        if gpu_name.trim().is_empty()
            || !matches_text(filter, gpu_name, praxis_policy::eligibility::gpu_key)?
        {
            return Ok(Some(
                "GPU-Modell durch effektiven Suchfilter nicht erlaubt".into(),
            ));
        }
    }
    let Some(filter) = query.get("geolocation") else {
        return Ok(None);
    };
    let Some(country) = location.and_then(offer_country) else {
        return Ok(Some(
            "Standort fehlt/ist unbekannt; Länderfilter kann nicht bestätigt werden".into(),
        ));
    };
    if matches_text(filter, &country.code, |s| s.to_ascii_uppercase())? {
        Ok(None)
    } else {
        Ok(Some(format!(
            "Land {} durch Länder-/Radius-/Ausschlussfilter nicht erlaubt",
            country.code
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestApp;

    fn rejection(
        slot: &SlotCfg,
        location: Option<&str>,
        on_demand: bool,
    ) -> Result<Option<String>> {
        super::rejection(slot, location, "RTX A4000", on_demand)
    }

    #[tokio::test]
    async fn country_and_model_admission_reject_before_any_provider_mutation() {
        let t = TestApp::new();
        let mut cfg = (*t.app.cfg()).clone();
        cfg.slots[0].location.countries = vec!["DE".into()];
        cfg.slots[0].search_query = "gpu_name in [\"RTX A4000\",\"RTX 4060 Ti\"]".into();
        t.app.cfg_swap(cfg);
        let provider = crate::test_support::MockProvider::new().await;
        provider.attach(&t.app);
        for (location, model) in [
            (Some("US"), "RTX A4000"),
            (None, "RTX A4000"),
            (Some("DE"), "RTX 3090"),
        ] {
            let offer = praxis_policy::OfferSnapshot {
                geolocation: location.map(str::to_string),
                gpu_name: model.into(),
                dph_total: 0.1,
                ..Default::default()
            };
            let error = crate::reconciler::create_instance(
                &t.app,
                1,
                &offer,
                praxis_common::Mode::Interruptible,
                0.1,
                60,
                Default::default(),
                false,
                "test",
            )
            .await
            .unwrap_err();
            assert!(error.to_string().contains("offer rejected"));
        }
        assert!(provider.calls.lock().unwrap().is_empty());
        assert!(t.app.db.try_instances(true).unwrap().is_empty());
        assert!(super::rejection(
            &t.app.cfg().slots[0],
            Some("Berlin, Germany"),
            "NVIDIA RTX_A4000",
            false
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn reference_data_and_haversine_are_consistent() {
        assert!(countries().len() > 230);
        assert_eq!(
            countries().len(),
            countries()
                .iter()
                .map(|c| &c.code)
                .collect::<BTreeSet<_>>()
                .len()
        );
        assert_eq!(country("de").unwrap().name, "Germany");
        assert!(distance_km([179.9, 0.0], [-179.9, 0.0]) < 23.0);
        assert!(distance_km([0.0, 90.0], [120.0, 90.0]) < 0.001);
        assert!((distance_km([0.0, 0.0], [180.0, 0.0]) - 20_015.11).abs() < 0.1);
        let loc = Location {
            radius_km: Some(0.0),
            ..Default::default()
        };
        assert_eq!(selected_countries(&loc, None).unwrap().unwrap(), ["DE"]);
    }

    #[test]
    fn exclusions_win_over_allowlist_radius_and_query_in_both_modes() {
        let t = TestApp::new();
        let mut slot = t.app.cfg().slots[0].clone();
        slot.location.countries = vec!["DE".into(), "AT".into(), "FR".into(), "CH".into()];
        slot.location.excluded_countries = vec!["CH".into()];
        slot.location.radius_km = Some(1000.0);
        slot.search_query = "gpu_ram>=16 geolocation!=DE geolocation notin [FR]".into();
        for mode in [false, true] {
            let q = query(&slot, mode).unwrap();
            assert_eq!(q["geolocation"], json!({"in":["AT"]}));
            assert_eq!(q["gpu_ram"]["gte"].as_f64(), Some(16_000.0));
            assert!(rejection(&slot, Some("Berlin, DE"), mode)
                .unwrap()
                .is_some());
            assert!(rejection(&slot, Some("AT"), mode).unwrap().is_none());
            assert!(rejection(&slot, None, mode).unwrap().is_some());
        }
    }

    #[test]
    fn on_demand_query_and_multiple_gpu_models_are_independent() {
        let t = TestApp::new();
        let mut slot = t.app.cfg().slots[0].clone();
        slot.search_query = "geolocation!=DE".into();
        slot.search_query_on_demand = Some("geolocation=DE".into());
        slot.requirements.gpu_names = vec!["RTX A4000".into(), "RTX 4060 Ti".into()];
        assert!(rejection(&slot, Some("DE"), false).unwrap().is_some());
        assert!(rejection(&slot, Some("DE"), true).unwrap().is_none());
        assert_eq!(
            query(&slot, true).unwrap()["gpu_name"]["in"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn invalid_or_empty_selection_fails_closed() {
        for loc in [
            Location {
                radius_km: Some(f64::NAN),
                ..Default::default()
            },
            Location {
                radius_km: Some(-1.0),
                ..Default::default()
            },
            Location {
                latitude: Some(50.0),
                ..Default::default()
            },
            Location {
                origin_country: "unknown".into(),
                ..Default::default()
            },
            Location {
                countries: vec!["DE".into()],
                excluded_countries: vec!["DE".into()],
                ..Default::default()
            },
        ] {
            assert!(selected_countries(&loc, None).is_err());
        }
        assert!(selected_countries(&Location::default(), Some(&json!({"gte":"DE"}))).is_err());
        assert!(selected_countries(&Location::default(), Some(&json!({"neq":"ZZ"}))).is_err());
    }
}
