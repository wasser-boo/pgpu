# Router 0.26: Standort-/GPU-Auswahl, Kosten und Peer-Bereinigung

## Länderfilter und Ausschlüsse

```toml
search_query = 'gpu_ram>=16 cpu_ram>=48 num_gpus=1 geolocation in [DE,AT,CH,NL,BE,LU,FR,CZ,PL,DK]'
# Alternativen:
# search_query = 'gpu_ram>=16 geolocation!=DE'
# search_query = 'gpu_ram>=16 geolocation notin [DE,FR]'
```

`=`, `!=`, `in` und `notin` unterstützen Länder-ISO-Codes. Listen dürfen Leerzeichen/Anführungszeichen enthalten. Hardware-Ranges wie `gpu_ram>=16 gpu_ram<=48` bleiben erhalten. Defekte, leere oder doppelte gleichartige Filter werden abgelehnt, nicht stillschweigend weggelassen. Beide Suchstrings (`search_query` und `search_query_on_demand`) werden geprüft und jeweils für ihren Mietmodus benutzt.

Die Syntax benötigt **0.26 oder neuer**. Alte Images können neue Suchsyntax ignorieren und dürfen dafür nicht verwendet werden.

## Karte, Startland Deutschland und Radius

Pro Slot optional:

```toml
[slots.location]
origin_country = "DE"
countries = ["DE", "AT", "CH", "NL", "BE", "LU", "FR", "CZ", "PL", "DK"]
excluded_countries = []
# radius_km = 1000.0
# latitude = 50.0
# longitude = 10.0
```

**Settings → Länder & GPU-Modelle** enthält Startland, Karte/Kartenklick, Radius, Länder-Allowlist, Ausschlüsse und die effektive Vorschau beider Mietmodi. Karten-/Vorschaudaten kommen nur vom eigenen Router, nicht von externen Karten- oder Geocoding-Diensten.

- Deutschland ist der Standard-Startpunkt. Es wird keine Startstadt oder Radiusdistanz unterstellt.
- Ohne `radius_km` gibt es keinen Radiusfilter. Optionaler genauer Startpunkt: `latitude` und `longitude` immer gemeinsam setzen; sonst gilt der Referenzpunkt des Startlands.
- Radius bedeutet **Länder-Näherung**: ausgewählt werden Länder, deren kartografischer Referenzpunkt innerhalb der Großkreisentfernung liegt. Grenzen dienen der Kartenanzeige, nicht einer exakten Host-Ortung.
- **Das gesamte Land ist dann erlaubt.** Ein Host, eine Überseeregion oder ein Randgebiet kann außerhalb des Kreises liegen. Insbesondere große Länder machen diese Näherung grob. Es ist keine Latenz- oder Kilometer-Garantie für einen GPU-Server.
- Länder-Allowlist, Radius und explizite `search_query`-Filter werden geschnitten. Leere Allowlist = alle Länder vor Radius/Ausschlüssen. Für eine reine Radiusauswahl die Liste leeren.
- **Ausschlüsse haben immer Vorrang**, auch `geolocation!=DE`/`notin` aus dem Suchstring. Eine unmögliche/leere Schnittmenge wird als Konfigurationsfehler angezeigt.
- Unbekannte Standorte dürfen bei aktivem Länderfilter nicht gemietet werden. Die Prüfung erfolgt auch lokal vor manuellen/automatischen Mieten; Provider-Filter allein sind keine Sicherheitsgrenze.
- Die Standortauswahl betrifft neue Angebote/Mieten. Bestehende kalte/warm gehaltene Verträge werden durch einen Geofilterwechsel nicht gelöscht oder allein deswegen vom Wiederanlauf ausgeschlossen. Sonstige bestehende Hardware-/Kosten-/Policy-Regeln gelten weiter.

Der UI-Speicherpfad erhält andere TOML-Einstellungen und Kommentare. Such-Cache wird nach Speichern verworfen. Automatische Miete/Policy bleibt aktiv und verwendet die neue Auswahl beim nächsten Abgleich; das Speichern führt selbst keinen Benchmark oder Instanzumbau aus.

### Kartendaten

Natural Earth, **Public Domain**, Admin-0 Countries 1:50m. Quelle und Lizenz: <https://www.naturalearthdata.com/about/terms-of-use/>.

- Upstream: `nvkelso/natural-earth-vector`, Commit `ca96624a56bd078437bca8184e78163e5039ad19`.
- `geojson/ne_50m_admin_0_countries.geojson`, SHA256 `3e458fc036ad0a66411f2c1e6cac49c5d7bfb81cb1123bc513b22511a2b7fdeb`.
- Reproduktion: `python3 scripts/update-countries.py` (oder `--source /pfad/zur/original.geojson` offline).
- ISO-Zuordnung über `ISO_A2_EH`; 237 Codes. Referenzpunkte sind `LABEL_X/LABEL_Y`, **keine exakten geographischen Mittelpunkte oder Hauptstädte**. Deutschland: Länge 9.678348, Breite 50.961733.
- Nicht zugeordnete/disputierte Codes werden nicht als fiktive Vast-Länder erfunden. Kartengrenzen sind vereinfachte Daten und keine politische Aussage.

