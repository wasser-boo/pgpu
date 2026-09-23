# pgpu — Praxis GPU Router

> **Aktueller Qualitätsstatus:** Kernpfade gehärtet, automatisierte Rust-Tests
> und isolierte Prozess-/Browser-Smoke-Tests. Noch **keine kommerzielle Freigabe**.
> Änderungen, Upgrade-Hinweise und konkrete offene Release-Gates:
> [Produktionsreife / Hardening](docs/production-readiness.md).
> **Bauplan inklusive Nice-to-haves noch nicht vollständig:**
> [aktueller Abgleich, Restarbeiten und Image-Abhängigkeiten](docs/plan-audit.md).
> Die folgenden datierten Live-Berichte beschreiben frühere Entwicklungsstände.

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

## Patch 0.26.1

- Vast-Kostenabgleich über `/api/v0/charges/`: HTTP-301 behoben, Redirect-Schutz bleibt erhalten. Übersichtliche Charges-Statusanzeige statt Provider-HTML, tatsächliche Nutzung separat von lokalen Schätzungen.
- Bid- und On-demand-Preise werden nicht mehr verwechselt; Compute und Speicher zählen genau einmal. Aktueller On-demand-Preis in HTML; Stundenlimit-Meldung nennt Kostenanteile und Cap, nicht ein vermeintliches API-Limit.
- Informative Webhooks nach Miete und erster geprüfter Router-Freigabe, bei echten Instanz-/Slot-Zustandswechseln und Backend-Ersetzungen. Keine unveränderten Heartbeats; dauerhafte Deduplizierung.
- **Alle vier Stunden** eine Ausgabenübersicht mit Tages-/Monatsverbrauch, Restbudget, Stundenkosten, Slot-Status und Aktualität der Vast-Daten (`spend_summary_interval_s = 14400`).

Slot-Label-Scope, bisherige Budget-/Speicherhistorie, Limits und Währungskurs bleiben erhalten. Details: [Auswahl & Kosten](docs/selection-and-billing.md), [Webhooks](docs/webhooks.md). Veröffentlichung aktualisiert keine laufende Installation.

## Neu in 0.26: Länderkarte, Kostenabgleich und mehrere Webhooks

- Länderlisten und Ausschlüsse: `geolocation in [DE,AT,CH]`, `geolocation!=DE`, `geolocation notin [DE,FR]`; ungültige Filter werden abgelehnt.
- Settings-Karte mit Startland Deutschland, optionalem Radius, Länder-Allowlist/Ausschlüssen und mehreren alternativen GPU-Modellen. Länder-Näherung, **kein garantierter Host-Radius**; Ausschlüsse gewinnen immer.
- Tatsächliche Vast-Nutzung nach **Slot-Labels**, nicht nach Online-Status oder Kontostand. Gestoppte/historische passende Verträge zählen; Grenzen bleiben unverändert, Budgetkorrekturen sind konservativ und dauerhaft.
- Sichere, wiederholbare NetBird-Bereinigung nur für nachweislich entfernte eigene GPU-Verträge. Schlafende Verträge und unbekannte Geräte bleiben geschützt.
- Mehrere Webhook-URLs mit separaten Ergebnissen/Wiederholungen, Discord-kompatible Nachrichten und authentifizierter Testbutton/API. Secret-URLs landen nicht im Eventlog.

Anleitungen: [Standort/GPU-Auswahl, Billing und Peer-Cleanup](docs/selection-and-billing.md) · [Discord/Webhooks testen](docs/webhooks.md).

Browser-QA ohne Cloud-Zugang: `cargo build --locked -p praxis-router`, danach `PLAYWRIGHT_MODULE=/pfad/node_modules/playwright node scripts/test-settings.cjs`. Chromium (`CHROMIUM=/pfad/zur/binary`) und Playwright werden nur fürs Testen benötigt, nicht im Router-Image. Der Test startet einen isolierten Router mit leerer Datenbank und zwei Loopback-Webhooks; keine echten Mieten/Benachrichtigungen.

## Ab 0.25: GPU-Messdaten und Host-Auswahl

