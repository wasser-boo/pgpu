# Bauplan-Abgleich und Veröffentlichungsstatus

**Ergebnis: Bauplan v2 einschließlich Nice-to-haves ist NICHT vollständig umgesetzt.**
Die frühere Phasenübersicht belegt einzelne historische Spikes, nicht jeden Punkt
in `/home/wasser/gpu-router-plan.md`. Der Nutzer hat inzwischen die Veröffentlichung
des **aktuellen Teilstands** aller betroffenen Repositories/Images ausdrücklich
beauftragt. Release-Ziel: Router 0.25, Agent 0.2, Decision 0.2/GS-Alias 0.5,
ComfyUI 0.3, Praxis 0.10; historischer GS-Fork nur unter `legacy-*`.
Commit-/Registry-Nachweise werden separat erfasst. Keine Deployment-Neustarts,
GPU-Mieten oder Hardware-Benchmarks sind damit freigegeben oder ausgeführt.

Ausgangs-Commits: pgpu `7057bd5`, Agent `e490ca6`, ComfyUI `497d13a`,
Decision `e92dd8a`, historischer Llama-Fork `efe50ad`. Vorhandene Änderungen
wurden beibehalten. „Lokal“ bedeutet Quellcode/Offline-Tests, nicht Hardware-Abnahme.

## Neu korrigiert

- **Idle-only Flip:** Policy berücksichtigt Streams und Busy beider Backer.
  Ausführung prüft den aktuellen Active, Pin/Lock, Readiness, Busy, Batch,
  Busy-Grace und offene Provider-Aufträge erneut. Umschalten und Cache-Publikation
  laufen unter derselben Traffic-Sperre wie Request-/Benchmark-Zulassung.
  Ein vor dem Flip ausgewähltes, inzwischen veraltetes Ziel erhält vor jeder
  Upstream-Übertragung 503. Explizites `/inst/…` bleibt expliziter Debug-Zugriff.
- **Kein Pool-Bypass:** Bei `pool.warm=1` geht Traffic nur an den freigegebenen
  Active, nicht bereits an einen ungeflippten Ersatz. Bei mehreren warmen
  Backern bleibt Pool-Routing erhalten. Ein abgelehnter Flip darf den bisherigen
  Active nicht per nachfolgendem SwapOut entsorgen. Stop des alten Backers
  schaltet den bereits umgeschalteten Slot nicht mehr auf ungewünscht.
- **Pin-Rennen:** API-/Schedule-Pins teilen die Management-Sperre mit Lifecycle-
  Operationen. Automatische Start/Stop/Destroy/Bid-Aktionen prüfen neue Pins/Locks
  vor Provider-I/O erneut. Explizite Betreiberaktionen bleiben möglich. Ein Pin
  storniert keine schon übermittelte/dauerhaft vorgemerkte Provider-Operation.
- **Lifecycle:** Eigenes persistentes `boot_started_at` für Restart-Warmup.
  Manueller Modus wird von der normalen Policy-Automatik ausgenommen; Tages-
  und Monats-Hard-Cap gelten weiterhin, bestehende Pin-Ausnahmen bleiben erhalten.
  Gewöhnlicher Idle-Stop beendet kein laufendes Zeitfenster vorzeitig.
- **Zeitberechnung:** Nachtfenster gehören zum Starttag, Prewarm kann über
  Mitternacht/Wochenwechsel reichen, Fensterende funktioniert auch an Werktagen.
  Sleep-Dauer verwendet verstrichene Sekunden statt Uhrzeit-Modulo; tägliches
  Resume wird aus Stopzeit und Router-Zeitzone einschließlich DST berechnet.
  Schedule-/Sleep-Verträge werden nicht vor ihrem Resume per Idle-GC zerstört.
- **Asset-Readiness:** Jede Agent-Verbindung benötigt ein aktuelles Manifest,
  auch ein ausdrücklich leeres. Fehlende Probe-Konfiguration ist nicht healthy.
  Lokale Dateien werden wirklich gehasht; gleiche Größe plus alter erwarteter
  Hash genügt nicht. Router prüft den finalen Required-Status nach Lieferung.
