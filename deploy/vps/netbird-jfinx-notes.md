# ***REMOVED*** — GPU-NetBird (VPS ***REMOVED***, separiert von tgrid)

Stand 20.09. abends II: Management läuft (netbird-server + dashboard + traefik, ~/docker-compose.yml).
**Langlebiger PAT „pgpu-router-mint" ist gemintet (bis 20.09.2027)** — liegt als
`NB_API_TOKEN_JFINX` in deploy/.env (gitignored). Der alte 1-Tages-PAT („token")
ist damit obsolet. Bei der Router-Migration: Wert als `NB_API_TOKEN` übernehmen
(der jetzige NB_API_TOKEN ist der tgrid-PAT — der bleibt bis zum tgrid-Cleanup
benötigt).

- **Gruppen**: `vast-gpus` (Vast-Boxen), `praxis-stack` (Router-Peer), `operators` (Workstation/NAS)
- **Default All→All-Policy GELÖSCHT** (Trennung!)
- **Policies**: boxes-callhome (vast-gpus→praxis-stack:8080), router-dial-boxes
  (praxis-stack→vast-gpus:9100,8188,2700,11434-36), operators-dashboards
  (operators→praxis-stack:8080,1337,3537,8188,2700,11434-36), operators-debug-boxes
  (operators→vast-gpus:8188,9100,11434,11435)
- **Setup-Keys**: Router/Peer `praxis-stack`: ***REMOVED***,
  Operators/NAS: ***REMOVED***

## API-Schema des NEUEN netbird-server (weicht von tgrid ab!)
- POST /api/policies: `rules[].sources|destinations` = **Array von Gruppen-ID-Strings**
  (keine Objekte!), `ports` = **Array von Einzelport-Strings auf Rule-Ebene**
  (keine Ranges — dafür `port_ranges`).
- Setup-Keys/Gruppen: wie gehabt (auto_groups = ID-Array).

## Migration — Plan (20.09. spät): Stack wandert auf den NAS
Statt den Router auf dem Heimserver neu zu enrollen, zieht der GESAMTE Stack
auf den NAS (immer an). `deploy/nas/` = Komplett-Bundle (s. NAS-Abschnitt).
Der Heimserver-Stack bleibt GESTOPPT (gleicher VAST_API_KEY — niemals zwei
Router gleichzeitig!) und wird nach NAS-Health stillgelegt
(`docker compose -f vps-compose.yml down` auf dem Heimserver).

Beobachtung 20.09. ~19:10: Router-Log zeigte vast-GET-Fehler
(deprecated_endpoint für /api/v0) + alle Boxen „instance_gone" — Nutzer:
Boxen wurden selbst zugemacht, Vast mietet normal. FALLS der NAS-Router beim
Start keine Offers/Instances sieht: /api/v0→/api/v1 in crates/vast
(BASE-Konstante) prüfen. Vast-Konto hat aktuell 0 Instanzen.

Offen nach NAS-Start:
1. tgrid aufräumen: tote GPU-Peers/-Keys, toten praxis-vps-Peer
   (***REMOVED***) löschen — tgrid-PAT liegt als NB_API_TOKEN in deploy/.env.
2. Budget in deploy/nas/vps/config.toml nach Stabilisierung auf 2.0/2.4
   (Achtung: blockiert evtl. den ersten kompletten Pool-Refill, s. Kommentar).
3. Optional Heimserver-Daten übernehmen (alpine-tar pro Volume):
   praxis-data (Chats), router-data (Metering+Host-Blacklist), stt-models.
4. Workstation dual-enroll für jfinx (Operators-Key), wenn Browser-Zugriff
   vom Arbeitsplatz gewünscht — das NAS bringt sein eigenes jfinx-Access-
   NetBird mit.

## NAS: fertig ✅ (20.09. abends II) — zwei Artefakte
1. `deploy/nas-compose.yml` — NUR die beiden Access-NetBirds (Bridge-Netns,
   Sidecar-Muster): netbird-tgrid → tgrid (Peer „nas", Gruppen
developers+servers, reusable Key „nas-dual" `***REMOVED***-…`, bis 20.09.2027 —
tgrid nimmt expires_in in SEKUNDEN!) + netbird-jfinx → jfinx (Operators-Key).
Live getestet: beide Enrollments Connected (tgrid ***REMOVED*** / jfinx
***REMOVED***), Overlay-Datenpfad via ACL (nas→forgejo:22 OK); Test-Peers
hinterher per API gelöscht.
2. `deploy/nas/` (gitignored — echte Secrets inline!) = KOMPLETT-Bundle:
`docker-compose.yml` mit dem GANZEN GPU-Stack (netbird → jfinx als
„praxis-vps" mit Key `***REMOVED***-…`; stt; router mit ROUTER_TOKEN/VAST_API_KEY/
NB_API_TOKEN=jfinx-PAT inline; praxis mit ALLEN Env inline + master_key-
Secret + praxis-state-Bind) PLUS den beiden Access-NetBirds. Dazu `vps/
config.toml` (auf jfinx+vast-gpus umgestellt; Budget 12/14 € Test-Wert),
`vps/master_key`, `vps/praxis-state/` (Secret-Store). Start auf dem NAS:
das Verzeichnis rsyncen + `docker compose up -d` (Details im Compose-Header).
Verifiziert: `docker compose config` valid; praxis-stack-Key live enrollt
(praxis-vps → ***REMOVED***, Connected; Test-Peer danach gelöscht).
Frisch auf dem NAS: neue Peer-IPs, leere Volumes (keine Chats/Metering —
Übernahme optional, s. Migration Punkt 3).

## Updates NetBird-Stack (VPS): cd ~ && docker compose pull && docker compose up -d
(Das gepostete `netbirdio/reverse-proxy`-Snippet NICHT nutzen — Traefik macht den Job.)
