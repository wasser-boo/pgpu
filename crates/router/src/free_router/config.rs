use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub use_when_all_offline: bool,
    /// OpenAI base path on the existing dashboard listener; hot-reloadable.
    pub endpoint_path: String,
    /// Round-robin starting provider for every request instead of priority fill.
    pub jumper: bool,
    pub safety_buffer_requests: u32,
    /// Ordered preference list. Disabled entries never send requests.
    pub providers: Vec<Provider>,
}
impl Default for Config {
    /// Fail closed: nothing sends requests unless the operator opts in
    /// (docs/free-api-router.md: "Standardmäßig vollständig AUS").
    fn default() -> Self {
        Self {
            use_when_all_offline: false,
            endpoint_path: "/free/v1".into(),
            jumper: false,
            safety_buffer_requests: 5,
            providers: Vec::new(),
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Provider {
    pub id: String,
    pub enabled: bool,
    pub base_url: String,
    pub api_key: String,
    pub api_key_env: String,
    /// Explicit upstream model; never silently use an arbitrary paid model.
    pub model: String,
    pub requests_per_minute: u32,
    pub requests_per_hour: Option<u32>,
    pub requests_per_day: Option<u32>,
    pub requests_per_month: Option<u32>,
    pub tokens_per_minute: Option<u32>,
    pub tokens_per_day: Option<u32>,
    pub min_interval_ms: u32,
    pub safety_buffer_requests: Option<u32>,
    pub max_output_tokens: u32,
}
impl Default for Provider {
    /// An entry without an explicit `enabled` must never auto-activate: catalog
    /// presets and hand-written TOML entries stay off until the operator checks
    /// the account quota and enables them deliberately.
    fn default() -> Self {
        Self {
            id: String::new(),
            enabled: false,
            base_url: String::new(),
            api_key: String::new(),
            api_key_env: String::new(),
            model: String::new(),
            requests_per_minute: 0,
            requests_per_hour: None,
            requests_per_day: None,
            requests_per_month: None,
            tokens_per_minute: None,
            tokens_per_day: None,
            min_interval_ms: 0,
            safety_buffer_requests: None,
            max_output_tokens: 1024,
        }
    }
}
impl std::fmt::Debug for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Provider")
            .field("id", &self.id)
            .field("enabled", &self.enabled)
            .finish_non_exhaustive()
    }
}
impl Provider {
    pub fn key(&self) -> String {
        if self.api_key_env.is_empty() {
            self.api_key.clone()
        } else {
            std::env::var(&self.api_key_env).unwrap_or_default()
        }
    }
    pub fn buffer(&self, cfg: &Config) -> u32 {
        self.safety_buffer_requests
            .unwrap_or(cfg.safety_buffer_requests)
    }
    /// Account-level bucket: renaming, reordering, model changes or different API
    /// paths on the same origin must not reset a key's quota. Never persist keys.
    pub fn bucket(&self, key: &str) -> String {
        use sha2::{Digest, Sha256};
        let origin = url::Url::parse(&self.base_url)
            .map(|u| u.origin().ascii_serialization())
            .unwrap_or_default();
        hex::encode(Sha256::digest(format!("{origin}\0{key}")))
    }
}
impl Config {
    pub fn validate(&self) -> Result<()> {
        let path = &self.endpoint_path;
        let reserved = [
            "api",
            "gpu",
            "inst",
            "static",
            "login",
            "settings",
            "do",
            "offers",
            "performance",
            "instances",
            "term",
            "schedules",
            "assets",
            "healthz",
            "readyz",
        ];
        ensure!(path.starts_with('/') && path.len() >= 2 && path.len() <= 128
            && !path.ends_with('/') && !path.contains("//")
            && path.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'/' || b == b'-' || b == b'_')
            && !reserved.contains(&path.split('/').nth(1).unwrap_or("")),
            "free_router.endpoint_path: use an absolute path such as /free/v1 or /v1, without trailing slash, query or reserved dashboard/API prefixes");
        ensure!(
            self.providers.len() <= 64,
            "free_router: at most 64 providers"
        );
        ensure!(
            self.safety_buffer_requests <= 10000,
            "free_router: invalid safety buffer"
        );
        let mut ids = HashSet::new();
        for (i, p) in self.providers.iter().enumerate() {
            // Never include submitted values in errors (URLs can contain secrets).
            ensure!(
                !p.id.is_empty()
                    && p.id.len() <= 64
                    && p.id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                    && ids.insert(&p.id),
                "free_router provider #{}: invalid/duplicate ID",
                i + 1
            );
            let url = url::Url::parse(&p.base_url).ok();
            ensure!(url.as_ref().is_some_and(|u| {
                // Plain HTTP only on loopback, useful for local adapters and tests.
                (u.scheme() == "https" || (u.scheme() == "http" && matches!(u.host_str(), Some("127.0.0.1" | "[::1]" | "localhost"))))
                    && u.host_str().is_some() && u.username().is_empty() && u.password().is_none()
                    && u.query().is_none() && u.fragment().is_none()
            }), "free_router provider #{}: require HTTPS base URL without credentials/query/fragment", i + 1);
            ensure!(
                p.api_key.len() <= 8192 && !p.api_key.chars().any(char::is_control),
                "free_router provider #{}: invalid key",
                i + 1
            );
            ensure!(
                p.api_key_env.len() <= 128
                    && p.api_key_env
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
                "free_router provider #{}: invalid environment variable name",
                i + 1
            );
            ensure!(
                p.model.len() <= 256 && !p.model.chars().any(char::is_control),
                "free_router provider #{}: invalid model",
                i + 1
            );
            ensure!(
                (1..=131072).contains(&p.max_output_tokens),
                "free_router provider #{}: invalid output token cap",
                i + 1
            );
            let buffer = p.buffer(self);
            ensure!(
                buffer <= 10000
                    && p.requests_per_minute <= 1_000_000
                    && p.min_interval_ms <= 86_400_000,
                "free_router provider #{}: invalid limits",
                i + 1
            );
            for limit in [
                p.requests_per_hour,
                p.requests_per_day,
                p.requests_per_month,
                p.tokens_per_minute,
                p.tokens_per_day,
            ]
            .into_iter()
            .flatten()
            {
                ensure!(
                    limit > 0,
                    "free_router provider #{}: optional limits must be positive or omitted",
                    i + 1
                );
            }
            if p.enabled {
                ensure!(!p.model.trim().is_empty() && p.requests_per_minute > buffer, "free_router provider #{}: model required and RPM must exceed reserve (override reserve for small quotas)", i + 1);
                ensure!(
                    [
                        p.requests_per_hour,
                        p.requests_per_day,
                        p.requests_per_month
                    ]
                    .into_iter()
                    .flatten()
                    .all(|l| l > buffer),
                    "free_router provider #{}: request quotas must exceed reserve",
                    i + 1
                );
            }
        }
        Ok(())
    }
}