- **Asset-I/O:** WS-Push ist vor dem Einlesen auf **32 MiB** begrenzt; darüber
  bleibt Chunk-/Resume-Transport offen. Uploads sind begrenzt, atomar, ohne
  absolute/verschachtelte/traversierende Dateinamen; Scope-Symlinks dürfen die
  Asset-Wurzel nicht verlassen. Temporäre Uploads erscheinen nicht im Manifest.
  Agent-Push prüft Größe/Hash und setzt Dateirechte vor der Veröffentlichung.
  Der kaputte doppelte `http://`-Präfix im Pull-Fallback wurde korrigiert.
- **ComfyUI-Nice-to-haves:** Qwen-Download in gesperrtes, revisionsgebundenes
  Staging; Wiederholung kann dort fortsetzen, erst vollständige Dateien werden
  atomar veröffentlicht. Fremde/alte unvollständige Zieldaten werden nicht
  gelöscht. Torch-Inductor-Cache liegt unter `/workspace/inductor-cache`.
  `PRAXIS_QWEN_TTS_IDLE_SECONDS=0` bedeutet weiterhin **One-shot**, nicht „ewig warm“.
- **Build-Eingaben:** Router und eingebetteter/Standalone-Agent bauen mit
  `Cargo.lock` und `--locked`; das Agent-Transport-Dockerfile unterscheidet
  amd64/arm64 statt immer ein amd64-Binary einzupacken. Docker-Builds sind noch
  nicht erneut ausgeführt; das ist keine behauptete Image-Verifikation.

## Abgleich aller Planbereiche

