# Produktionsreife und Hardening

## Status

**Gehärteter Single-Operator-Router für ein vertrauenswürdiges VPN, keine
Freigabe als öffentlich erreichbarer oder mandantenfähiger kommerzieller Dienst.**
Ein Testlauf und ein Code-Review begründen weder „99/100“ noch ein SLA.
Die historischen Live-Berichte in der README sind keine automatisierten Abnahmetests.

## In diesem Hardening abgesichert

- Kostenmessung über persistente Zeit-Cursor statt Heartbeat-Alter. Cursor,
  Instanzledger und Tagesledger werden in derselben SQLite-Transaktion gebucht.
  Lokale Tagesgrenzen, Sommerzeit, Restarts und rückwärts springende Uhren sind getestet.
- GPU-Kosten folgen dem zuletzt beobachteten Provider-Zustand, nicht einer nur
  lokal behaupteten erfolgreichen Abschaltung. Storage läuft bis Destroy weiter.
- Stop/Destroy-Absichten stehen **vor** dem Provider-Aufruf dauerhaft in
  `pending_operations`. Netzwerk-/API-Fehler melden keinen Erfolg; der Reconciler
  wiederholt sie. DELETE 404 gilt als bereits erledigt. Auch HTTP 200 mit
  `success:false` wird als Fehler behandelt.
- Providerbestätigung invalidiert beide Routing-Caches. Ein nach Stop weiter
  als running gemeldeter Vertrag wird erneut gestoppt. Dashboard und Bulk-API
  geben Teilfehler weiter statt sie hinter einem Redirect/HTTP 200 zu verstecken.
- Slot-Start wird tatsächlich awaited. Fehlgeschlagene Starts zerstören nicht
  mehr automatisch nach drei Versuchen die vorhandene Disk.
- Mieten, Starts und Gebotserhöhungen prüfen Budget/Rate/Kapazität unter einer
  gemeinsamen Prozess-Sperre erneut. Das verhindert veraltete Snapshot-Entscheidungen
  und paralleles Überschreiten der lokalen Instanzlimits. Gebote müssen positiv,
  endlich und innerhalb der Slot-Ceiling sein.
- Ein DB-Edit kann einen Interruptible-Vertrag nicht in On-Demand verwandeln:
  Änderung des Instanz-Mietmodus antwortet jetzt 409; dafür ist Ersatzmiete nötig.
- HTTP-/SSE-In-flight-Leases gelten bis Stream-Ende, Fehler oder Disconnect.
  Hop-by-hop-Header werden entfernt, Mehrfachheader und abschließende Pfad-Slashes
  bleiben erhalten. HTTP-Clients werden wiederverwendet; Response-Header haben
  ein 120-s-Timeout (keine pauschale Begrenzung der SSE-Laufzeit).
- STT-Uploads werden nicht komplett gepuffert. WS-Relays erhalten Text/Binary-
  Frametypen, nutzen Backpressure und enden beim Abbruch einer Richtung.
- Leeres Admin-Token öffnet keine API mehr; Startup verlangt mindestens 32
  ASCII-Buchstaben/Ziffern bzw. `-`/`_`. Cookie-authentifizierte Anfragen prüfen
  Origin/Host und Cross-Site-Metadaten; Cookies verwenden SameSite=Strict.
- Agent-Kommandorückgaben und Terminal-IDs sind pro Agent getrennt. Terminal-
  Ausgabepuffer sind begrenzt; ein langsamer Client wird getrennt.
- Config-Validierung prüft u. a. Zeitzone, Budgets, Preisfenster, Poolgrößen,
  doppelte Slot-IDs/Listener-Ports und Serviceverweise. Ausgelassene Konfig-
  Sektionen erhalten dieselben Standardwerte wie leere Sektionen.
- Historische Readiness überlebt Preemption. Aktuelle Gesundheit und „war schon
  einmal healthy“ sind getrennt. Locks schützen auch Warmup/Drain/Flip-Pfade.
- Fehlerhafte/unvollständige Vast-Inventare werden nicht als leere Liste
  akzeptiert; das Pagination-Limit liefert einen Fehler statt eines Teilergebnisses.
  Vast-v0-URLs werden korrekt gebildet. Externe Log-URLs erhalten keinen API-Key.
