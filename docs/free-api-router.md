# Free-API-Router und Jumper

Optionaler Cloud-Fallback für LLM-Textchat. **Standardmäßig aus**; ohne Aktivierung bleibt das bisherige GPU-Routing erhalten.

## Einrichtung

1. Dashboard → **Settings → Free-API-Router & Jumper**.
2. Anbieter aus dem Katalog hinzufügen (oder eigenen OpenAI-kompatiblen Endpunkt eintragen).
3. Eigenen API-Key oder einen **Env-Variablennamen** hinterlegen, kostenlose Modell-ID wählen, tatsächliche Account-/Modell-Limits eintragen. Bei Docker müssen Env-Variablen **im Router-Container** verfügbar sein; ein Export auf dem Host allein reicht nicht.
4. Anbieter aktivieren und mit ↑/↓ sortieren.
5. **Use free router when all offline** einschalten. Optional **Jumper** einschalten. Speichern; kein Neustart erforderlich.

Neue Anbieter sind immer deaktiviert. Es werden keine Accounts oder Keys automatisch angelegt und beim Speichern keine Testrequests geschickt. Leeres Key-Feld behält den gespeicherten Key (gleiche ID und URL); „Key löschen“ entfernt ihn. Beim Wechsel der ID/URL Key erneut eingeben. `api_key_env` hat Vorrang; fehlt diese Variable oder ist sie leer, wird der Anbieter übersprungen, nicht auf den direkt gespeicherten Key zurückgefallen.

**Datenschutz/Kosten:** Prompts, Systemanweisungen und Tool-Inhalte gehen an externe Betreiber; keine Garantie gleicher Modelle, Qualität, Kontextlänge, Tools oder Datenschutzbedingungen. Nur autorisierte eigene Kontingente nutzen. Einen ausdrücklich kostenlosen Plan/ein kostenloses Modell wählen und kostenpflichtige Abrechnung/Auto-Aufladung beim Anbieter deaktivieren. Der Router kann Geldguthaben, Neuronenbudgets und nicht veröffentlichte Session-/Wochenlimits nicht zuverlässig messen.

## Wann wird geroutet?

- Gesunde Instanzen im **angefragten LLM-Slot** haben Vorrang, auch bei aktiviertem Jumper. Sobald wieder eine GPU routbar ist, gehen neue Requests wieder an sie; laufende Cloud-Streams werden nicht umgehängt.
- Ohne gesunden GPU-Backend greift der Free-Router auf dem `api`-Service für **POST `/v1/chat/completions`** und **GET `/v1/models`**. Das gilt sowohl am Passthrough-Port (z. B. `11434`) als auch unter `/gpu/1/api/v1/...`.
- „All offline“ heißt hier *kein gesundes, veröffentlichtes Ziel dieses Slots*, also auch gestoppt, kalt, preempted oder noch nicht bereit. Eine Media-GPU zählt nicht als LLM-Backend.
- Unterstützt: Textnachrichten, OpenAI-Function-Tools (soweit das gewählte Modell sie unterstützt), JSON und SSE (`stream: true`), `n=1`. `/v1/models` liefert lokal den Alias `free-router`, ohne Anbieterquota zu verbrauchen. Bei Cloud-Chat wird die eingehende Modell-ID durch das konfigurierte Anbietermodell ersetzt.
- Weitergereicht werden nur Chat-Parameter (Nachrichten, Sampling, Function-Tools, Antwortformat, Streaming und Tokenlimits). Lokale llama.cpp-Spezialoptionen und anbieterspezifische Routing-/Fallback-Parameter wie `models`, `route`, `provider` oder kostenpflichtige Plugins werden nicht weitergereicht. So kann ein Client nicht die konfigurierte Modellwahl durch einen fremden Fallback überschreiben.
- Nicht übersetzt: native Ollama-API (`/api/chat`), Responses-/Completions-API, Bilder/Audio, Embeddings, WebUI, STT, ComfyUI, WebSockets. `/inst/...` bleibt expliziter GPU-Direktzugriff. GPU-Health-Endpunkte werden nicht als Cloud-Health ausgegeben.
- Kein impliziter GPU-Wake und kein GPU-Traffic-/Busy-Lease durch Free-Requests. `X-Router-Wait` wartet in diesem Fall nicht auf eine GPU. Unabhängige Zeitpläne/Auto-Rent-Policy bleiben unverändert aktiv.
- Sind alle Anbieter unbrauchbar/limitiert, antwortet der Router mit **503**, `X-Router-State: free_router_exhausted` und `Retry-After`. Kein bezahlter Notfallprovider und kein unbegrenztes Retry-Loop.

Die Datenpfade behalten das bestehende NetBird-Vertrauensmodell. **Nicht ungeschützt ins öffentliche Internet stellen.** Die Konfigurations-/Status-API benötigt Dashboard-Session oder Router-Bearer.

