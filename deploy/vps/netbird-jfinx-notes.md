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

## Migration (offen):
0. ⚠️ **Vast hat am 20.09. ~19:10 die v0-API abgeschaltet** (deprecated_endpoint):
   der Router (crates/vast, BASE=…/api/v0) sieht keine Instanzen/Offers mehr und
   hat daraufhin alle 3 Boxen als „instance_gone" entsorgt (Vast-Konto: 0
   Instanzen). crates/vast MUSS vor dem nächsten Box-Start auf /api/v1 umgestellt
   werden (Endpoints + Antwort-Schema prüfen), sonst mietet der Pool nie wieder.
1. vps/config.toml `[netbird]`: api_url=management_url=https://***REMOVED***,
   groups=["vast-gpus"] setzen; .env: NB_MANAGEMENT_URL=https://***REMOVED***,
   NB_SETUP_KEY=***REMOVED***-…, NB_API_TOKEN=<NB_API_TOKEN_JFINX>.
2. NetBird-Volume frisch (`docker volume rm praxis-vps_netbird-config`) →
   praxis-netbird enrollt als NEUER Peer in jfinx (Gruppe praxis-stack).
3. Compose neu hoch; router_nb_ip=auto findet die neue IP.
4. tgrid-GPU-Boxen zerstören → Pool refüllt mit jfinx-Keys (Mint läuft über
   NB_API_TOKEN=neuer PAT). (Vast-Konto hat aktuell 0 Instanzen — nichts
   mehr zu zerstören, Refill startet automatisch.)
5. Workstation dual-enroll: 2. netbird-Instanz (eigene Config),
   Management-URL https://***REMOVED***, Operators-Key.
6. Danach tgrid aufräumen (alte GPU-Peers/Keys; praxis-vps aus tgrid-Gruppen).

## NAS: fertig ✅ (20.09. abends II)
`deploy/nas-compose.yml` = Zero-Config-Compose mit BEIDEN NetBirds (Bridge-
Netns, Sidecar-Muster): netbird-tgrid → tgrid (Peer „nas", Gruppen
developers+servers, frischer reusable Key „nas-dual" `***REMOVED***-…`, bis
20.09.2027 — tgrid nimmt expires_in in SEKUNDEN!) + netbird-jfinx →
jfinx (Operators-Key). Live getestet: beide Enrollments Connected (tgrid
***REMOVED*** / jfinx ***REMOVED***), Overlay-Datenpfad via ACL
(nas→forgejo:22 OK); Test-Peers danach per API gelöscht. Auf dem NAS nur
`docker compose -f nas-compose.yml up -d`.

## Updates NetBird-Stack (VPS): cd ~ && docker compose pull && docker compose up -d
(Das gepostete `netbirdio/reverse-proxy`-Snippet NICHT nutzen — Traefik macht den Job.)