- Unabhängige `[vast]`-Schalter `activate_blacklist` (Default an) und `activate_whitelist` (Default aus); Ausschalten löscht keine Einträge.
- Lokale Hardware-/Modell-/Kostenprüfung, auch vor manuellen Mieten und Wiederanlauf. Eine Whitelist oder ein guter Score umgeht keine Limits.
- Optional passive Inferenzmetriken, dauerhafter Host-/GPU-Katalog, JSON-Export und `/performance` im Dashboard; keine Prompt-/Antworttexte in der Historie.
- Konfigurierbare, begrenzte LLM-/Disk-Benchmarks mit Agent 0.2.0; automatische Ausführung und scorebasierte Angebotsbevorzugung separat opt-in.

**Anleitung und alle Schalter:** [GPU-Performance und Whitelist](docs/gpu-performance.md), Beispiele in `config.example.toml`. Benchmark-Slots sind während des Tests exklusiv (neue Requests: 503). Diese Funktionen sind ab Image **0.25** enthalten. Neue Image-Veröffentlichungen aktualisieren bestehende Deployments nicht automatisch.

## Bauplan-Status (v2)

| Phase | Inhalt | Status |
|---|---|---|
| 0 | Spike: volle Kette live | ✅ **2026-09-20 nachts bewiesen**: Mieten → Enroll → Connector-Dial :9100 → Asset-Push über WS (Datei auf der Box verifiziert, idempotent) → Healthy → Flip → Proxy `100.105.6.69:8188` + `/gpu/2/comfy/…` + `/inst/…` alle 200, Idle-Stop, PREEMPT→Auto-Replace, Peer-Cleanup bei Destroy |
| 1 | vast + policy + SQLite + Metering | ✅ (34 Unit-Tests; Vast-API v1) |
| 2 | gpu-agent | ✅ v0.1.2: lauscht 127.0.0.1:9100, Router wählt sich ein; Health-Probes direkt auf Loopback; Asset-Push-Handler + Readiness-Gate (§14.2) |
| 3 | Proxy | ✅ live bewiesen (Passthrough, Pfad-Strip, /inst, STT-Sidecar, X-GPU-*-Header) |
| 4 | Reconciler + Policy live | ✅ erprobt: Wake, Idle-Stop, PREEMPTED→Auto-Replace, Budget-Alerts + budget-geblockter Start, Warming-Gate gegen Doppel-Miete |
| 5 | Dashboard | ✅ gebaut, alle Seiten rendern 200; Host-Bilanz + Blacklist-Knöpfe auf `/offers`; Klick-Fluss im Browser noch offen |
| 6 | Praxis-Integration | ✅ **2026-09-20 mittags bewiesen**: Praxis (pagent) → Router → llama-server, `HTTP 200`, Chat + Folge-Turn mit Verlauf („ok“-Test ×2). Ursprünglicher 500er war kein Router-Problem: Qwen3.6-GGUF-Template wirft „System message must be at the beginning“, Praxis injiziert System-Notizen mitten im Verlauf → Fix im Praxis-llamacpp-Adapter (alle system-Messages an Position 0 mergen, `testing` 0f149a6, auf pagent deployed als Daemon). **20.09. abends**: GpuRouterClient (X-Router-Wait/Wake/Job-Id/Cold-503) live, s. „Stand 20.09. abends“ |
| 7 | VPS-Umzug | ✅ **Compose fertig + live getestet** (20.09. abends, auf dem Heim-Server als eigener NetBird-Peer `praxis-vps`): `deploy/vps-compose.yml` + `deploy/vps/README.md` — NetBird+Router+STT+Praxis in einem Namespace, Praxis-Image `vayayo/praxis`, Master-Key-Zustellung ohne env/argv, Env-only-Config, Auto-Miete-Toggle. Später zieht dieselbe Compose 1:1 auf den VPS um |

## Stand 20.09. abends II — Browser-Klicktest: 3 echte Bugs gefunden + gefixt (Router 0.8)

Der ausstehende Dashboard-Klicktest (browser-äquivalent per curl: Login →
Cookie → alle Seiten → alle Form-POSTs) hat drei **produktive Bugs**
aufgedeckt, alle seit Router 0.2 drin:

| Bug | Wirkung | Fix |
|---|---|---|
| Dashboard-Slot-Formen (`wake/stop/destroy/swap/pin/bid`) bauten synthetische API-Requests **ohne Bearer** | `guarded!` → 401 → **stilles No-Op** — jeder Klick tat nichts | `api_slot` setzt Bearer (wie `do_rent` es immer tat) |
| Instanz-Formen (`stop/start/bid/mode/lifecycle/cmd`) ebenfalls ohne Bearer | dito, inkl. supervisorctl-Knöpfe | Header nachgerüstet |
| SSE `/api/v1/events/stream` + Terminal-WS nur Bearer | `EventSource`/Browser-WS können **keine** Header setzen → Live-Feed + xterm im Browser stumm/401 | Cookie-ODER-Bearer (session_ok) |

Dazu: Login validiert jetzt den Token (Fehlermeldung statt stiller
Login-Loop mit Müll-Cookie), `/term/:id` 404 für unbekannte Instanz,
`slot_desired`-Events jetzt auch im Live-Feed (Db broadcastet jedes
`add_event` — Sender in main injiziert, `events.emit` sendet nicht mehr
selbst), Overview-Karte zeigt ohne aktive Instanz die **wärmende Box**
(Download-Progress statt blindem „cold") und beim Hot-Swap einen
„→ Ersatz #id wärmt“-Hinweis, Gebot-Zeile nur bei vorhandener Box.

Live verifiziert: Wake-Form erzeugt Events, Budget-Hard-Cap blockt die
Miete korrekt (Policy-Early-Return), Blacklist/Unblacklist-Knöpfe
schreiben DB, Asset-Upload landet im Volume, SSE liefert wake+stop+
slot_desired live, Proxy 503 `x-router-state: cold` auf kalten Slots,
STT-Sidecar `ready` (de, ja). 43 Unit-Tests grün. Image `0.8` auf Hub,
Compose läuft.

## Stand 20.09. abends — GpuRouterClient live, Auto-Miete-Toggle, VPS-Compose

**Praxis ↔ Router komplett verdrahtet** (Bauplan §12.4, praxis `testing`
f793f58+): `GpuRouterClient` (Env `GPU_ROUTER_URL`/`GPU_ROUTER_TOKEN`/
`GPU_ROUTER_WAIT_S`, ohne URL No-Op) mit:
- **X-Router-Wait im llamacpp-Adapter** — live bewiesen: erster Chat nach
  Kaltstart hielt die Verbindung („proxy: impliziter Wake“), die gestoppte
  Box wurde gestartet; war sie vast-seitig tot (PREEMPTED): Auto-Replace
  mietete frisch, Download-Progress lief, nach Healthy-Flip kam der zweite
  Chat sofort mit `HTTP 200 „ok“` durch. Ohne den Header failte der erste
  Chat 5× am Retry-Backoff (20.09. Vormittag) — damit erledigt.
- **Comfy-503 = typisierter `RouterCold`**: „GPU-Slot warming (Router 503)
  — Wake angestoßen; kein Job übermittelt“ statt irreführendem „job may
  have executed“. Live im pagent-Log gesehen.
- **X-Router-Job-Id**: Router zählt Batches (JobBatches, 300-s-Fenster nach
  letztem Request) als busy — kein Idle-Stop zwischen TTS-Sätzen.
- **Media-Prewarm** beim Chat-Turn, wenn die Antwort per ComfyUI gesprochen
  wird (nur wenn reply_tts_enabled für den Kanal) — Wake während das LLM
  generiert.
- **Auto-Miete-Schalter** (Dashboard-Overview-Knopf +
  `POST /api/v1/settings/auto_rent`): AUS = Policy mietet/startet nichts,
  laufende Boxen stoppen („aus ist aus“, Pinned ausgenommen), Proxy
  antwortet sofort `503 x-router-state: auto_rent_off` (kein Hold),
  Wake-API 409. Live getestet: Der Schalter stoppte auch eine seit 3 h im
  Vast-Image-Pull hängende Media-Box sofort. Persistiert in
  settings-Tabelle, überlebt Restarts.
- **Router-Fixes**: `mark_destroyed` räumt jetzt `active_instance` ab +
  Reconciler heilt die Active-Invariante (Box zerstört + Active stehen
  geblieben → Proxy 502te auf die tote Box statt 503+Wake zu antworten
  — live reproduziert und gefixt); `describe_slot_state` meldet
  warming/stopped/preempted aus DB statt stale Heartbeat.
- **VPS-Deploy fertig** (`deploy/vps-compose.yml` + `deploy/vps/`):
  NetBird-Container + Router + STT + Praxis in EINEM Netzwerk-Namespace
  (kein Port-Publishing, öffentliche IP exponiert nichts, ACLs regeln),
  Praxis-Image `vayayo/praxis` (Binary+Node+POML-Bundle), Master-Key-
  Zustellung per Root-Entrypoint→tmpfs→lesen+löschen (nie in env/argv,
  Agent-Shell kann Store nie lesen, shared VM-mode genügt), Erststart
  legt leeren verschlüsselten Store an, Secrets via Dashboard.
  Live auf dem Heim-Server getestet (Peer `praxis-vps` 100.105.184.160,
  loopback-Kette Praxis→Router✓, Overlay von wasser✓) — gleiche Compose
  zieht später 1:1 auf den VPS um. Details: `deploy/vps/README.md`.
- Budget zurück auf 2.0/2.4 (testweise 4.6/5.0 für den E2E-Tag).

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
- **Media-TTS-E2E auf warmer Box**: Die frisch gemietete Media-Box hing
  ~3 h im Vast-Image-Pull (16,8 GB) und wurde vom Auto-Miete-Test-Stop
  beendet — der sprechende Pfad (TTS → X-Router-Job-Id → busy → WAV)
  braucht einmal eine warme Box zum Bestätigen (Spike bewies die Kette
  grundsätzlich; Comfy-503-Handling ist live gesehen worden). Budget-Tag
  war am 20.09. ausgeschöpft (3,88 $) — nächster Tag/Cap-Erhöhung.
- ~~Browser-Test Dashboard-Klick-Fluss~~ ✅ 20.09. abends II: browser-äquivalent
  durchgeführt — 3 Produktiv-Bugs gefunden+gefixt (s. o.), Formen/SSE/Upload/
  Blacklist jetzt live verifiziert. Bleibt: echter Klick im GUI-Browser als
  Augen-Kontrolle (optional).
- ~~Dashboard-Kosmetik `active_instance`~~ ✅ zeigt jetzt wärmende Box + Swap-Hinweis.
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
- `policy` — reine Entscheidungen (`decide(snapshot, cfg)`), deterministisch getestet (keine gemessene Coverage-Angabe)
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

## Free-API-Router & Jumper (optional)

**Settings → Free-API-Router & Jumper**: Anbieter mit eigenen Keys, kostenlosen Modell-IDs, Limits und gewünschter Reihenfolge eintragen. `Use free router when all offline` aktiviert den LLM-Textchat-Fallback, sobald im angefragten Slot keine gesunde GPU verfügbar ist. Gesunde GPUs behalten Vorrang.

- **Priorität:** ersten verfügbaren Anbieter bis zur Reservegrenze nutzen.
- **Jumper:** bei jedem Request zum nächsten verfügbaren Anbieter, auch bei parallelen Requests.
- Standardmäßig **5 Requests Reserve**, persistente SQLite-Zähler, Minuten-/Stunden-/Tages-/Monats- und Tokenlimits, Provider-Cooldowns/`Retry-After`.
- 40 Verzeichnis-Einträge als Vorlagen, Free-Tiers vs. Testguthaben/Adapter klar markiert; keine automatische Aktivierung. Alle Kontingente erschöpft → 503 mit Wartezeit, nicht unbegrenzte Nutzung.

Standardmäßig aus. Externe Anbieter erhalten Prompts/Tool-Inhalte; kostenlose Modelle und Accountlimits selbst bestätigen, bezahlte Nutzung beim Anbieter sperren. **[Einrichtung, Konfiguration und Grenzen](docs/free-api-router.md)**.

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
Dateien über **32 MiB** werden vor dem Einlesen abgewiesen; großer
Chunk-/Resume-Push ist noch offen (HTTP-Pull ist auf Vast kein verlässlicher
Fallerückweg). Jede neue Agent-Verbindung braucht ein aktuelles Manifest,
auch wenn es leer ist. Fehlende `required`-Assets → Agent meldet `Degraded`
(assets_missing) → kein Flip ohne Referenzstimme. Bounded/atomarer Upload im
Dashboard, derzeit nur flache ASCII-Dateinamen. Live-Updates mit Service-
Neustart sind noch nicht durch eine vollständige Idle-Rollout-Pipeline geschützt;
siehe [Bauplan-Abgleich](docs/plan-audit.md).