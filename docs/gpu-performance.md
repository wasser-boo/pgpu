# GPU-/Host-Katalog, Messungen und optionale Auswahl

## Sicherheitsgrenzen und Stand

Die Funktionen sind in **Router und Agent-Quellcode** implementiert. Der bereits vorher gestartete Docker-Build **Router 0.24 enthält diese nachträglichen Änderungen nicht**. Agent-Benchmarks benötigen den neuen Agent **0.2.0** im GPU-Image. Alte Agents können passiv beobachtet werden, verstehen aber den neuen Benchmark-Befehl nicht. Ein Router-Update ersetzt keinen Agent in einem bereits gebauten Fork-Image.

Keine Funktion führt beim Konfigurationslesen Benchmarks aus. Neue Mess-/Benchmark-/Ranking-Funktionen sind standardmäßig aus. Vor einem Upgrade SQLite sichern. Diese Funktionen ersetzen nicht die Freigabekriterien in [production-readiness.md](production-readiness.md).

## Whitelist und Blacklist

```toml
[vast]
activate_blacklist = true
activate_whitelist = false
blacklist_after_fails = 2
```

| Blacklist | Whitelist | Zulässig |
|---|---|---|
| aus | aus | Alle Hosts, die Hardware-/Preis-/Budgetprüfungen bestehen |
| an | aus | Nicht gesperrte Hosts |
| aus | an | Nur ausdrücklich freigegebene Hosts |
| an | an | Freigegeben **und** nicht gesperrt |

Ausschalten löscht keine Einträge, Fails oder Messwerte. `blacklist_after_fails=0` deaktiviert nur die **automatische Eintragung**, nicht die Durchsetzung vorhandener Einträge. `activate_blacklist=false` deaktiviert beides; Fails werden weiter gezählt. Whitelisting hebt eine Blacklist nicht implizit auf und erfolgt **niemals automatisch aufgrund eines Scores**. Eine aktivierte leere Whitelist blockiert Mieten/Starts absichtlich.

Pflege unter **Offers → Host freigeben / Blacklisten** oder per Bearer-API:

```sh
curl -fsS -X POST -H "Authorization: Bearer $ROUTER_TOKEN" "$ROUTER_URL/api/v1/machines/12345/whitelist"
```

Aktionen: `whitelist`, `unwhitelist`, `blacklist`, `unblacklist`. Schalter lassen sich im bestehenden TOML-Editor unter Settings speichern/hot-reloaden. Sie gelten bei neuen Mieten, Wiederanlauf und Gebotsänderungen; laufende Verträge werden dadurch nicht eigenmächtig zerstört.

### Zusätzliche harte Slot-Anforderungen

```toml
[slots.requirements]
gpu_names = ["RTX 5090", "RTX 5070 Ti"]
# machine_ids = [12345] # zusätzliche Slot-Allowlist, unabhängig von den Trust-Schaltern
min_gpu_ram_gb = 16.0
min_cpu_ram_gb = 64.0
min_disk_bw = 1500.0
min_inet_down = 500.0
min_cuda = 12.9
min_reliability = 0.98
num_gpus = 1
gpu_price_ceiling_usd_h = { "RTX 5090" = 0.30, "RTX 5070 Ti" = 0.22 }
max_storage_usd_h = 0.04
max_download_usd_gb = 0.005
max_effective_usd_h = 0.34
```

GPU-Namen werden exakt verglichen (Groß-/Kleinschreibung, Leerzeichen/Unterstriche, optionales `NVIDIA`-Präfix normalisiert; `RTX 5090 D` ist nicht `RTX 5090`). Fehlende Hardwarewerte erfüllen ein positives Minimum nicht. `min_cuda` ist der Provider-CUDA-Wert, nicht die GPU-Compute-Capability. VRAM/RAM sind GiB aus Vast-MiB; Disk-Menge folgt Vast-GB, Disk-Bandbreite MB/s, Netzwerk Mbit/s.

