//! Asset-Server: private Assets (reference.wav, runtime-settings.json,
//! Workflows, Caches) liegen im Router-Volume; Agents ziehen sie beim Start
//! (§14 Bauplan). Manifest aus Verzeichnis-Scan + Hash-Cache (SQLite),
//! GET mit Range/ETag für Resume.

use crate::db::Db;
use crate::state::SharedApp;
use axum::body::Body;
use axum::extract::{Path, Request};
use axum::http::{header, StatusCode};
use axum::http::HeaderValue;
use axum::response::{IntoResponse, Response};
use praxis_common::node::{AssetEntry, AssetManifest};
use sha2::{Digest, Sha256};
use std::path::{Path as StdPath, PathBuf};

const ALL: &str = "all";

fn assets_root(app: &SharedApp) -> PathBuf {
    app.cfg.router.data_dir.join("assets")
}

/// Rolle-Anteil: llm/media/all.
fn dirs_for_role(app: &SharedApp, role: &str) -> Vec<PathBuf> {
    let root = assets_root(app);
    vec![root.join(role), root.join(ALL)]
}

/// Meta-Sidecar `<file>.meta.toml` → target/mode/restart/required.
#[derive(Default, serde::Deserialize)]
struct Meta {
    #[serde(default)]
    target: String,
    #[serde(default)]
    mode: String,
    #[serde(default)]
    restart: String,
    #[serde(default)]
    required: bool,
}

fn meta_for(file: &StdPath) -> Meta {
    let side = file.with_extension(format!("{}.meta.toml", file
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()));
    let side = if side.exists() {
        side
    } else {
        // auch <file>.meta.toml ohne Extra-Extension
        let mut s = file.as_os_str().to_os_string();
        s.push(".meta.toml");
        StdPath::new(&s).to_path_buf()
    };
    if !side.exists() {
        return Meta::default();
    }
    toml::from_str(&std::fs::read_to_string(side).unwrap_or_default()).unwrap_or_default()
}

fn sha256_cached(db: &Db, file: &StdPath) -> String {
    let key = file.to_string_lossy().to_string();
    let mtime = file
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Cache-Key mit mtime: Hash nur bei Änderung neu rechnen.
    let cache_key = format!("{key}\u{1}{mtime}\u{1}{}", file.metadata().map(|m| m.len()).unwrap_or(0));
    if let Some(cached) = db.asset_hash(&cache_key) {
        return cached;
    }
    let mut hasher = Sha256::new();
    if let Ok(mut f) = std::fs::File::open(file) {
        use std::io::Read;
        let mut buf = [0u8; 64 * 1024];
        loop {
            match f.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => hasher.update(&buf[..n]),
                Err(_) => break,
            }
        }
    }
    let sha = hex::encode(hasher.finalize());
    let _ = db.set_asset_hash(&cache_key, &sha);
    sha
}

/// Manifest für eine Rolle (all + role).
pub fn manifest_for_role(app: &SharedApp, role: &str) -> AssetManifest {
    let mut assets = Vec::new();
    for dir in dirs_for_role(app, role) {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue }
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            if name.ends_with(".meta.toml") {
                continue;
            }
            let meta = meta_for(&path);
            let target = if meta.target.is_empty() {
                // Default-Target aus Verzeichnisstruktur: /workspace/<rel>
                let root = assets_root(app);
                let rel = path
                    .strip_prefix(&root)
                    .or_else(|_| Ok::<&std::path::Path, ()>(&path))
                    .unwrap()
                    .components()
                    .skip(1) // Gruppen-Präfix (all|llm|media) abschneiden
                    .collect::<std::path::PathBuf>();
                format!("/workspace/{}", rel.to_string_lossy().trim_start_matches('/'))
            } else {
                meta.target.clone()
            };
            assets.push(AssetEntry {
                id: format!("{}:{}", dir_relative_label(&path, app), name),
                target,
                size: path.metadata().map(|m| m.len()).unwrap_or(0),
                sha256: sha256_cached(&app.db, &path),
                mode: if meta.mode.is_empty() { "0644".into() } else { meta.mode },
                restart: meta.restart,
                required: meta.required,
            });
        }
    }
    AssetManifest { assets }
}

fn dir_relative_label(path: &StdPath, app: &SharedApp) -> String {
    let rel = path.strip_prefix(assets_root(app)).unwrap_or(path);
    rel.parent()
        .and_then(|p| p.to_str())
        .unwrap_or(ALL)
        .to_string()
}

/// `GET /api/v1/node/assets/manifest`
pub async fn node_manifest(app: SharedApp, token: String) -> Response {
    let Some(inst) = app.db.instance_by_token(&token) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let role = serde_json::to_string(&inst.role).unwrap_or_default().trim_matches('"').to_string();
    let m = manifest_for_role(&app, &role);
    axum::Json(m).into_response()
}

