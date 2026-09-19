# pgpu — Praxis GPU Router

Vast.ai-Interruptible-Verwaltung für Praxis: **Slots statt Instanzen.**
Slot 1 = Rolle `llm` (fork-llama), Slot 2 = Rolle `media` (fork-comfyui).
Vast-Instanzen backen einen Slot; beim Hot-Swap flippt der Slot auf die
neue Instanz, danach ist sie der Slot.

```
Praxis ──NetBird──► ROUTER (pgpu)
                      ├─ Proxy :8188 :2700 :11434 :11435 :11436 → aktive Instanz
                      ├─ Dashboard :8080 (askama+HTMX+SSE, xterm-Terminal)
                      ├─ Reconciler (30 s) ─ Policy (Budget/Bid/Swap/Idle, getestet)
                      ├─ vast client · node-agent hub (WS call-home) · assets server
                      └─ SQLite (/data)
                               │ NetBird
                               ▼
                 GPU-Instanz (vast) → supervisord → netbird · services · gpu-agent
```

## Bauplan-Status (v2)

| Phase | Inhalt | Status |
|---|---|---|
| 0 | Spike | — (bald: billigste GPU + Templates live) |
| 1 | vast + policy + SQLite + Metering | ✅ (26 Unit-Tests) |
| 2 | gpu-agent | ✅ ([praxis-gpu-agent](https://forgejo.the.grid/Marvin/praxis-gpu-agent)) |
| 3 | Proxy (Passthrough/Pfad, WS/SSE, Drain, 503/Hold) | ✅ |
| 4 | Reconciler + Policy live | ✅ (gegen Live-Daten validieren) |
| 5 | Dashboard | ✅ (Overview/Offers/Instanz/Terminal/Assets/Settings) |
| 6 | Praxis-Integration | URL-Wechsel reicht (gleiche Ports) |
| 7 | VPS-Umzug | compose ist host-unabhängig |

## Crates

- `common` — Protokoll Router↔Agent, Rollen/Modi/Lifecycle/State-Maschine
- `vast` — v0-API-Client (bundles-Suche, asks, bid_price, status, credit, logs)
- `policy` — reine Entscheidungen (`decide(snapshot, cfg)`), 100 % getestet
- `router` — axum: Proxy, API, Dashboard, Reconciler, SQLite, Hub, Assets

## Deploy

```sh
cp config.example.toml deploy/config.toml   # bind_ip = NetBird-IP, Keys, Token
export ROUTER_TOKEN=... VAST_API_KEY=...
bash deploy/build.sh                        # vayayo/praxis-gpu-router
IMAGE_TAG=vayayo/praxis-stt:0.1 bash deploy/stt/build.sh
docker compose -f deploy/docker-compose.yml up -d
```

Ports (alle an `bind_ip` = NetBird-IP gebunden, kein EXPOSE im Image):
- **Passthrough** (drop-in für Praxis, gleiche Ports wie direkt): `8188` → media/comfy, `2700` → STT (Router-Sidecar), `11434/11435/11436` → llm api/webui/embed
- **Dashboard**: `8080` — `/`, `/offers`, `/schedules`, `/assets`, `/settings`, `/api/v1/*`
- **Pfad-Routing**: `/gpu/<slot>/<svc>/…`, `/inst/<vast_id>/<svc>/…`

## Praxis-Anbindung

`settings.llama_base_url`/`comfyui_base_url`/`voice_vosk_url` auf die
Router-NetBird-IP zeigen lassen — Ports bleiben gleich. Optional:
`X-Router-Wait: <sek>` hält Requests auf kaltem Slot bis healthy
(impliziter Wake), `X-Router-Job-Id` hält den Slot busy (Batches),
`X-Router-Priority: critical` für on-demand-Wünsche.

## Instanzen (Templates)

- Slot-Env beim `create`: `NB_SETUP_KEY` (ephemer via NetBird-API),
  `NB_HOSTNAME=gpu-<role>-<token8>`, `NB_SOCKS5_LISTENER_PORT=1080`,
  `ROUTER_URL`, `PRAXIS_NODE_TOKEN`, slot-spezifisch (`LLAMA_MODEL`, …)
- Die Forks backen den Agent ein:
  `COPY --from=vayayo/praxis-gpu-router:latest /gpu-agent /opt/praxis/bin/gpu-agent`
  + `[program:gpu-agent]` (priority 15, autorestart).

## Assets

Private Dateien (`reference.wav`, `runtime-settings.json`, Workflows) liegen
unter `/data/assets/{all,llm,media}` im Router-Volume; Agents ziehen sie
beim Start (SHA256, Range-Resume, `.meta.toml`-Sidecars mit
`target/mode/restart/required`). Upload im Dashboard.