Die Auswahl und manuelles Mieten verwenden dieselbe lokale Prüfung, nicht nur den Vast-Suchstring. Preis- und Budgetlimits gelten vor jeder Score-Bevorzugung; `force=true` ist bei Create kein Umgehungsweg mehr. Das Budget berücksichtigt den geschätzten Model-Download zusätzlich zur 30-Minuten-Projektion **aller** Verträge. Diese Kosten sind Schätzungen, keine Garantie einer Vast-Rechnung; Tarif-/Warmup-/Traffic-Abweichungen bleiben möglich.

Neue Mieten speichern ihre Hardware-Fakten für spätere Wiederanläufe. Historische Instanzen ohne diese Fakten dürfen neue strikte Hardware-Minima nicht stillschweigend bestehen. Dann entweder Anforderungen bewusst prüfen/anpassen oder bei der nächsten regulären Miete neue Fakten erfassen – kein automatisches Destroy/Recreate.

## Passive Daten echter Nutzung

Pro Slot:

```toml
[slots.performance]
enabled = true
collect_usage = true
profile = "qwen35b-q4_ctx8192_runtime-v1"
retention_days = 90
max_samples = 50000
```

Erfasst werden abgeschlossene und abgebrochene HTTP-Inferenz-Requests über den Router für `/v1/chat/completions`, `/v1/completions` und `/completion`:

- gesamte beobachtete Request-Dauer (inklusive Upload, Queue, Netzwerk und Client-Backpressure), Erfolg/Abbruch;
- Prompt-/Output-Tokenzahlen und Decode-/Prefill-Token/s, **wenn der Server sie tatsächlich liefert**;
- bei SSE Zeit bis zum ersten Content-/Reasoning-/Tool-Delta, nicht bis zum ersten HTTP-Byte;
- frische Agent-Momentaufnahme von GPU-Auslastung/VRAM (bestehender Agent: GPU 0, kein Peak und keine exakte request-exklusive Zuordnung);
- Host-/Instanz-ID, GPU-Modell, Profil, Workload-/Allocation-Key, tatsächlich verwendetes Image und geschätzte Vertragsrate.

Keine Prompttexte, Antworten, Toolargumente, Embedding-Vektoren oder Tokens/Secrets werden gespeichert. Der Beobachter verändert weder Requests noch SSE und zählt **keine Chunks als Tokens**. Ein nicht-streamender Request hat keine messbare TTFT; fehlende Metriken bleiben `null`/„—“. Die SSE-/JSON-Auswertung ist speicherbegrenzt (64 KiB pro Zeile / 256 KiB JSON). Ohne abschließendes SSE-Signal wird ein Stream nicht als vollständig gewertet.

Passive Daten dienen deiner Auswertung. Sie gehen **nicht** in das Angebotsranking ein: kurze Chats und lange Prompts sind kein kontrollierter Vergleich. Direkt am GPU-Service vorbeigeschickte Requests und andere Protokolle (ComfyUI/TTS/WS) werden nicht als LLM-Token-Benchmark ausgegeben.

## Konfigurierbare Benchmarks

```toml
[slots.performance.benchmark]
enabled = true
auto_when_idle = false
idle_s = 120
min_interval_s = 86400

[slots.performance.benchmark.spec]
port = 11434
model = "DEIN-EXAKTER-ALIAS-AUS-v1-models"
requests = 3
prompt_repetitions = 64
max_tokens = 128
timeout_s = 120
disk_read_mb = 0
```