## Mehrere GPU-Modelle

```toml
[slots.requirements]
gpu_names = ["RTX 5070 Ti", "RTX 4060 Ti", "RTX A4000", "RTX 3090"]
num_gpus = 1
min_gpu_ram_gb = 16.0
```

Alternativ in Settings ein Modell pro Zeile; leer erlaubt alle ansonsten passenden Modelle. Dies sind **Alternativen**, keine Anforderung nach vier physischen GPUs. Die Auswahl wird an Vast weitergegeben und zusätzlich lokal geprüft. Rohfilter wie `gpu_name="RTX 3090"` bleiben weitere Einschränkungen — im TOML-Editor entfernen/ändern, wenn sie nicht mehr gewünscht sind. Mindest-RAM/VRAM, Host-Trust und sämtliche Preisgrenzen werden nicht gelockert.

Preisfenster stehen unter `[slots.bid]`: `rent_min_usd_h` und `ceiling_usd_h`. Beispielsweise `0.0` bis `0.20` bedeutet 0–20 **US-Cent pro GPU-Stunde**, nicht Euro-Cent und nicht automatisch inklusive Storage/Traffic. Zusätzliche effektive Kostenlimits stehen in `[slots.requirements]`. Eine bestehende Obergrenze wird durch dieses Update nicht angehoben.

## Preise und Stundenlimit (0.26.1)

Vast liefert im **Bid-Suchmodus** unter `dph_total` einen anderen Preis als bei **On-demand**. Ein ausschließlich in der Bid-Suche gefundenes Angebot erhält deshalb keinen synthetischen On-demand-Preis. Fehlende/unbrauchbare Preise sperren diesen Mietmodus; der Angebotscache wird beim Routerstart verworfen. `search_query_on_demand` ist eine eigene Suche, keine Zusicherung, dass jede Bid-Instanz auch dort enthalten ist.

Provider-`dph_total` enthält bereits den zugewiesenen Speicher. Router-intern wird der Compute-Anteil aus `dph_base`, ersatzweise Gesamtpreis minus Speicher, normalisiert. **Compute + Speicher genau einmal** gilt gemeinsam für Admission, Settings und Ausgabenübersichten. Beim Vertragsabgleich werden beobachteter Compute- und Speicherpreis zusammen aktualisiert. Der historische interne Feldname `dph_total` bezeichnet ab 0.26.1 daher den normalisierten Compute-Anteil; Rohantworten der Vast-API behalten ihre eigene Semantik. Alte Budget-/Meter-Historie wird nicht rückwirkend gelöscht oder als Erstattung behandelt.

Die Instanzansicht zeigt bei On-demand die zuletzt gespeicherte aktuelle Mietrate statt des ursprünglichen Spot-/Gebotsfelds, daneben Speicher und Summe ohne Traffic. Angebot und späterer Providerstand sind Momentaufnahmen, keine Preisgarantie. Ein veränderbares Spot-Gebotsformular wird bei On-demand nicht angeboten.

**`hourly rate limit reached` ist kein API-Request-Limit.** Die Meldung nennt vorhandenen Compute, angefragten Compute, sämtliche Mietdisks und `limits.max_total_rate_usd_h`. Startende Instanzen zählen bereits, gestoppte Verträge behalten ihre Speicherkosten. Beim Resume und beim Ändern eines Gebots wird die vorhandene Disk genau einmal berücksichtigt, weder doppelt noch gar nicht. Bestehende Caps werden nicht angehoben.

## Echte Vast-Nutzung nach Slot-Labels

**Patch 0.26.1:** Vast verlangt den abschließenden Slash. 0.26 erhielt auf `/charges` eine HTTP-301-Antwort; bestehende Kosten/Budgethistorie blieben dabei erhalten. 0.26.1 ruft direkt `/charges/` auf, ohne Weiterleitungen für API-Zugangsdaten zu erlauben.

Der Abgleich liest **`GET /api/v0/charges/`** mit dem Datumsfenster und Vertragstyp `instance`, komplett paginiert. Quelle: <https://docs.vast.ai/api-reference/billing/show-charges>. Payment-Invoices, Einzahlungen, Überweisungen, Refund-Transaktionen und bloße Kontostand-Deltas sind keine GPU-Nutzung und werden nicht als solche verbucht.

Scope sind die von den konfigurierten Slots erzeugten Labels:

```text
praxis-llm-s1-<token8>
praxis-media-s2-<token8>
```

**Nicht nach Online-Status filtern:** laufende, gestoppte/schlafende und historische/entfernte passende Verträge zählen, einschließlich GPU, Disk und Traffic in deren tatsächlichen Nutzungskosten. Andere Rollen/Slot-IDs/Präfixe zählen nicht. Aktuelle explizite Labels haben Vorrang; für historische Verträge werden Abrechnungsmetadaten bzw. bekannte DB-Labels herangezogen. Gleiche Label-Namensräume auf mehreren Routern im selben Vast-Konto teilen diesen Scope; für Peer-Löschungen ist dagegen zusätzlich die lokale Ownership erforderlich.