/// `GET /api/v1/node/assets/{id}` — Range-fähig, ETag/304.
pub async fn node_asset(
    app: SharedApp,
    token: String,
    Path(id): Path<String>,
    req: Request,
) -> Response {
    let Some(inst) = app.db.instance_by_token(&token) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let role = serde_json::to_string(&inst.role).unwrap_or_default().trim_matches('"').to_string();
    let m = manifest_for_role(&app, &role);
    let Some(_entry) = m.assets.into_iter().find(|a| a.id == id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let file = asset_path_by_id(&app, &id);
    let Some(file) = file else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let sha = sha256_cached(&app.db, &file);
    let etag = format!("\"{sha}\"");
    if let Some(h) = req.headers().get(header::IF_NONE_MATCH) {
        if h.as_bytes() == etag.as_bytes() {
            return Response::builder().status(StatusCode::NOT_MODIFIED).header(header::ETAG, etag).body(Body::empty()).unwrap();
        }
    }
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::ETAG, &etag)
        .header("x-asset-sha256", &sha)
        .header("x-asset-size", len.to_string());
    // Range-Unterstützung (Resume von .part-Downloads im Agent).
    let range = req.headers().get(header::RANGE).and_then(|r| r.to_str().ok()).map(|s| s.to_string());
    if let Some(range) = range {
        if let Some(start) = range.strip_prefix("bytes=").and_then(|r| r.split('-').next()).and_then(|s| s.parse::<u64>().ok()) {
            if start < len {
                let file = std::fs::File::open(&file).unwrap();
                use std::io::{Seek, SeekFrom};
                let mut file = file;
                let _ = file.seek(SeekFrom::Start(start));
                let stream = tokio_util_stream_once_then_chunked(file);
                builder = builder
                    .status(StatusCode::PARTIAL_CONTENT)
                    .header(header::CONTENT_RANGE, format!("bytes {start}-{}/{}", len - 1, len));
                return builder.body(Body::from_stream(stream)).unwrap();
            }
        }
    }
    if let Ok(f) = std::fs::File::open(&file) {
        builder = builder.header(header::CONTENT_TYPE, mime_for(&file));
        let stream = tokio_util_stream_once_then_chunked(f);
        return builder.body(Body::from_stream(stream)).unwrap();
    }
    StatusCode::NOT_FOUND.into_response()
}

fn mime_for(file: &StdPath) -> String {
    match file.extension().and_then(|e| e.to_str()) {
        Some("wav") => "audio/wav".into(),
        Some("json") => "application/json".into(),
        Some("toml") => "application/toml".into(),
        Some("zst") => "application/zstd".into(),
        Some("tar") => "application/x-tar".into(),
        _ => "application/octet-stream".into(),
    }
}

/// Datei zur Asset-ID finden (id = "<dirlabel>:<name>").
fn asset_path_by_id(app: &SharedApp, id: &str) -> Option<PathBuf> {
    let (label, name) = id.split_once(':')?;
    let dir = assets_root(app).join(label);
    let path = dir.join(name);
    if path.is_file() {
        Some(path)
    } else {
        None
    }
}

/// 64-KiB-Chunk-Stream über eine std::File (spawn_blocking-Reader).
fn tokio_util_stream_once_then_chunked(file: std::fs::File) -> impl futures::Stream<Item = Result<bytes::Bytes, std::io::Error>> {
    use futures::stream::unfold;
    let file = std::sync::Arc::new(std::sync::Mutex::new(file));
    unfold(file, |file| async move {
        let f = file.clone();
        let chunk = tokio::task::spawn_blocking(move || {
            use std::io::Read;
            let mut buf = vec![0u8; 64 * 1024];
            match f.lock().unwrap().read(&mut buf) {
                Ok(0) => None,
                Ok(n) => {
                    buf.truncate(n);
                    Some(Ok(bytes::Bytes::from(buf)))
                }
                Err(e) => Some(Err(e)),
            }
        })
        .await
        .ok()
        .flatten();
        chunk.map(|c| (c, file))
    })
}

/// Dashboard: Assets auflisten (per Rolle).
#[allow(dead_code)]
pub async fn list(app: SharedApp) -> Response {
    let mut out = serde_json::json!({});
    for role in [ALL, "llm", "media"] {
        let m = manifest_for_role(&app, role);
        out[role] = serde_json::to_value(&m.assets).unwrap_or_default();
    }
    axum::Json(out).into_response()
}

/// Dashboard: Upload (multipart) → /data/assets/<scope>/<name>.
pub async fn upload(
    app: SharedApp,
    scope: String,
    name: String,
    req: Request,
) -> Response {
    let scope = if scope == ALL { ALL.to_string() } else { scope };
    if !["llm", "media", ALL].contains(&scope.as_str()) {
        return (StatusCode::BAD_REQUEST, "scope must be llm|media|all").into_response();
    }
    let safe: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '/')).collect();
    if safe.is_empty() || safe.contains("..") {
        return (StatusCode::BAD_REQUEST, "invalid name").into_response();
    }
    let dir = assets_root(&app).join(&scope);
    let target = dir.join(&safe);
    if let Some(parent) = target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let body = req.into_body();
    use http_body_util::BodyExt;
    match body.collect().await {
        Ok(c) => {
            let bytes = c.to_bytes();
            if std::fs::write(&target, &bytes).is_err() {
                return (StatusCode::INTERNAL_SERVER_ERROR, "write failed").into_response();
            }
            app.events.emit(
                &app.db,
                "asset_uploaded",
                None,
                None,
                &format!("asset {}/{} ({} Bytes)", scope, safe, bytes.len()),
                &serde_json::json!({"scope": scope, "name": safe, "bytes": bytes.len()}),
            );
            (StatusCode::OK, format!("uploaded {}/{}", scope, safe)).into_response()
        }
        Err(_) => (StatusCode::BAD_REQUEST, "body error").into_response(),
    }
}

/// HeaderValue helper.
#[allow(dead_code)]
pub fn hv(s: &str) -> HeaderValue {
    HeaderValue::from_str(s).unwrap_or(HeaderValue::from_static("x"))
}