| Plan | Stand im Quellcode und verbleibende Lücken |
|---|---|
| §0–1 Slots/Architektur | Lokal: Rollen, stabile Slots, Backer, private Ports, Rust-Komponenten. Agent ist separates Repo. **Bewusste Korrektur zum frühen Plan:** Router wählt Agent an und pusht über WS; HTTP-Pull auf Vast ist kein verlässlicher Transport. |
| §2 Datenmodell | Lokal: SQLite, Migrationen, Cursor-/Instanzmeter, historische Readiness, Events, Offer-/Mietfakten und Performance-Historie. Cursor-Ledger ersetzt das skizzierte Run-Tabellenschema. Offen: crashsicheres Create-/Adoption-Journal; keine atomare Provider+SQLite-Transaktion behaupten. |
| §3 Reconciler | Teilweise: Inventar, Agent-Verbindungen, Comfy-Queue, Streaming-Leases, Batch-/Grace, Preemption, Meter und Credit-Abgleich. Offen/unvollständig: vollständige agentlose Health-Fallback-Kette, robuste Download-/Netzstatistik und Kosten-Zuordnung, wirkliche LLM-Metrics-Probe, Warm/Kalt-Signal des Qwen-Workers. |
| §4.1 Budget | Lokal: Tages-/Monatsgrenzen, 30-Minuten-/Downloadkosten-Zulassung, erneute Prüfung vor Provider-I/O, Soft-Idle-Stop. **Offen:** dauerhafter Hard-Cap-Drain mit Frist statt sofortigem Stop; Projektion vollständiger Lifecycle-Fenster. Pin-Ausnahme widerspricht dem wörtlichen „alles stoppen“ und muss ausdrücklich geklärt werden. |
| §4.2 Gebote | Teilweise: Margin, Ceiling, Busy-Verteidigung, Budgetkontrolle. Offen: historischer Min-Bid-Trend und opt-in `X-Router-Priority: critical` → On-Demand pro Job. On-Demand ist nicht gegen Provider-/Hostfehler „unkillbar“. |
| §4.3 Swap | Idle-Flip und Cache-/Request-Rennen lokal korrigiert. Vorwärmen, Trigger, Hysterese, Kostenprüfung vorhanden. Offen: vollständige dauerhafte Drain-Pipeline mit Agent-Drain, Maximalfrist, Retry und Erhalt von Stop/Destroy-Absicht; Integration/Fault-Injection für alle Übergänge. |
| §4.4 Wake/Idle | Lokal: Stop/Destroy-Regeln, kaltes 503/Retry-After, Hold, Warm-Hours, Start vorhandener Disk. Offen: klassifizierte und persistente Resume-Backoffs/Fresh-Fallback; unklarer Start darf nicht automatisch Daten zerstören. |
| §4.5 Modi/Lifecycle | Teilweise: Modi bei Miete, Lifecycle-Regeln, korrigierte Zeitberechnung. Offen: Moduswechsel als echter Ersatzvertrag statt 409, Manual als Automatik-Eigenschaft unabhängig vom Provider-Vertrag, vollständige Fensterkosten-Prognose/On-Demand-Warnung und Anzeige belegter Host-GPUs. |
| §4.6 Angebote | Lokal: Hardware-/Modell-/Preisgrenzen, Kostenranking, unabhängige Black-/Whitelist, optional vergleichbare Benchmark-Scores. Offen: automatische rollen-/profilabhängige Qualitäts-Abnahme vor Erstnutzung; keine globale GPU-Note aus Utilisierung ableiten. |
| §5 Proxy | Lokal: Pfade/Ports, SSE/WS, Origin-Strip, Header, 503/Hold, Streaming-Leases. **Leases sind pro Slot, nicht pro Instanz.** Offen: per-Backer-Leases für präzisen Pool-Drain, vollständige Rollen-Pool-Aliase, natives Dashboard-TLS; Browser/UI-Abnahme aller Pfade. Service-Ports bleiben ACL-geschützt, nicht Admin-Bearer-geschützt. |
| §6 Agent/Templates | Agent-Einbau, Supervisor, NetBird-Hostname, Kommandos/Logs/Terminal vorhanden. Forks beziehen noch alte Router-Tags. Offen: tatsächliche Download-ETA/-Phasen, ephemeral Enrollment mit bewiesenem Stop/Resume, Image-Größen-/Pullzeit-Messung und end-to-end Secret-/ACL-Review. |
| §7 Dashboard | Overview, Angebote, Instanzaktionen, Settings, Logs/Terminal, Performance vorhanden. Offen: vollständiger Lifecycle-Mietdialog mit Prognose, Timeline/nächste/letzte Ausführung/Pause/Run-now, STT-Modellverwaltung/RAM/Latenz, Bid-Trend, Pending-Restart-Anzeige und vollständige Webhook-Abdeckung. |
| §8 API | State/Budget/Offers, Instanz-/Slotaktionen, Node-WS und Performance-Endpunkte vorhanden. **Nicht vollständig:** Schedules-CRUD/PATCH/Pause/Run-now, sichere Mode-Swap-Operation, sämtliche Rollen-Pool-Aliase/Slot-Aktionen gemäß Vertragsliste. Eine TOML-Editor-Seite ersetzt diese APIs nicht. |
| §9 Deploy | Compose/NetBird-VPS-Variante und non-root Router vorhanden; Build-Abhängigkeiten unten. Offen: gepinnte Release-Artefakte, erneute Builds/Prüfung, Restore-/Rollback-Drill und Deployment-Abnahme. |
| §10 Phasen | Historische Spikes vorhanden. Neue Änderungen nur offline getestet. Keine erneuten Live-Outbid-/Chaos-/Browser-/VPS-Tests; diese benötigen ausdrücklich genehmigte Infrastruktur-/Kostenaktionen. |
| §11 Risiken | Downloadkosten, knappe passende Hosts, Preemption, Billing-Drift und Restart-Risiko gelten weiterhin. Score-Historie/Filter entschärfen sie, beseitigen sie nicht. |
| §12.1 beide Templates | Agent, Hostname, private Listener vorhanden. **Offen:** ephemeral Peers mit Resume, vollständige autonome Assets inkl. großer Caches, echte Traffic-Buchung, optimierte/gemessene Layer-Größe und Download-tauglicher Docker-Healthcheck. `NB_NETSTACK_SKIP_PROXY=true` ist noch gesetzt; nicht blind entfernen und funktionierenden Dial durch toten Pull ersetzen. |
| §12.2 Llama | Decision lokal korrigiert: aktueller GS/Codacus/Decision-Vertrag, per-Binary-Libraries, Cache-aus-Readiness, Profil-Timeout/Umgebungsbindung/Reihenfolge, read-only `/props`/`/metrics`, RAM/VRAM-/Kontext-Dokumentation. **Offen:** Prepare/Serve, sichere Download-Retries/Resume/Parallelisierung/Mirrors, Erststart-Qualitätsabnahme, routerseitiges Metrics-Busy, CPU-Embedding-Sidecar, ETA×Preis-Abbruch. |
| §12.3 ComfyUI | Staging/atomare Qwen-Veröffentlichung und persistenter Compile-Cache lokal ergänzt. **Offen:** Vosk tatsächlich aus GPU-Image entfernen, selektive/parallelisierte Provisionierung, Qwen-Warmstatus, permanenter Warmmodus/Idle-Abstimmung, TLS-Mikro und Nachweis fresh→healthy <10 min. Fremde Staging-Verzeichnisse werden nicht pauschal gelöscht. |
| §12.4 Praxis | `src/gpu_router.rs` enthält State/Wake/Wait/Job-ID und Session-Helfer. **Keine vollständige Abnahme:** alle UI/Discord-Hooks, Badge, Critical-Weitergabe, genau-einmal Stream-Retry ohne Tool-Replay und konfigurierbare LLM/TTS/STT-Fallback-Kette müssen durch Integrationstests abgesichert/ergänzt werden. |
| §13 Storage | Lokale Disk/Stop-Präferenz, feste Retention und Kostenfilter vorhanden. **Offen:** dynamischer Keep/Rebuild-Breakeven, Volume-Inventar/Attach/Affinität, versionierte private Cache-Synchronisation/Backup-Workflow, optionaler R2-/Range-Mirror. Kein FUSE/WAN-mmap als vermeintliche Transferersparnis. Externe Storage-Provisionierung benötigt Ziel/Zugang/Budget. |
| §14 Assets/Node | Kleine role-scoped WS-Assets, Hashprüfung, Token-Zuordnung, Logs/Terminal vorhanden; aktuelles Manifest-Gate korrigiert. **Offen:** rekursive/Slot-Overrides, großer fortsetzbarer Chunk-Push, durable Retry-/Sync-Generationen, idle-freigegebene Service-Restarts/Pending-UI, strukturierte Download-/Acceptance-Reports und Ereignisse. Bisherige Asset-Kommandos können noch unmittelbar Services restarten: keine sichere allgemeine Live-Asset-Rollout-Pipeline. Optionales Shred ist auf Provider-SSDs keine garantierte Löschung. |
| §15 STT lokal | Router-Sidecar und Local-/Media-Routing vorhanden. **Offen:** echtes Lazy-Load/Idle-Unload, Load/Unload-API, RAM-/Latenzverwaltung und Ressourcen-/Mehrsprachigkeits-Abnahme. GPU-Comfy-Image enthält weiterhin Vosk. |

