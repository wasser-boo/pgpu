# Webhooks und Discord testen

## Discord-Konfiguration

In Discord: **Servereinstellungen → Integrationen → Webhooks → Webhook erstellen → Webhook-URL kopieren**. Die kopierte URL ist ein Zugangsschlüssel; nicht veröffentlichen oder ins Git einchecken.

Den vorhandenen `[alerts]`-Abschnitt der Router-Konfiguration bearbeiten (nicht ein zweites Mal hinzufügen):

```toml
[alerts]
webhook_urls = [
  "https://discord.com/api/webhooks/ID_KANAL_1/TOKEN_1",
  "https://discord.com/api/webhooks/ID_KANAL_2/TOKEN_2",
]
webhook_format = "discord"
```

Die Platzhalter durch die tatsächlich kopierten URLs ersetzen; für nur einen Kanal den zweiten Eintrag entfernen. Eine normale Discord-Kanal-/Einladungs-URL ist kein Webhook.

Jede Warnung und Testnachricht geht an **alle** eingetragenen Ziele. Die bisherige einzelne `webhook_url = "..."` bleibt kompatibel und wird gegebenenfalls zusätzlich berücksichtigt. Identische URLs (auch mit unterschiedlichem Discord-`wait`-Parameter) werden nur einmal angeschrieben. Maximal 16 verschiedene Ziele; leere/ungültige Einträge werden beim Speichern abgelehnt.

1. **Settings → config.toml → Speichern & live anwenden**.
2. Unter **Webhooks / Discord → Testnachricht senden** klicken.
3. In jedem gewählten Discord-Kanal erscheint:

   ```text
   [PGPU] webhook_test
   ✅ PGPU-Testnachricht: Die gespeicherte Webhook-Konfiguration funktioniert.
   Dies ist nur ein Verbindungstest — keine GPU wurde gestartet, gestoppt oder gemietet.
   ```

Die Oberfläche zeigt für **jedes Ziel** Annahme oder Fehler sowie die Gesamtzahl erfolgreicher Zustellungen. Ein defekter Webhook blockiert die anderen nicht. Zielnummern entsprechen der Konfigurationsreihenfolge (eine vorhandene alte Einzel-URL zuerst). Getestet wird nur die **gespeicherte/live geladene** Konfiguration, nicht der noch ungespeicherte Inhalt des Editors. Das Speichern allein versendet keine Nachricht. Höchstens ein Test pro zehn Sekunden. Der Test verändert keine GPU, Mietverträge, Limits oder Budgets.

## API-Test

Nur nach bewusstem Auslösen wird eine Nachricht an jeden eingetragenen Dienst gesendet. `ROUTER_TOKEN` ist der Router-Administrationsschlüssel, **nicht** der Discord-Token.

```bash
read -rsp 'Router token: ' ROUTER_TOKEN; echo
ROUTER_URL='http://praxis-vps.the.grid:8080'
curl --fail-with-body --silent --show-error \
  --request POST \
  --header "Authorization: Bearer ${ROUTER_TOKEN}" \
  "${ROUTER_URL}/api/v1/alerts/test"
unset ROUTER_TOKEN
```

Erfolgsantwort (Beispiel):

```json
{"ok":true,"sent":2,"failed":0,"results":[{"target":1,"host":"discord.com","format":"discord","ok":true,"status":200},{"target":2,"host":"discord.com","format":"discord","ok":true,"status":200}]}
```

Ohne Webhook-Konfiguration: HTTP 400. Ohne Router-Authentifizierung: HTTP 401. Zu häufige Tests: HTTP 429. Mindestens ein fehlgeschlagenes Ziel: HTTP 502 mit `sent`, `failed` und sämtlichen Einzelergebnissen inklusive verständlichem Fehler/HTTP-Status. Erfolgreiche Ziele werden trotz eines anderen Fehlers nicht erneut angeschrieben. Die Test-API nimmt **keine fremde Ziel-URL oder eigene Nachricht** an.

## Andere Dienste

`webhook_format = "auto"` ist der Default und eignet sich für gemischte Ziele. Discord, Slack und `ntfy.sh` werden **pro Ziel** anhand des Hostnamens erkannt; andere Dienste erhalten generisches JSON. Ein ausdrücklich gewähltes Format gilt dagegen für alle URLs. Für eigene Reverse-Proxies oder selbst gehostetes ntfy das Format ausdrücklich setzen.

| Format | Gesendete Daten |
|---|---|
| `discord` | `{"content":"[PGPU] …","allowed_mentions":{"parse":[]}}`; `wait=true` fordert eine Bestätigung an |
| `slack` | `{"text":"[PGPU] …"}` |
| `ntfy` | UTF-8-Text im POST-Body an die Topic-URL |
| `json` | `{"kind":"…","message":"…"}` (bisheriges generisches Format) |

Discord-Nachrichten bleiben innerhalb des 2.000-Zeichen-Limits; Benutzer-/Rollen-/Everyone-Pings sind deaktiviert. Weiterleitungen werden nicht verfolgt. URLs dürfen HTTP(S) verwenden, aber keine eingebetteten Benutzername/Passwort- oder Fragment-Bestandteile.

## Automatische Warnungen und Fehler

Die Policy sendet weiterhin ihre Warnungen (Budget, Preemption, Bid-Limit, fehlgeschlagene Swaps). Gleiche Warnungsarten werden für 30 Minuten dedupliziert. Nicht jedes gewöhnliche Router-Ereignis ist automatisch eine Warnung.

Versanderfolg, Fehler und Wiederholungen stehen in **Events** (`webhook_sent`, `webhook_failed`, `webhook_retry`; Tests: `webhook_test_sent`/`webhook_test_failed`). Fehlgeschlagene HTTP-Antworten gelten **nicht** als zugestellt. URL und ungeprüfte Antworttexte werden nicht protokolliert.

Automatische Warnungen werden bei Netz-/5xx-/429-Fehlern **pro fehlgeschlagenem Ziel** höchstens zweimal wiederholt. Erfolgreiche Ziele werden nicht wiederholt. Numerische `Retry-After`-Angaben werden beachtet; bei mehr als fünf Minuten wird nicht vorzeitig erneut gesendet. Andere 4xx-Fehler benötigen eine korrigierte Konfiguration. Die Wiederholungen laufen im Hintergrund und blockieren keine GPU-Verwaltung; sie sind Best-Effort und überleben keinen Prozessneustart. Es wird nach einer URL-/Formatänderung nicht erneut an das alte Ziel gesendet.

Typische Fehler:

- **400:** falsches Datenformat/Parameter, eventuell falsches Format für einen Proxy.
- **401/403:** ungültige Webhook-Zugangsdaten oder fehlende Berechtigungen.
- **404:** Webhook gelöscht oder URL falsch kopiert.
- **429:** Rate-Limit; später erneut testen.
- **Netzwerkfehler:** DNS, Internetzugang des Router-Containers oder TLS prüfen.

Ein Router-Neustart ist für Änderungen unter `[alerts]` nicht erforderlich. Diese Funktionen benötigen das neue Router-Image; eine veröffentlichte Version aktualisiert ein laufendes NAS nicht automatisch.