- `/healthz` prüft HTTP-Liveness; `/readyz` verlangt einen frischen erfolgreichen
  Reconciler-Lauf und lesbare Budgettabellen. Readiness bedeutet **nicht**, dass
  ein GPU-Slot schon warm ist. Tote Haupt-Backgroundtasks beenden den Prozess.
  SIGINT/SIGTERM beendet Listener mit maximal 30 Sekunden Drain-Zeit.

## Lokal verifiziert

```sh
cargo test --workspace --locked
cargo build --workspace --locked
python3 scripts/smoke.py
git diff --check
```

Beim Abschluss dieses Hardening: **79 Tests bestanden** (vorher 50), darunter
27 Router-Regressionstests. Der Prozess-Smoke-Test verwendet eine temporäre DB,
einen zufälligen Loopback-Port und eine explizite Umgebung **ohne Cloud-Zugangsdaten**.
Er prüft Liveness, Readiness, Admin-Auth, Cross-Origin-Abweisung und SIGTERM.
Toolchain im lokalen Lauf: Rust/Cargo 1.98.1. Keine echten GPU-Aktionen, keine
Produktionsdatenbank und kein Deployment wurden dafür verwendet.

Nachträgliche [GPU-Messdaten-/Whitelist-Erweiterung](gpu-performance.md):
**100 Tests im Router-Workspace**, zusätzlich **2 Rust- und 5 Python-Tests im Agent**.
Messdaten-/Angebotsseiten und Performance-API sind im isolierten Smoke-Test enthalten.
Keine echte Inferenz-/Disk-Benchmark-Last gestartet; die Messqualität auf realer
Hardware ist noch nicht abgenommen. Die Features sind nicht im bereits vorher
gestarteten Router-Image 0.24 enthalten.

Nach dem [vollständigen Bauplan-Abgleich](plan-audit.md): **119 Workspace-Tests**,
**5 Rust- und 5 Python-Tests im Agent**, **68 ComfyUI-Tests** bestanden. Idle-Flip/
Routing-Rennen, Restart-Warmup-Uhr, Nacht-/DST-Resume und Asset-Readiness wurden
zusätzlich abgesichert. Der unveränderte Decision-Fork hat noch 6 fehlgeschlagene
Tests. Bauplan und Nice-to-haves sind nicht vollständig umgesetzt; neue Images
wurden deshalb noch nicht gebaut/gepusht. Siehe die konkrete Restliste im Abgleich.

`.github/workflows/ci.yml` führt Tests, Build und Smoke-Test aus; zusätzlich
verbietet Clippy vergessene Futures und Mutex-Guards über await. **Clippy ist
lokal nicht installiert und wurde hier nicht erfolgreich ausgeführt; der neue
CI-Lauf ist ebenfalls noch nicht nachgewiesen.** Keine Behauptung über
Test-Coverage-Prozente, Dependency-Audit, Lastfestigkeit oder Hochverfügbarkeit.

## Upgrade / Betrieb

1. Wartungsfenster und Provider-Inventar festhalten. Keine zweite Router-Instanz
   gegen dieselben Verträge starten: die Management-Sperre ist pro Prozess.
2. Vor dem Start eine konsistente DB-Sicherung erstellen, z. B. mit SQLite
   `.backup`, nicht nur eine laufende WAL-Hauptdatei kopieren. Config, Assets und
   Secrets getrennt sichern; Restore in einem isolierten Datenverzeichnis prüfen.
3. `ROUTER_TOKEN` prüfen/gegebenenfalls rotieren (z. B. `openssl rand -hex 32`).
   Alte kurze Tokens führen jetzt bewusst zu einem Startfehler.
4. Migration ergänzt Cursor und Readiness-Historie. **Historische Beträge des
   alten fehlerhaften Meters werden nicht repariert.** Vorhandene Instanzen
   beginnen am Migrationszeitpunkt; ihr bisheriges Leben wird nicht erneut gebucht.
   Alte, bereits verlorene Readiness-Historie lässt sich nicht rekonstruieren.
5. Beispiele validieren und Tests/Smoke-Test ausführen. Dann zunächst Staging,
   niedrige Provider-Ausgabenlimits, Alerts und menschliche Kontrolle nutzen.
