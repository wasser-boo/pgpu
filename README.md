# pgpu — Praxis GPU Router

Vast.ai-Interruptible-Verwaltung für Praxis: **Slots statt Instanzen.** Slot 1 =
Rolle `llm` (fork-llama), Slot 2 = Rolle `media` (fork-comfyui). Vast-Instanzen
backen einen Slot; beim Hot-Swap flippt der Slot auf die neue Instanz.

```
Praxis ──NetBird──► ROUTER (pgpu)
                      ├─ Proxy :8188 :2700 :11434 :11435 :11436 → aktive Instanz des Slots
                      ├─ Dashboard :8080 (askama+HTMX+SSE, Terminal)
                      ├─ Reconciler (30 s) ─ Policy (Budget/Bid/Swap/Idle, rein + getestet)
                      ├─ vast client · node-agent hub (WS call-home) · assets server
                      └─ SQLite (/data)
                               │ NetBird
                               ▼
                 GPU-Instanz (vast) → supervisord → netbird · services · gpu-agent
```

## Crates
- `common` — Protokoll Router↔Agent, Service-Defs, Events
- `vast` — Vast.ai API-Client (Offers/Instances/Bids/Balance/Logs)
- `policy` — reine Entscheidungs-Funktionen (budget, bid, swap, idle) — 100 % unit-getestet
- `router` — axum: Proxy, API, Dashboard, Reconciler, SQLite, Agent-Hub, Assets

## Agent
Der Node-Agent (WS call-home, Health/Busy/GPU/Progress, Asset-Sync, Exec) ist ein
eigenes Repo: `forgejo.the.grid/Marvin/praxis-gpu-agent`. Er wird beim Router-Image
mitgebaut und liegt dort unter `/gpu-agent` (Forks `COPY --from` ihn sich).

## Deploy
```sh
cp config.example.toml config.toml   # anpassen: bind_ip = NetBird-IP, Keys
docker compose -f deploy/docker-compose.yml up -d
```

Ports (alle an die NetBird-IP gebunden, kein EXPOSE im Image):
- Passthrough: 8188 (ComfyUI), 2700 (Vosk/STT), 11434/11435/11436 (llama API/WebUI/Embed)
- Dashboard: 8080 (`/`, `/offers`, `/schedules`, `/settings`, `/api/v1/...`)

Details: README-Bauplan v2 (Slots, Policy, Hot-Swap, Budget) — siehe Commit-History.