- `enabled=true` erlaubt den **manuellen** Instanz-Button bzw. `POST /api/v1/instances/ID/benchmark` (202 = gestartet, nicht bereits erfolgreich).
- `auto_when_idle=true` erlaubt zusätzlich regelmäßige Versuche auf healthy/idle Instanzen, begrenzt durch `min_interval_s`. Kein Benchmark-Trigger nur durch das Öffnen des Dashboards.
- Voraussetzungen: frischer gesunder Agent, keine laufenden Requests/Batches, kein Agent-Busy, keine ausstehende Stop/Destroy-Operation.
- **Der gesamte Slot wird während des Tests exklusiv reserviert. Neue Requests bekommen 503/Retry-After.** Bestehende Requests werden niemals für einen Benchmark abgebrochen. Automatik deshalb nur für geeignete Wartungs-/Idle-Zeiten einschalten. Externer Direktzugriff auf die GPU kann den Versuch trotzdem stören.
- Versuche werden vor dem Senden persistent verbucht, auch ein Fehler hat Cooldown. Neustarts erzeugen keinen Benchmark-Sturm. Fehler sind sichtbar, führen aber nicht blind zur GPU-Blacklist.
- Der Start prüft Budget-/Ratenlimits erneut. Hard-Budgets und bewusste Stop/Destroy-Aktionen haben weiterhin Vorrang. Automatische Benchmarks überspringen gepinnte/gelockte Slots.
- Agent muss `PRAXIS_AGENT_ALLOW_BENCHMARK=1` gesetzt haben (bei **neuen** Mieten setzt der Router es nur bei freigeschalteten Benchmarks). Bestehende Agent-Umgebungen werden nicht heimlich geändert.
- Agent benötigt Python 3. Er verwendet ausschließlich Loopback, keine Proxies/Redirects/Downloads/Restarts. Heartbeats laufen während des Tests weiter; bei Session-Abbruch/Timeout wird der Benchmark-Client beendet. Die Inferenzserver-GPU-Arbeit muss der Server nach Client-Disconnect selbst abbrechen.
- Grenzen: 1–10 Requests, 1–256 Prompt-Wiederholungen, 16–512 Output-Tokens, 10–600 Sekunden Gesamtdeadline, maximal 256 MiB Disk-Test. Nur synthetische Prompts, feste Parameter, kein Benutzerinhalt. Prefix-Cache wird angefordert deaktiviert; Betriebssystem-/Modell-Caches werden **nicht** geleert. Das ist kein Kaltstart-Test und kein Qualitäts-/Korrektheits-Acceptance-Test.

### Disk und GPU während der Inferenz

Der Benchmark liest vor/nach der Inferenz die zugänglichen `/proc/PID/io`-Zähler der sichtbaren `llama-server`-Prozesse. Daraus kommen **physisch gelesene/geschriebene Bytes während der Inferenz**. Prozesswechsel, Berechtigungsfehler oder fehlende Prozesse ergeben unbekannte Werte, nicht angeblich null. Andere Arbeit im gleichen Prozess kann die Werte beeinflussen.

Optional `disk_read_mb=128`: vorhandene `.gguf` direkt unter `PRAXIS_AGENT_MODEL_DIR` read-only mit `O_DIRECT` lesen. Kein Download, Schreiben, Cache-Leeren oder Symlink-Folgen. Wenn das Dateisystem O_DIRECT nicht unterstützt, bleibt der Wert unbekannt – es wird **kein RAM-/Pagecache-Test als SSD-Geschwindigkeit verkauft**. Einheit des Ergebnisses: dezimale MB/s; konfiguriertes Datenvolumen: MiB.

GPU-/VRAM-Momentaufnahme am Ende jeder Inferenz über `nvidia-smi` (mehrere GPUs: gemittelte Util, aufsummierter VRAM). Mehr Disk-I/O ist **kein positiver Score-Faktor**: es kann Offloading/zu wenig RAM anzeigen. Modellladezeit, Temperatur-/Power-Verlauf, echte Peak-VRAM-Messung, TTS-RTF und ComfyUI-spezifische Benchmarks sind noch nicht enthalten.

## Score und Bevorzugung

```toml
[slots.performance.score]
enabled = true
preference_weight = 0.25
min_samples = 3
max_age_days = 30
decode_target_tps = 40.0
prefill_target_tps = 500.0
ttft_target_ms = 1000.0
decode_weight = 0.7
prefill_weight = 0.2
ttft_weight = 0.1
```