6. `/readyz`, Provider-Bestand, Budget-Drift und offene `pending_operations`
   überwachen. Ein 502 bei Stop/Destroy kann einen weiterhin dauerhaft vorgemerkten
   Auftrag bedeuten, nicht dessen Stornierung.
7. SIGTERM fährt den **Router**, nicht automatisch die gemieteten GPUs herunter.
   Soll die Infrastruktur schlafen, vor dem Shutdown `sleep_all` ausführen und
   den tatsächlichen Vast-Zustand bestätigen. Notfallzugriff auf Vast behalten.

Die Service-Proxys sind absichtlich **nicht** durch das Router-Admin-Token
abgesichert (Drop-in-Verhalten für Praxis). Ports ausschließlich an Loopback/
NetBird binden und ACLs testen. Das VPN ersetzt keine Mandantentrennung. Für
Zugriff außerhalb dieses Vertrauensbereichs sind TLS, getrennte Service-Auth,
Rate-Limits und echte Sessions mit Ablauf/Rotation/RBAC erforderlich. Ein
HTTPS-Reverse-Proxy muss Host/Origin konsistent erhalten und Secure-Cookies
setzen; der Router selbst liefert derzeit keine vollständige TLS-/Sessionverwaltung.

## Offene Release-Gates – keine kommerzielle Freigabe davor

- **Provisioning-Crashkonsistenz:** Create hat noch kein dauerhaftes Intent-/
  Idempotency-Journal. Provider-Erfolg mit verlorener Antwort oder Crash vor
  DB-Insert kann eine unverwaltete, kostenpflichtige Instanz hinterlassen.
  Eindeutige Miet-Labels reduzieren Verwechslungen, lösen dieses Problem aber
  nicht. Vor weiterer Automiete Inventar manuell abgleichen, falls Create unklar
  endete. Ein persistenter Recovery-/Adoption-Pfad mit Fault-Injection fehlt.
- **Vollständige Lifecycle-Vertragstests:** Restart-Warmup, zeitgesteuertes Resume,
  Preemption, parallele Pool-Jobs und Datenbank/Agent/Provider-Reihenfolgen müssen
  zusammen geprüft werden. Der Warmup-Watchdog verwendet jetzt ein persistentes
  `boot_started_at`; Schedule-/Sleep-Zeitrechnung und Flip-Zulassung haben neue
  Regressionstests. Das beweist noch nicht die gesamte Zustandsmaschine oder eine
  vollständige fristgebundene Drain-/Resume-Pipeline.
- **Abrechnungsabgleich:** Der Meter ist eine Schätzung anhand beobachteter
  Zustände, nicht die Provider-Rechnung. Zustandswechsel zwischen Polls, Router-
  Ausfälle, Downloads, Traffic, Top-ups und fremde Kontoverträge müssen abgeglichen
  werden. Ein Hard-Cap garantiert keinen exakten maximalen Rechnungsbetrag;
  Provider-Ausfälle und bewusst gepinnte Instanzen können Abschaltung verhindern.
- **Security-Review und Dependency-Audit:** einschließlich Agent/Fork-Images,
  privater Assets, Terminal/Exec, Supply Chain und Secret-Rotation. Neue lokale
  Agent-/ComfyUI-Tests ersetzen keinen Audit der tatsächlich gebauten Images.
- **Last-/Chaos-/Soak-Tests:** langsame/disconnectende Clients, viele offene
  SSE/WS-Verbindungen, große Uploads, mindestens mehrtägiger Betrieb, Disk-full,
  Crash an jedem Provider/DB-Übergang, 429/5xx/Timeouts und verlorene Agent-Verbindungen.
  Akzeptanzwerte für Latenz, Speicher, Wiederanlauf und Kostenabweichung festlegen.
- **Betrieb:** Restore-Drill, Rollback-Plan, Event-/DB-Retention, Monitoring und
  Alarmierung, SLOs, Incident-Runbook und fest gepinnte/auditierte Images.
  SQLite-Zugriffe sind noch synchron; horizontales Active/Active ist nicht unterstützt.

Erst nach diesen Nachweisen ist eine begrenzte Produktionsfreigabe vertretbar.
Eine Aussage „besser als die meisten kommerziellen Produkte“ erfordert darüber
hinaus konkrete Vergleichsprodukte und vergleichbare Messungen.