Nach Start und anschließend stündlich läuft der Abgleich im Hintergrund. Er blockiert weder Lifecycle-Steuerung noch lokales Metering. Fehlende Rechte, HTTP-Fehler, kaputte/inkonsistente oder unvollständige Seiten ergeben **keinen Null-Verbrauch**; letzter erfolgreicher Stand und Budgethistorie bleiben erhalten.

Settings zeigt echte Providerkosten und lokale Schätzungen getrennt, Zeitraum, Quelle `/api/v0/charges/`, letzten Versuch, letzten Erfolg und Einzelergebnisse. Provider-HTML wird nicht als Fehlertext durchgereicht; auch alte gespeicherte 301-HTML-Seiten werden in der Oberfläche gekürzt. `/api/v1/budget` enthält `vast_usage`, lokale `metered_*` und effektive `spent_*`-Werte. Tages-/Monatsgrenzen werden aus `router.tz` in UTC-Unix-Grenzen für die Vast-Abfrage umgerechnet; die Vergleichsfenster sind in den Daten enthalten. Provider-Tagesaggregation, Verzögerung und Rundung können Unterschiede zum lokalen Live-Meter oder einer anders eingestellten Vast-Ansicht verursachen. Ohne lokalen Datensatz steht die Schätzung ausdrücklich auf unbekannt.

### Konservative Budgetkorrektur

- Bestätigte Slot-Nutzung bildet einen dauerhaften **Mindestverbrauch**. Darunter fällt das Budget nicht zurück, wenn ein Vertrag stoppt, entfernt oder später anders beschriftet wird.
- Seit dem Abgleich angefallene lokale Nutzung wird weitergerechnet; wiederholte Abgleiche addieren dieselbe Rechnung nicht erneut.
- Verzögerte/niedrigere Providerstände löschen keine Kosten und setzen keinen Zähler auf null. Das ist ein konservativer Schutz, keine Behauptung einer endgültigen oder sekundengenauen Rechnung.
- Alte nicht pro Instanz zuordenbare Meter-/Traffic-Werte bleiben erhalten. In solchen Zeiträumen wird statt potenziell doppelter Einzelkorrekturen ein aggregierter Provider-Mindestwert verwendet. Neue Traffic-Reports werden atomar der Instanz und dem Tagesbudget zugeordnet.
- **Konfigurierte Tages-/Monatslimits, Währungskurs und `hard_action` werden nicht verändert.** Reale höhere Kosten können wie sonst die bestehende Budget-Policy auslösen. Zeitplan-Budget-Overrides bleiben wirksam.

## NetBird-Peers sicher aufräumen

```toml
[netbird]
cleanup_unused_peers = true # Default; false deaktiviert die Bereinigung
```

Ungefähr alle fünf Minuten prüft der Router nach vollständigem Vast-Inventarabgleich lokal bekannte, seit mindestens fünf Minuten entfernte Verträge. Ein Peer wird nur bei eindeutiger Namenszuordnung, bestätigter Abwesenheit des Vertrags, Offline-Status und erneuter Prüfung direkt vor DELETE entfernt.

- Ein noch vorhandener Vast-Vertrag schützt seinen Peer **auch gestoppt/schlafend oder ohne Agent-Heartbeat**.
- Lebende lokale Datensätze und aktive Provider-Labels mit gleichem Token-Präfix schützen ebenfalls.
- Unbekannte/fremde Geräte, mehrdeutige Namen, gerade wieder verbundene/umbenannte Peers und fehlerhafte/partielle Inventare werden nicht gelöscht.
- Ein akzeptierter Destroy-Auftrag allein reicht nicht. Fehler werden später wiederholt, 404 gilt als bereits entfernt. Mehrere veraltete Peers desselben eindeutig entfernten Vertrags können bereinigt werden.
- Ereignisse: `netbird_peer_deleted`; API-Fehler im Router-Log. Es werden keine Mietverträge durch diese Bereinigung gelöscht.

## Prüfung und Upgrade

```bash
praxis-router --version
praxis-router --config config.toml --check-config
```

`--check-config` prüft nur Schema/Filter, ohne Provider-Anfragen und ohne Authentifizierungs-/Erreichbarkeitsnachweis. Normaler Startup und Live-Speichern verlangen weiterhin gültige Router-Authentifizierung.

Vor Upgrade Config und SQLite-Volume sichern. Neue Tabellen sind additive Migrationen, alte Meter-Historie bleibt erhalten. Bei Rollback auf 0.25 wieder eine 0.25-kompatible Konfiguration verwenden: neue Suchsyntax/Länderregeln dürfen dort nicht stillschweigend ignoriert werden. Versionstags behalten; das Publizieren eines Images aktualisiert kein laufendes NAS automatisch.

Discord, mehrere URLs und Testbutton: [Webhooks](webhooks.md).