Die Zusatzfunktionen Performance-Katalog, Benchmarks und Trust-Schalter sind in
[gpu-performance.md](gpu-performance.md) dokumentiert. Auch dort bleiben reale
Messqualität, TTS-/Comfy-Benchmarks, Peak-VRAM und weitere spezialisierte Metriken offen.

## Betroffene Images und Reihenfolge

Der Nutzer hat zusätzlich **Commit und Push jedes geänderten Repositorys nach
Abschluss** autorisiert. Vor Veröffentlichung: alle relevanten neuen Dateien und
Lockfiles prüfen/einschließen, bestehende Arbeit erhalten, Branch/Remote prüfen,
keine Secrets/Produktionsdaten aufnehmen; normal committen/pushen, kein Force-Push.
Erst danach Images in Abhängigkeitsreihenfolge veröffentlichen. Die erneute
Freigabe verlangt ausdrücklich den aktuellen Teilstand; sie bestätigt nicht die
Vollständigkeit des Plans. Belege gehören in den jeweiligen Release-Bericht.

Die Suche nach `gpu-agent`/`PGPU_IMAGE` in den Dockerfiles ergibt:

1. **`vayayo/praxis-gpu-router`** — `pgpu/deploy/Dockerfile`, benannter Build-Kontext
   `agent=../praxis-gpu-agent`; enthält Router **und** `/gpu-agent`. Zuerst bauen,
   neue Versions-Tags verwenden, amd64/arm64 prüfen und Digest festhalten.
