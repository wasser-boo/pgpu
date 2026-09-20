# ***REMOVED*** — GPU-NetBird (VPS ***REMOVED***, separiert von tgrid)

Stand 20.09. abends: Management läuft (netbird-server + dashboard + traefik, ~/docker-compose.yml).
Konfiguriert via API (PAT, s. .env — **PAT läuft ~1 Tag**, langlebigen minten!):

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
1. vps/config.toml `[netbird]`: api_url=management_url=https://***REMOVED***,
   groups=["vast-gpus"] setzen; .env hat die neuen Werte schon.
2. NetBird-Volume frisch (`docker volume rm praxis-vps_netbird-config`) →
   praxis-netbird enrollt als NEUER Peer in jfinx (Gruppe praxis-stack).
3. Compose neu hoch; router_nb_ip=auto findet die neue IP.
4. tgrid-GPU-Boxen zerstören → Pool refüllt mit jfinx-Keys (Mint läuft über
   NB_API_TOKEN=neuer PAT).
5. Workstation/NAS dual-enroll: 2. netbird-Instanz (eigene Config),
   Management-URL https://***REMOVED***, Operators-Key.
6. Danach tgrid aufräumen (alte GPU-Peers/Keys; praxis-vps aus tgrid-Gruppen).

## Updates NetBird-Stack (VPS): cd ~ && docker compose pull && docker compose up -d
(Das gepostete `netbirdio/reverse-proxy`-Snippet NICHT nutzen — Traefik macht den Job.)
