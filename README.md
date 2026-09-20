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
                      ├─ vast client · netbird mgmt · agent-connector (Router wählt ein)
                      └─ SQLite (/data)
                               │ NetBird (netstack + local forwarding, ACL s. unten)
                               ▼
                 GPU-Instanz (vast) → supervisord → netbird · services · gpu-agent(:9100, loopback)
```

**Richtung ist wichtig:** Outbound der Vast-Boxen ist blockiert (Netstack-
SOCKS5 empirisch tot — `assets_synced failed:1` x7 bei korrekter ACL), also
wählt der ROUTER sich in die Agenten ein (`ws://<nb_ip>:9100` via Netstack
Local Forwarding) und **pusht Assets über die Session**. Call-home bleibt
als Fallback (`PRAXIS_AGENT_CALL_HOME=1`, lokal/wo Outbound geht).

**NetBird-ACL (tgrid):** Boxen werden via Mint in `Gpuserver` **und**
`servers` engerollt — ohne `servers` greift `developers→servers` nicht und
Router/wasser erreichen die Boxen nicht. Policies:
- „praxis gpu router": `Gpuserver→test:8080` (Call-home/WS)
- „praxis to gpu router" (20.09. neu): `Gpuaccess→test:8080,8188,2700,11434-36`
  — ohne die war Praxis (pagent) vom Router komplett abgeschnitten
  (LLM-Fail „temporarily unavailable ×5"). Achtung NetBird-API: Regeln mit
  gleicher ID in einem PUT werden verworfen → separate Policy pro Regel.
Boxen heißen `gpu-<role>-<tok8>` (netbird up --hostname im Fork-Entrypoint).

## Bauplan-Status (v2)

| Phase | Inhalt | Status |
|---|---|---|
| 0 | Spike: volle Kette live | ✅ **2026-09-20 nachts bewiesen**: Mieten → Enroll → Connector-Dial :9100 → Asset-Push über WS (Datei auf der Box verifiziert, idempotent) → Healthy → Flip → Proxy `100.105.6.69:8188` + `/gpu/2/comfy/…` + `/inst/…` alle 200, Idle-Stop, PREEMPT→Auto-Replace, Peer-Cleanup bei Destroy |
| 1 | vast + policy + SQLite + Metering | ✅ (34 Unit-Tests; Vast-API v1) |
| 2 | gpu-agent | ✅ v0.1.2: lauscht 127.0.0.1:9100, Router wählt sich ein; Health-Probes direkt auf Loopback; Asset-Push-Handler + Readiness-Gate (§14.2) |
| 3 | Proxy | ✅ live bewiesen (Passthrough, Pfad-Strip, /inst, STT-Sidecar, X-GPU-*-Header) |
| 4 | Reconciler + Policy live | ✅ erprobt: Wake, Idle-Stop, PREEMPTED→Auto-Replace, Budget-Alerts + budget-geblockter Start, Warming-Gate gegen Doppel-Miete |
| 5 | Dashboard | ✅ gebaut, alle Seiten rendern 200; Host-Bilanz + Blacklist-Knöpfe auf `/offers`; Klick-Fluss im Browser noch offen |
| 6 | Praxis-Integration | ✅ **2026-09-20 mittags bewiesen**: Praxis (pagent) → Router → llama-server, `HTTP 200`, Chat + Folge-Turn mit Verlauf („ok“-Test ×2). Ursprünglicher 500er war kein Router-Problem: Qwen3.6-GGUF-Template wirft „System message must be at the beginning“, Praxis injiziert System-Notizen mitten im Verlauf → Fix im Praxis-llamacpp-Adapter (alle system-Messages an Position 0 mergen, `testing` 0f149a6, auf pagent deployed als Daemon). Details s. „Stand 20.09. mittags“ |
| 7 | VPS-Umzug | compose ist host-unabhängig |

## Stand 20.09. mittags — Kette komplett live

**Praxis läuft auf eigener GPU-Infra.** E2E-Test (SSE-Session 44a06d92 auf
pagent, `/v1/chat`): `HTTP 200 nach 16.5 s, success: True, "ok"` — plus
Folge-Turn mit Verlauf. Box: 51735121 (RTX 5060 Ti, on_demand 0.2867 $/h,
~42 tok/s, 5224 Prompt-Tokens auf der Box verifiziert).

Was dazukam:
- **Router 0.4** (30b1db9): Idle-Uhr nach Hot-Swap — eine frisch geflippte
  Ersatzbox erbte die Idle-Uhr des Vorgängers und wurde **33 s nach dem
  Flip** als „idle seit 920 s“ gestoppt (→ die 502er im ersten E2E-Versuch).
  Jetzt `max(last_request, healthy_since)`. Dazu: Create-Snapshot-Race-Guard
  (wake/„preempted: replace“ prüfen bei Ausführung, ob inzwischen healthy
  aktiv — sonst Doppel-Miete) und `set_instance_state` räumt healthy/busy
  bei preempted/stopped ab (stale-healthy-Anzeige-Bug).
- **Router 0.5 — Slot-Mietmodus `on_demand`** (8c8f1af): `mode = "on_demand"`
  im Slot-Config mietet zum Listenpreis (dph_total) statt min_bid+Margin —
  outbid-sicher. Auslöser: H200-Schnäppchen 0.0153 $/h war Minuten nach
  Miete outbid; Interruptible-Churn kostete den halben Tag. Ranking im
  OD-Modus nach dph_total, Ceiling greift weiter, kein Kosten-Optimierungs-
  Churn zurück in den Interruptible-Markt. Default bleibt interruptible.
- **Praxis-Fix** (`testing`, 0f149a6): llamacpp-Adapter normalisiert — alle
  system-Rollen zu EINER führenden System-Message (Cloud-Provider tolerieren
  mid-history-system, strikte GGUF-Templates nicht). 3 Unit-Tests.
- **Deploy pagent**: Praxis läuft als Daemon (`~/release/start-praxis.sh`,
  Login-Shell-PATH für nvm/POML!, Log `~/release/logs/praxis-console.log`),
  `.env`: `LLAMACPP_API_BASE=http://100.105.6.69:11434`,
  `GATEWAY_API_KEY` (min. 16 Zeichen), Comfy/Vosk-Settings zeigen auf Router.
  Build auf pagent: `~/praxis` (branch `testing`, warmes target, ~5 min).

### Offene Punkte (nächste Session)
- **GpuRouterClient in Praxis** (Bauplan §12.4): 503+Retry-After des Routers
  behandeln (kalter Slot nach Idle-Stop), Wake bei Session-Start,
  X-Router-Job-Id für Batches. Bis dahin: erster Chat nach 15-min-Idle-Stopp
  failt 5× (Praxis-Cooldown < Restart-Dauer), zweiter geht durch (Box warm,
  Restart ~1–2 min).
- Budget zurück auf 2.0/2.4 (heute testweise 4.0/4.4) — und Entscheidung,
  ob Slot 1 im Interruptible-Schnäppchen-Modus bleibt (Optimum) oder
  on_demand (Stabilität, ~0.23–0.29 $/h).
- pagent: Passwort `marvin1015` ändern, `GATEWAY_API_KEY` echtes Secret.
- Dashboard-Kosmetik: `active_instance` zeigt bis zum Flip die alte Box.
- Browser-Test Dashboard-Klick-Fluss (Blacklist-Knöpfe etc.).
- **Machine-Blacklist** ✅ 20.09. live: `machine_stats` zählt Fails (preempt während warmup, instance_gone im Warmup, warmup-timeout, agent >3 min still), Auto-Blacklist nach `vast.blacklist_after_fails` (Default 2), `best_candidate` filtert blacklisted Hosts, Offer-Score durch `reliability2` geteilt. Erster Live-Fang: Host 32560 + 144003 je Fail #1 in der ersten Stunde. Dashboard `/offers` zeigt Host-Bilanz + Blacklist-Knöpfe; API `GET /api/v1/machines`, `POST /api/v1/machines/:id/blacklist|unblacklist`.
- **False-Preempt-Schutz** ✅ 20.09.: `cur_state=stopped` zählt nicht mehr als tot, solange `actual` noch loading/created ist; echte Preempts (actual=exited) weiterhin erkannt.
- Alert-Dedupe (30 min/Art — budget_80 spamte alle 30 s), Audit-Trail `slot_desired` (wer setzt desired), Reconciler-Loop entkoppelt Offer-Refresh (blockierendes tick() verzog jede Wake-Reaktion), Connector dialt preempted Boxen nicht mehr.
- **Idle-ab-Healthy** ✅ 20.09.: 50-min-Warmup-Box wurde 29 s nach dem Flip gestoppt (Idle zählte ab Miete) — jetzt `healthy_since` in DB, Idle-Uhr ab Healthy/letztem Traffic. Nebenbefund gefixt: `prewarm_start` lief an fensterfreien Tagen Some und verschluckte den „Fenster zu → Stop".
- **Preempted-Warmup-Zombies** ✅ 20.09.: nie-healthy-gewordene preempted Boxen werden aufgeräumt und belegen `max_per_slot` nicht mehr (sonst blockieren sie jeden Wake-Replace). Heartbeat kann PREEMPTED nicht mehr auf booting zurücksetzen (zählte Doppel-Fails).
- **LLM-Slot live** ✅ 20.09. abends–mittags: `qwen3.6-35b-a3b` (20.4 GB, Guide-Modell) statt Flash-Next (82 GB braucht Blackwell — moe-cache aktivierte auf Ada nicht, Fork-Guard griff; 0.30 $/h verbrannt, dann umgestellt). Download-Progress über Agent klappt (`PRAXIS_AGENT_MODEL_TOTAL_BYTES`), Warmup auf 60 min erhöht. Volle Kette inkl. Praxis-Chat bewiesen (s. „Stand 20.09. mittags").
- **Unreachable-Stopper**: Box mit vast=running + Agent >5 min tot wird gestoppt (Disk warm, kein GPU-Geld-Brennen); `resume_fallback_fresh` zerstört die alte Box nach 3× fehlgeschlagenem Start (blockiert sonst den Wake-Pfad).
- **LLM-Image bauen** ✅ (CUDA-Build ≈ 30–60 min, 0.3 mit cmake-Retry-Loop gegen
  cicc-SIGSEGV/Error 139) + Slot-1-Test ✅ — siehe oben.
- Praxis-Session-Hook (wake bei Session-Start), GpuRouterClient (X-Router-Wait/Job-Id) — jetzt Haupt-Nacharbeit (s. o.), Browser-Test Dashboard.

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
Router-NetBird-IP zeigen lassen — Ports bleiben gleich. In Praxis (live,
pro Kontext):

```
/context set settings.comfyui_base_url=http://100.105.6.69:8188
/context set settings.voice_vosk_url=ws://100.105.6.69:2700/de
```

(STT läuft router-lokal als Sidecar; Sprachsuffix `/de`|`/fr`|`/ja` wie bisher —
die Routen `/`, `/ws`, `/ws/{lang}`, `/{lang}` existieren alle.)

LLM: Praxis erreicht llama.cpp über `LLAMACPP_API_BASE` (env, Standard
`http://localhost:11434`) → im aktiven Install auf
`http://100.105.6.69:11434` setzen (`.env` des Praxis-Release). Optional:
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
unter `/data/assets/{all,llm,media}` im Router-Volume; der **Router pusht sie
über die Agent-WS-Session** (Manifest → Agent prüft SHA → fehlende Bytes als
Base64; `.meta.toml`-Sidecars mit `target/mode/restart/required`). Große
Dateien (>48 MB) bewusst außen vor (Range-HTTP-Endpunkte bleiben als
Fallerückweg). Fehlende `required`-Assets → Agent meldet `Degraded`
(assets_missing) → kein Flip ohne Referenzstimme. Upload im Dashboard.