## Priorität oder Jumper

**Priorität (`jumper = false`)**: Erster verfügbarer Anbieter in der Liste. Ist seine nutzbare Quote erschöpft, folgt der nächste. Nach Ablauf der Limits ist ein früherer Anbieter wieder bevorzugt.

**Jumper (`jumper = true`)**: Jeder zugelassene Request nimmt den nächsten verfügbaren Anbieter in der Liste, auch bei parallelen Requests. Deaktivierte Anbieter, fehlende Keys und Cooldowns werden übersprungen. Die Auswahl springt nach dem letzten Anbieter zurück zum Anfang. Der Cursor ist pro Router-Prozess; **Kontingente und Cooldowns sind dagegen persistent**.

Beispiel: 10 Anbieter mit jeweils 30 RPM und 5 Reserve erlauben lokal höchstens **250 Requests je rollierender Minute**, sofern keine kleineren Tages-/Token-/Accountlimits greifen. Round-Robin verteilt diese Last, **erhöht aber nicht die Summe der zulässigen Kontingente**. Er umgeht keine Limits oder Nutzungsbedingungen.

## Rate-Limits

Jeder externe Versuch wird **vor dem Senden atomar in SQLite reserviert**. Auch 429, Timeout, Verbindungsfehler und abgebrochene Streams bleiben gezählt. Neustart, Umsortieren, Umbenennen, Modellwechsel oder Aus-/Einschalten löschen keine Zähler. Der Bucket ist ein SHA-256-Fingerprint aus URL-Origin und Key, nicht aus der frei wählbaren Eintrags-ID; derselbe Key auf demselben Origin teilt sich daher ein Budget. Unterschiedliche Keys desselben Accounts/Projekts können anbieterweit trotzdem gemeinsame Limits haben — solche Kontingente nicht mehrfach einplanen.

| Einstellung | Bedeutung |
|---|---|
| `requests_per_minute` | Pflicht bei aktivem Anbieter, rollierende 60 Sekunden |
| `requests_per_hour` | Optional, rollierende Stunde |
| `requests_per_day` | Optional, rollierende 24 Stunden |
| `requests_per_month` | Optional, konservative rollierende **31 Tage**, kein Kalenderreset |
| `tokens_per_minute`, `tokens_per_day` | Optionale konservative Tokenreservierung |
| `min_interval_ms` | Zusätzlicher Mindestabstand, z. B. 1000 bei 1 Request/Sekunde |
| `safety_buffer_requests` | Global standardmäßig 5, je Anbieter überschreibbar; gilt für jedes Requestfenster |
| `max_output_tokens` | Ausgabe-Obergrenze (Standard 1024); wird auch bei höherem Clientlimit durchgesetzt |

Bei 30 RPM werden höchstens 25 Requests zugelassen. Bei RPM ≤ 5 muss die Reserve **explizit kleiner** eingestellt werden (z. B. 2 RPM, Reserve 1); die Validierung verhindert einen versehentlich komplett unbrauchbaren aktiven Eintrag. Optionale Limits weglassen, statt 0 einzutragen.

Tokenzählung: **serialisierte JSON-Bytes + maximale Ausgabe-Tokens**, keine Speicherung der Inhalte, kein modellspezifischer Tokenizer. Ungenutzte Ausgabetokens werden nicht zurückgebucht. Das ist absichtlich restriktiv und kann große Requests ablehnen, obwohl ein echter Tokenizer noch Spielraum sähe; versteckte Provider-/Reasoning-Kosten sind nicht exakt vorhersehbar. Text-/Tool-Requests werden auf 2 MiB begrenzt. Für einen Request, der grundsätzlich größer als die Tokenquote ist, hilft erst ein kleinerer Request/eine angepasste Konfiguration, nicht bloß Warten.

Zusätzlich ausgewertete Header:

- `Retry-After`: Sekunden oder HTTP-Datum.
- `x-ratelimit-remaining-requests` / `x-ratelimit-reset-requests`.
- `x-ratelimit-remaining-tokens` / `x-ratelimit-reset-tokens`.
- `ratelimit-remaining` / `ratelimit-reset`, alternativ `x-ratelimit-remaining` / `x-ratelimit-reset`.
- Reset-Werte: Sekunden, Unix-Zeit, RFC3339/HTTP-Datum oder zusammengesetzte Dauer wie `1m2.5s`.

Provider-Hinweise können die lokale Quote nur **weiter einschränken**. Antworten außer Reihenfolge füllen kein noch gültiges Kontingent wieder auf. Nicht standardisierte/fehlende Header, andere Clients, anbieterweite Projektquoten und veränderte Pläne verhindern eine universell exakte Restkontingentgarantie. Limits im Account prüfen; der Katalog ist **keine automatische Live-Synchronisation**.