2. **`vayayo/praxis-gpu-agent`** — Standalone-Transportartefakt aus dem Agent-Repo,
   ebenfalls aktualisieren, wenn dieser veröffentlichte Artefaktkanal beibehalten
   wird. Scratch enthält keine Python-/GPU-Tools und ist kein vollständiges
   Benchmark-Runtime-Image. Ein Export des bereits gebauten `/gpu-agent` vermeidet
   unterschiedliche Agent-Artefakte zwischen Kanälen.
3. **`vayayo/praxis-llamacpp-decision`** und bestehender Alias
   **`vayayo/praxis-llamap-gs-gpu`** — **beide aus `fork-llama-decision`**.
   `PGPU_IMAGE` ausdrücklich auf den neuen Router-Digest setzen. Der Default ist
   noch `:0.23`. Der alte `fork-llama` darf **nicht** dessen `latest` überschreiben!
4. **`vayayo/praxis-comfyui-gpu`** — `fork-comfyui`, ebenfalls neuer
   `PGPU_IMAGE`-Digest erforderlich; Default noch `:0.2`.
5. **Historischer `fork-llama`** — technisch ebenfalls Agent-Konsument, Default
   `:0.2`; nur unter getrennten Legacy-Tags bauen, falls noch unterstützt.
6. **`vayayo/praxis`** ist durch die zusätzlich beauftragte modellgesteuerte
   Tool-Ausgabe (`_output`, Default Volltext, gespeicherte Antworten) jetzt ebenfalls
   betroffen. **`vayayo/praxis-stt`** bleibt unverändert; keine Lazy-Load-Abnahme.

Für jeden Push: Version und `latest` bewusst zuordnen, Registry-Manifest/Digest/
Architekturen unabhängig prüfen, Agent-Artefakt im Router und in jedem Fork
vergleichen. Ein Router-Push tauscht weder eingebettete noch laufende Agents aus.
**Deployment ist ein separater, noch nicht ausgeführter Schritt.**

## Verifikation und Freigabeblocker

- pgpu: **119 Tests**, `cargo build --workspace --locked`, isolierter Prozess-
  Smoke-Test bestanden. Temporäre SQLite-Dateien, Fake-Provider/Loopback; keine
  echte Cloud-Aktion.
- Agent: **5 Rust + 5 Python** bestanden, locked Build bestanden; keine GPU-Last.
- ComfyUI: **68 Python-Tests** mit Fake-Modellen bestanden. Dafür wurde nur eine
  temporäre Python-Umgebung mit `aiohttp==3.13.3` unter `/tmp` erstellt.
- **Decision: 71 Tests bestanden.** Die sechs alten Fehler wurden gegen den
  aktuellen Drei-Backend-Vertrag korrigiert; Kontextgrößen nicht zurückgedreht.
  Historische Base-Hashes bleiben erhalten, lokale Abweichungen sind ausdrücklich
  verzeichnet. Neue Offline-Tests prüfen zusätzlich tatsächliche Launcher-/Profil-
  und Library-Isolationskorrekturen; kein Hardware-Leistungsnachweis.
- Clippy/rustfmt lokal nicht verfügbar; CI und reale Hardware-Abnahme nicht
  nachgewiesen. `git diff --check` ist kein Ersatz für diese Prüfungen.

Logs: `/tmp/pgpu-plan-tests.log`, `/tmp/pgpu-plan-build.log`,
`/tmp/agent-plan-tests.log`, `/tmp/agent-plan-build.log`,
`/tmp/comfy-plan-tests.log`, `/tmp/decision-plan-baseline-tests.log`.

**Keine Freigabe als „alles aus dem Bauplan umgesetzt“.** Nächste Arbeitspakete:
Create-Recovery und begrenzte Drain-/Resume-Operationen; Schedules-API/UI samt
Fensterbudget; Critical-/Mode-Swap; vollständiger Asset-Rollout; Llama-/Comfy-/STT-
Entkopplung; Storage-Nice-to-haves; Praxis-Vertragstests; anschließend reproduzierbare
Images, Registry-Prüfung und separat genehmigte Hardware-/Betriebsabnahme.
