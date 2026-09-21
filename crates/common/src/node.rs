//! WS-Protokoll Router ↔ gpu-agent (`/api/v1/node`).
//!
//! Agent → Router: `hello`, `heartbeat`, `event`, `cmd_result`, `term`
//! Router → Agent: `cmd` (restart_service, tail, exec, sync_assets,
//! run_acceptance, drain), `term_in`, `term_resize`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// GPU-Auslastung (aus `nvidia-smi --query-gpu=...`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GpuStats {
    #[serde(default)]
    pub util_pct: f64,
    #[serde(default)]
    pub mem_used_mb: f64,
    #[serde(default)]
    pub mem_total_mb: f64,
}

/// Download-Fortschritt (Bytes unter /workspace vs. Manifest).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Progress {
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub done_bytes: u64,
    #[serde(default)]
    pub total_bytes: u64,
}

impl Progress {
    pub fn pct(&self) -> Option<f64> {
        if self.total_bytes > 0 {
            Some(self.done_bytes as f64 / self.total_bytes as f64 * 100.0)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NodeMessage {
    Hello {
        token: String,
        role: String,
        agent_version: String,
        #[serde(default)]
        nb_ip: Option<String>,
        #[serde(default)]
        hostname: Option<String>,
        #[serde(default)]
        services: HashMap<String, bool>,
    },
    Heartbeat {
        health: crate::AgentHealth,
        #[serde(default)]
        busy: bool,
        #[serde(default)]
        busy_reason: String,
        #[serde(default)]
        gpu: GpuStats,
        #[serde(default)]
        progress: Progress,
        #[serde(default)]
        disk_free_gb: f64,
        #[serde(default)]
        net_rx_gb: f64,
    },
    Event {
        kind: String,
        #[serde(default)]
        payload: serde_json::Value,
    },
    CmdResult {
        id: u64,
        ok: bool,
        #[serde(default)]
        data: serde_json::Value,
    },
    /// Terminal-Output eines exec/term-Streams.
    Term {
        id: u64,
        stream: String,
        #[serde(default)]
        data: String,
    },
    /// Agent beendet einen exec/term-Stream.
    TermEnd {
        id: u64,
        #[serde(default)]
        code: Option<i32>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RouterCommand {
    Cmd {
        id: u64,
        #[serde(flatten)]
        command: Command,
    },
    /// stdin für einen offenen exec/term-Stream.
    TermIn { id: u64, data: String },
    /// PTY-Resize (Browser-Größe → Agent).
    TermResize { id: u64, cols: u16, rows: u16 },
    TermClose { id: u64 },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    RestartService { service: String },
    StopService { service: String },
    StartService { service: String },
    /// Letzte Zeilen einer Datei (tail -n).
    Tail { file: String, lines: u32 },
    /// Einmalige Kommandoausführung, Output als cmd_result.
    Exec { argv: Vec<String> },
    /// Interaktive bash-Session (Pipes, kein vty) für das Dashboard-Terminal.
    TermOpen { cols: u16, rows: u16 },
    SyncAssets,
    /// Push über die WS-Session (Outbound der Boxen ist auf Vast tot):
    /// Router schickt das Manifest; der Agent prüft lokal und antwortet
    /// (cmd_result) mit den fehlenden IDs.
    PushAssetsManifest { manifest: Vec<AssetEntry> },
    /// Push: Asset-Bytes Base64 (kleine Dateien; große Caches bleiben
    /// lokal/HTTP). Agent schreibt, prüft SHA, chmod, restartet Service.
    PushAssetData { entry: AssetEntry, data_b64: String },
    RunAcceptance,
    /// Busy-Flag am Agent setzen (Router meint: Drain/Fertig).
    Drain,
    Undrain,
}

/// Eintrag im Asset-Manifest (`GET /api/v1/node/assets/manifest`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssetEntry {
    pub id: String,
    pub target: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub sha256: String,
    #[serde(default = "default_mode")]
    pub mode: String,
    /// supervisor-Programm, das nach Sync neugestartet werden soll.
    #[serde(default)]
    pub restart: String,
    /// Wenn true: `healthy` erst melden, wenn Asset vorhanden.
    #[serde(default)]
    pub required: bool,
}

fn default_mode() -> String {
    "0644".into()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssetManifest {
    pub assets: Vec<AssetEntry>,
}