Bei 429/503 gilt `Retry-After` bzw. ein angegebener Reset, ohne Hinweis 60 Sekunden. 401/402/403/404 pausieren den Eintrag 5 Minuten (Key, Guthaben, Modell prüfen); andere 5xx und Transportfehler 30 Sekunden. Dann wird ein anderer zulässiger Anbieter versucht. Andere 4xx werden nicht über alle Anbieter wiederholt. Maximal ein Versuch pro Eintrag/Clientrequest, 15 Sekunden auf Antwortheader je Versuch, insgesamt 60 Sekunden; SSE hat einen 120-Sekunden-Lese-Idle-Timeout. **Nach Beginn einer Antwort niemals Retry oder Modellwechsel.** Ein Provider kann bei einem Timeout bereits gearbeitet haben; eine exakt-einmalige Ausführung über unabhängige APIs ist nicht garantierbar.

## TOML-Beispiel

```toml
[free_router]
use_when_all_offline = true
jumper = true
safety_buffer_requests = 5

[[free_router.providers]]
id = "openrouter"
enabled = true
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"
model = "openrouter/free"
requests_per_minute = 20
requests_per_day = 50
max_output_tokens = 1024

[[free_router.providers]]
id = "zweiter-anbieter"
enabled = false # erst nach Prüfung aktivieren
base_url = "https://api.groq.com/openai/v1"
api_key_env = "GROQ_API_KEY"
model = "" # exakte aktuell kostenlose Modell-ID einsetzen
requests_per_minute = 30 # tatsächliches Modell-/Accountlimit prüfen!
# requests_per_day = ...
# tokens_per_minute = ...
max_output_tokens = 1024
```

Die Reihenfolge der `[[free_router.providers]]` ist die Router-Reihenfolge. Normale HTTPS-Endpunkte verwenden; unverschlüsseltes HTTP ist nur für Loopback-Adapter/Tests erlaubt. Redirects werden nicht verfolgt. Eingehende Router-Tokens, Cookies, Origin oder beliebige Client-Header werden **nicht** an Anbieter weitergereicht; sie erhalten nur den explizit konfigurierten Bearer-Key und JSON-/Accept-Header. Providerfehler werden ohne fremde Fehlertexte/Secrets zurückgegeben. Der authentifizierte TOML-Editor enthält absichtlich die vollständige Konfiguration; die neue Settings-API liefert Keys nur leer zurück.

## Status und Konfigurations-API

`GET /api/v1/free-router` (Session/Bearer) liefert:

- `config`: Schalter, Limits und Reihenfolge, **ohne API-Key-Werte**.
- `statuses`: Key vorhanden, lokal nutzbare Requestquote, Wartezeit/Grund; Tokenkosten werden erst beim konkreten Request geprüft.
- `catalog`: Anbieter-Vorlagen.
- `revision`: Konfigurationsrevision für konfliktfreies Speichern.

`PUT /api/v1/free-router` nimmt `{"config": ..., "revision": "...", "clear_keys": ["id"]}` an. Leere Key-Werte behalten die bisherige Zuordnung. Falsche Schemafelder, ungültige Limits und veraltete Revisionen werden abgelehnt. Speichern erfolgt atomar mit Dateimodus 0600 über denselben Apply-Pfad wie der TOML-Editor. Die Settings-Oberfläche aktualisiert Quoten alle fünf Sekunden, ohne ungespeicherte Felder zu überschreiben.

Cloud-Chat-Antworten enthalten `X-Router-State: free_router`, `X-Free-Router-Provider`, `X-Free-Router-Mode: priority|jumper` und `X-Gpu-Slot`; keine GPU-Instanz-ID.

## Katalog und Überprüfung

Quelle: [Free-LLM Quick Reference](https://github.com/nejib1/Free-LLM#quick-reference--base-urls--api-keys), Repository-Stand vom 23.09.2026. Die Webseite `free-llm.com/free-api` war bei der Implementierung nicht direkt abrufbar (403). 40 Einträge aus der Quick Reference: OpenAI-kompatible URL-Vorlagen (Google, Cohere, Cloudflare und Qwen mit Kompatibilitätspfad), mit **free / credits / trial / adapter** gekennzeichnet. Replicate, Coze und Cerebrium werden nicht als direkt kompatible APIs ausgegeben. Keine Live-Tests mit echten Providerkonten; URLs, Modellberechtigungen, Free-Status und Quoten vor Benutzung bestätigen.

Offline-Tests decken Reserven/Resets, tägliche/monatliche/Tokenlimits, parallele Zulassung, Neustarts, Header/Cooldowns, Jumper/Priorität, GPU-Vorrang, SSE, Credential-Isolation, Redirects und Settings-Validierung ab:

```sh
cargo test --workspace --locked
# Optional mit installiertem Chromium/Chrome, ohne npm oder Cloudzugang:
python3 scripts/free_router_ui_smoke.py
```
