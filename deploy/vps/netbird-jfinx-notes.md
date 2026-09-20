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

Beobachtung 20.09. ~19:10: Router-Log zeigte vast-GET-Fehler + alle Boxen
„instance_gone". AUFGELÖST 21.09. nachts: NICHT die v0-API und NICHT das
Env-Reading — **der alte Vast-API-Key wurde von Vast ungültig** (v0
users/current + v0 asks + v1 instances: „Invalid user key"; nur die
publice Offers-Suche nahm ihn weiter an → wirkte wie „Key wird nicht
gelesen"). Neuer Key (in deploy/.env + deploy/nas/docker-compose.yml,
NIE ins Repo!) verifiziert auf ALLEN Router-Endpoints: v1-instances
(success/next_token-Schema ✓), v0-users/current (Metering ✓, Credit
10.71 $), v0-asks (auth ✓), v0-Suche mit Router-Body (Offers mit
dph_total/min_bid ✓). crates/vast braucht KEINE Änderung — die Teil-
Migration (instances=v1, Rest=v0) passt. ⚠️ Credit ist knapp: Pool-Refill
(2× LLM-Download + Media) frisst die 10 $ schnell — ggf. aufladen.

Offen nach NAS-Start:
1. Budget in deploy/nas/vps/config.toml nach Stabilisierung auf 2.0/2.4
   (Achtung: blockiert evtl. den ersten kompletten Pool-Refill, s. Kommentar).
2. Optional Heimserver-Daten übernehmen (alpine-tar pro Volume):
   praxis-data (Chats), router-data (Metering+Host-Blacklist), stt-models.

Erledigt (21.09. nachts): ✅ toter tgrid-`praxis-vps`-Peer (***REMOVED***,
alter Heimserver) per API gelöscht; ✅ Workstation dual-enroll
(`deploy/ws-jfinx-compose.yml`: jfinx als Host-Netz-Container, wt1/WG-51821/
no-DNS via `netbird-wt1.sh`, live bewiesen: Router/Praxis/Gateway von der
Workstation erreichbar); ✅ Multi-Arch-Images (router 0.15, praxis 0.5,
stt 0.2 mit libatomic1-Fix — NAS=arm64 läuft); ✅ STT-Provisioning mit
wget-Resume/Retries.

## Stack-Bein in tgrid (21.09. nachts) — Router AUS tgrid erreichbar
`deploy/nas/docker-compose.yml` hat den Service `netbird-tgrid`: ZWEITER
NetBird im Stack-Netns (network_mode: service:netbird) mit Interface wt1,
WG-Port 51821, DNS aus (alles regelt `deploy/nas/netbird-wt1.sh`, gleiche
Wrapper-Datei wie Workstation). Peer-Name „praxis-vps", Gruppen servers+test,
reusable Key „praxis-vps-nas" `***REMOVED***`
(bis 20.09.2027; tgrid-API: expires_in in SEKUNDEN!). Damit greifen die
tgrid-ACLs: developers→servers:ALLE (volle Nutzung von allen dev-
Maschinen) + Gpuaccess→test:8080,8188,2700,11434-36 (Legacy). Muster live
getestet (Testpair mit Dummy-HTTP: aus beiden Netzen HTTP 200 auf dieselbe
Netns-IP; Test-Peers danach gelöscht). Router bleibt parallel via jfinx
erreichbar (wt0) — Boxen/Agents laufen NUR über jfinx.
⚠️ Sicherheit: Das tgrid-Bein umgeht die jfinx-ACLs für tgrid-devs
(entwurfsgemäß gewollt — Nutzer-Entscheidung 21.09.).

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

## Workstation-jfinx (ws-jfinx-compose.yml): optional, aktuell NICHT deployed
Zweck: gibt DIESEM PC Browser-Zugriff aufs jfinx-Overlay (Host-Netz-Container,
wt1) — volle Operators-Rechte inkl. Praxis :1337/:3537. Nutzer-Entscheidung
21.09.: abgebaut (Container+Peer gelöscht) — Router-Zugriff läuft uber das
tgrid-Bein (nur Router-Ports), das reicht. Datei bleibt als Muster im Repo;
auf dem NAS ware es sinnlos (NAS hat sein eigenes jfinx-Access-NetBird, der
Stack selbst sitzt ohnehin in jfinx).