**Kein universeller „RTX 5090 ist 92/100“-Wert.** Bewertung gilt für den konkreten Host, GPU-Anzahl/VRAM/CPU-RAM/CPU-Kerne/zugewiesene Disk und Workload. Der Workload-Key enthält Slot/Rolle, Profil, tatsächliches Image, Benchmark-Spezifikation und den bei der Miete gespeicherten deterministischen Image-/Env-Fingerabdruck (nicht Env-Klartext). Ein Config-Reload kann eine alte Instanz dadurch nicht als neue Runtime umetikettieren. Verwende immutable Image-Digests und ändere das Profil bei anderem Modell/Quant/KV-Cache/Kontext/Runtime. Der Router kann einen heimlich neu belegten `latest`-Tag nicht erkennen.

Nur erfolgreiche **aktuelle, modellgleiche Benchmark-Messungen** mit bekanntem Rental-Allocation-Key zählen. Unbekannte/neue Hosts, alte Profile, andere GPU-Ausstattung, fehlende aktive Score-Metriken und zu wenige Samples erhalten **keinen** erfundenen Score. Drei Requests sind keine statistische Produktionsgarantie; `min_samples` passend erhöhen. Bestehende Instanzen ohne gespeicherte Rental-Fakten können Daten liefern, werden aber nicht spekulativ für eine unbekannte zukünftige Allocation empfohlen.

Pro Messung:

- Durchsatz-Komponente = `100 × Ist / (Ist + Ziel)`
- Latenz-Komponente = `100 × Ziel / (Ist + Ziel)`
- gewichtetes Mittel der aktiv gewichteten Komponenten, danach Median der vergleichbaren Messungen.

Ziel erreicht = 50 Punkte, doppelter Durchsatz = 66,7; keine unkalibrierte absolute Qualitätsbehauptung. Fehlende Metriken mit positivem Gewicht verhindern den Score; für nicht lieferbare Prefill-Timings z. B. bewusst `prefill_weight=0` einstellen.

Bei aktiviertem Ranking: `Kostenrang × (1 - preference_weight × (Score - 50)/50)`. Default ±25 %, maximal konfigurierbar ±50 %. Unbekannt bleibt neutral; neue GPUs werden nicht pauschal ausgesperrt. Auswahl immer **nach** Trust-/Hardware-/Preisfiltern, Ausführung weiter durch globale Budget-/Kapazitätsprüfungen. Kein zusätzlicher Performance-Hot-Swap während laufender Arbeit.

## Anzeigen, aufbewahren und exportieren

- Dashboard **GPU-Messdaten** (`/performance`): Lebenszeit-Requestmittelwerte plus aktueller Benchmark-Score; Offers zeigt Eignung/Ablehnungsgründe und Score. Profil-Hash unterscheidet Konfigurationen; Allocation/Image im Tooltip.
- `GET /api/v1/performance/summary`: permanenter Katalog, je Host/GPU/Profil/Allocation/Modell/Quelle getrennt, mit Zählern und Mittelwerten je tatsächlich vorhandener Metrik.
- `GET /api/v1/performance?slot=1&machine_id=12345&limit=1000`: Rohdaten, neueste zuerst. Nächste Seite mit `before=<letzte id>`; 1–1000 Zeilen pro Abruf.
- Rohdaten werden pro Slot auf Alter und Anzahl begrenzt. **Lebenszeit-Aggregate bleiben bestehen**, auch nach Destroy einer GPU und Router-Neustart. Abschalten der Sammlung löscht nichts.
- Das sind Metadaten und Infrastrukturinformationen: nur über die authentifizierte Admin-API, SQLite/Backups privat halten.

```sh
curl -fsS -H "Authorization: Bearer $ROUTER_TOKEN" "$ROUTER_URL/api/v1/performance/summary" -o gpu-katalog.json
```

Tests sind offline mit temporärem SQLite, Mock-Agent-Kanälen und synthetischem Loopback-SSE-Server; sie starten keine echte GPU-Inferenz. Echte Server-Timing-Kompatibilität und Messqualität müssen vor produktiver Score-Bevorzugung mit bewusst freigegebenen kleinen Pilotversuchen geprüft werden.
