# VPS-Deploy: Praxis + GPU-Router + STT + NetBird (ein Compose-Stack)

Zielbild (Bauplan Phase 7): statt Router auf dem Heim-PC (`wasser-6-69`)
läuft der gesamte Praxis-Stack auf einem VPS — gleiche Compose auf jedem
Host, `/data`-Volumes mitnehmen, fertig. Der Stack:

```
                    ┌──────────────── VPS (1 Netzwerk-Namespace) ────────────────┐
Arbeitsplatz ──NetBird──► netbird (wt0) ─┬─► router :8080 + :8188 :2700 :11434-36
(pagent / wasser)     ACL-geregelt       ├─► stt :2700 (127.0.0.1, Vosk)
                                        └─► praxis :3537 (127.0.0.1 + Overlay)
GPU-Boxen (vast) ──NetBird──► router :8080 (Call-home/WS) ◄── Router wählt sich
                              in die Agents ein (:9100, netstack local forwarding)
```

**Warum das sicher ist (und was der Betreiber noch tun muss):**

1. **Kein Port-Publishing.** Alle Dienste teilen den Netzwerk-Namespace
   des NetBird-Containers (`network_mode: "service:netbird"`). Die
   öffentliche IP des VPS exponiert *keinen* dieser Ports — sie erreichen
   Peers ausschließlich über das NetBird-Overlay (`wt0`), und dort
   entscheidet allein die **NetBird-ACL** (WireGuard, tgrid).
2. **Loopback für die internen Dienste.** Praxis → Router → STT laufen
   über `127.0.0.1` im Namespace (STT lauscht bewusst nur auf Loopback,
   `VOSK_BIND=127.0.0.1`); kein GPU-Overlay-Hop, keine ACL zwischen ihnen.
3. **NetBird-ACLs sind die Firewall.** Der Peer `praxis-vps` braucht die
   Gruppen `test` (damit `Gpuserver→test:8080` Call-home und
   `Gpuaccess→test:8080,8188,2700,11434-36` greifen) und `servers`
   (`developers→servers` = Arbeitsplatz → Dashboards). Ohne passende
   Regel ist ein Port unerreichtbar — auch für andere Peers.
4. **Secrets nie in Env oder Klartext.** Externe Secrets (Discord-Token,
   Provider-Keys) liegen ausschließlich verschlüsselt im Secret-Store
   (`secrets.enc2`, AES-256-GCM + Argon2) — für die Agent-Shell nur
   Ciphertext. Der Master-Key wird per **root-Entrypoint** zugestellt:
   `vps/master_key` (root:root 0600, als Compose-Secret gemountet) →
   Entrypoint kopiert ihn nach `/dev/shm` (tmpfs, 0400) → droppt auf
   uid 1000 → **Praxis liest+löscht ihn beim Start**. Der Key ist nie
   in argv/env (`sh -c env`, `/proc/environ` bleiben sauber) und die
   Datei existiert nur im Startfenster, bevor der Agent Befehle
   ausführen kann; bei jedem Container-Start liefert der Entrypoint
   ihn erneut (reboot-sicher). GATEWAY_API_KEY/DASHBOARD_ADMIN_PASSWORD
   (Selbst-Zugangsdaten) bleiben bewusst im Env — sie gewähren nichts,
   was der Agent nicht ohnehin schon steuert.
   *Grenze (ehrlich):* Praxis läuft im shared VM-mode — die Agent-Shell
   kann alles lesen, was uid 1000 lesen kann (Chat-DB, Env-Datei). Der
   Master-Key + Store sind davon bewusst ausgenommen. `/proc/mem`-Zugriff
   der Shell auf den Praxis-Prozess blockt Yama (ptrace_scope=1).
5. **Least privilege.** Router (uid 10001) läuft unprivilegiert; der
   Praxis-Entrypoint startet als root NUR für die Key-Zustellung + chown
   der State-Verzeichnisse und droppt sofort per `setpriv` auf uid 1000.
   Nur der NetBird-Container braucht `NET_ADMIN` + `/dev/net/tun`
   (funktional unvermeidbar).
6. **Host-Basisabsicherung** (einmalig, außerhalb der Compose):
   `ssh` nur mit Key (`PasswordAuthentication no`), `ufw default deny`
   mit Ausnahme ssh, `unattended-upgrades`, Fail2Ban nach Geschmack.
   Docker published keine Ports dieses Stacks — die UFW-Regeln für die
   Containerdienste sind damit automatisch „deny by default".

## Voraussetzungen

- VPS ≥ 8 GB RAM / 2–4 vCPU (STT-Modelle de+ja ≈ 6 GB geladen),
  Debian 12/Ubuntu 24.04, Docker + Compose v2.
- Auf tgrid: **reusable Setup-Key** für den Peer `praxis-vps` mit den
  Gruppen `test` und `servers` minten (NetBird-UI → Setup Keys → neu,
  Typ *reusable*; der Router mintet Keys für GPU-Boxen später selbst
  per `NB_API_TOKEN`). Der NetBird-**Management-Server bleibt, wo er
  ist** (tgrid) — der VPS enrollt nur als Peer.

### DigitalOcean (empfohlene Variante)

- **Droplet:** `s-4vcpu-8gb` (≈ 48 $/Mon, 8 GB RAM) — kleiner geht nur
  mit `STT_LANGUAGES=de` (de-Modell ≈ 4,5 GB) und dann knapper 6 GB.
- **Region:** `nyc1/nyc2/nyc3` — die Vast-GPU-Angebote (3090/5070 Ti)
  stehen überwiegend in den USA; mit dem VPS in NYC laufen LLM-Stream,
  Comfy-Downloads und Agent-Dial regional statt transatlantisch.
  Wer Voice-Priorität hat (Mikrofon→STT aus DE): `fra1` — sonst gleiche
  Funktion, nur andere Latenzverteilung.
- **KVM → `/dev/net/tun`** ist vorhanden (`ls /dev/net/tun` prüfen).
- **DO-Cloud-Firewall:** inbound nur SSH erlauben — der Stack published
  keine Ports; alles läuft über das NetBird-Overlay.
- Ersteinrichtung: Ubuntu 24.04 + `apt install docker.io docker-compose-v2`
  (bzw. Dockers offizielles Repo), SSH-Key-only, dann „Einrichtung“ unten.
- GPU-Boxen ↔ VPS: Overlay via tgrid (Relay/Direct wie gehabt) — der
  Standort des Managements ist dafür egal.
- Images auf Docker Hub: `vayayo/praxis-gpu-router`, `vayayo/praxis-stt`,
  `vayayo/praxis` (bauen/pushen: `bash deploy/build.sh --push` im
  jeweiligen Repo, Praxis: `bash deploy/build.sh --push`).

## Eigenes NetBird-Netzwerk (statt tgrid) — empfohlen

Für den GPU-Stack ein **dediziertes NetBird** betreiben: GPU-Boxen, VPS-Stack
und dein Arbeitsrechner bilden ein eigenes Overlay — komplett getrennt vom
Rest-Netz (forgejo, pagent, …). Ein Kompromittieren/Lerken einer GPU-Box
kann nichts außer dem GPU-Netz sehen.

**Hosting:** Die offizielle NetBird-Selfhost-Compose
(`netbird getting-started`, ~7 Container: management, signal, relay,
coturn, dashboard, Zitadel-IdP, Caddy) braucht:

- eine Domain, die du per DNS steuerst — je ein A-Record auf die Instanz:
  `netbird.<domain>`, `dashboard.<domain>`, `api.<domain>`, `signal.<domain>`,
  `relay.<domain>`, `coturn.<domain>`, plus `*.coturn.<domain>` (Turn-Port-Range)
- offene Ports: 80/443 (tcp), 3478 (udp/tcp), 49152-65535/udp optional
  (Relay-Fallback), 33073 (signal, tcp — oder hinter dem Proxy)
- ~2–4 GB RAM — passt auf ein eigenes kleines Droplet (z. B.
  `s-2vcpu-4gb`, 24 $/Mon) neben dem 8-GB-Praxis-Droplet, oder auf
  jeden anderen kleinen Host, den du hast.

### Checkliste: frisches Netzwerk einrichten (Admin, einmalig)

1. **Gruppen anlegen:** `gpu` (GPU-Boxen), `praxis-stack` (VPS-Stack),
   `workstation` (dein Rechner).
2. **Policies anlegen** (Quelle → Ziel, tcp):
   - `gpu` → `praxis-stack`: **8080** — Call-home/Asset-Push der Agents
     (Router wählt sich ein, Boxen melden sich).
   - `praxis-stack` → `gpu`: **9100, 8188, 2700, 11434-11436** —
     Agent-Dial + Service-Proxy (ComfyUI/Vosk/llama-Ports der Boxen).
   - `workstation` → `praxis-stack`: **8080, 1337, 3537** — Router-,
     Praxis- und Gateway-Dashboard.
3. **PAT für den Router** (Setup Keys → API tokens): kommt als
   `NB_API_TOKEN` in die `.env` — der Router mintet damit pro GPU-Box
   einen ephemeren One-Off-Key (Gruppe `gpu`).
4. **Reusable Setup-Key** für den Peer `praxis-vps`
   (Gruppen: `praxis-stack`) → `NB_SETUP_KEY` in die `.env`.
5. **pgpu-Config** (`vps/config.toml`, `[netbird]`): `api_url` und
   `management_url` auf die neue Instanz setzen, `groups = ["gpu"]`
   (statt Gpuserver/servers) — die Boxen-enrollen dann ins neue Netz.
6. **Arbeitsreutzer:** netbird unterstützt mehrere parallele Netze —
   `netbird up --management-url https://netbird.<domain>` (separate Config,
   Service läuft zusätzlich zum tgrid-NetBird). Setup-Key für
   `workstation`-Gruppe separat minten.

NetBird-Management läuft dann auf dieser Instanz — der GPU-Stack
(`vps-compose.yml`) braucht nur `NB_MANAGEMENT_URL` + den Key aus Schritt 4.

## Einrichtung

### Variante A: Frischer Start (Standard — headless, kein Prompt)

Praxis fragt beim Start **nichts** ab. Der Erststart legt mit dem
zugestellten Master-Key selbst einen leeren verschlüsselten Store an;
die eigentlichen Secrets (Discord-Token, Provider-Keys) trägst du danach
über das **Dashboard (Settings-Tab)** ein — sie landen verschlüsselt im
Store und sind für die Agent-Shell unlesbar:

```sh
# 1. Checkout + Konfiguration
git clone ssh://git@forgejo.the.grid/Marvin/pgpu.git
cd pgpu/deploy
cp vps/.env.example .env
#    → NB_SETUP_KEY, ROUTER_TOKEN, VAST_API_KEY, NB_API_TOKEN eintragen
cp vps/config.example.toml vps/config.toml
chmod 600 .env

# 2. Secrets generieren (root: master_key wird root:root 0600)
sudo bash vps/gen-secrets.sh
#    → GATEWAY_API_KEY + DASHBOARD_ADMIN_PASSWORD in vps/praxis.env,
#      Master-Key in vps/master_key (Backup! sperrt den Secret-Store)
#    → GPU_ROUTER_TOKEN (= ROUTER_TOKEN) ergänzen
chmod 600 vps/praxis.env
mkdir -p vps/praxis-state

# 3. NetBird zuerst (IP steht erst nach Enroll fest)
docker compose -f vps-compose.yml up -d netbird
docker compose -f vps-compose.yml exec netbird netbird status --detail
#    → „NetBird IP: 100.105.x.y“ notieren (für eigene Notizen; Praxis
#      erreicht den Router über 127.0.0.1, egal welche IP der Peer hat)

# 4. Kompletter Stack
docker compose -f vps-compose.yml up -d

# 5. Secrets im Dashboard eintragen (einmalig):
#    http://100.105.x.y:1337 → Login DASHBOARD_ADMIN_PASSWORD → Settings:
#    DISCORD_BOT_TOKEN + Provider-Keys eintragen, dabei im Feld
#    „Master-Passwort“ den Inhalt von vps/master_key einfügen → speichert
#    verschlüsselt in secrets.enc2 (im praxis-state-Bind). Danach Discord
#    im Settings-Tab aktivieren.
```

**Ablauf des Master-Keys beim Start (zur Kontrolle):** Entrypoint (root)
liest `/run/secrets/master_key` → kopiert nach `/dev/shm` (0400) →
setpriv auf uid 1000 → Praxis liest+**löscht** die tmpfs-Datei →
`docker exec praxis-app sh -c 'ls -a /dev/shm; env | grep -i master'`
zeigt nach dem Start: nichts. Ein frischer Erststart loggt „leerer
verschlüsselter Secret-Store angelegt“.

Dashboard: `http://100.105.x.y:1337`, Gateway `:3537`, Router-Dashboard
`:8080` (Login ROUTER_TOKEN).

### Variante B: Migration von pagent (vorhandener State + Pairings)

Bestehende Installation übernehmen: State-Dateien ins `praxis-state`-Bind
legen — der verschlüsselte Store wird mit dem **alten** Master-Key
geöffnet (dafür `vps/master_key` mit eben diesem Inhalt erzeugen):

```sh
mkdir -p vps/praxis-state
# von pagent kopieren (scp über NetBird):
#   ~/release/data/          → nach dem ersten Start ins Volume (docker cp)
#   ~/release/contexts/     → dito
#   ~/release/secrets.enc2 ~/release/.secrets_salt → vps/praxis-state/
# Master-Key der alten Installation:
#   sudo sh -c 'printf %s "ALTER-MASTER-KEY" > vps/master_key && chmod 600 vps/master_key'
sudo chown -R 1000:1000 vps/praxis-state
```

Dann wie oben Schritt 3–4; Daten/`contexts` einmalig ins Volume kopieren:

```sh
docker compose -f vps-compose.yml up -d praxis
docker cp vps/praxis-state/data/. praxis-app:/opt/praxis/data/
docker cp vps/praxis-state/contexts/. praxis-app:/opt/praxis/contexts/
docker compose -f vps-compose.yml restart praxis
```

### Umstieg vom Heim-Deploy (gleicher Server!)

Der Stack kann **zuerst auf dem jetzigen Server** laufen (Extra-NetBird-
Peer `praxis-vps` nur fürs GPU-Netz) und später 1:1 auf den VPS umziehen —
Compose ist host-unabhängig. Beim Umstieg vom alten Host-Netz-Deploy auf
diesen Stack:

1. **Alten Router stoppen** (`docker compose down` im alten `pgpu/deploy`)
   und `VAST_API_KEY`/`NB_API_TOKEN`/`ROUTER_TOKEN` in die neue `.env`
   übernehmen — **niemals zwei Router mit demselben Vast-Key gleichzeitig
   laufen lassen** (beide würden dieselben Instanzen reconcilen und
   doppelt mieten).
2. Alte SQLite (`deploy/data/pgpu.sqlite`) ins neue `router-data`-Volume
   kopieren, wenn Metering-Historie/Host-Blacklist bleiben soll.
3. Praxis-URLs bleiben gleich (Router-Ports heißen im Namespace
   `127.0.0.1`), pagent kann danach abgebaut werden.

## Verifizieren

```sh
# Router: Dashboard vom Arbeitsplatz (NetBird-IP des VPS) — Login ROUTER_TOKEN
curl -s -H "Authorization: Bearer $ROUTER_TOKEN" http://100.105.x.y:8080/api/v1/state

# Praxis: Gateway vom Arbeitsplatz
curl -s http://100.105.x.y:3537/health

# Praxis → Router (Namespace-loopback, vom VPS aus):
docker compose -f vps-compose.yml exec praxis \
  curl -s -H "Authorization: Bearer $GPU_ROUTER_TOKEN" http://127.0.0.1:8080/api/v1/state
```

Erwartung: Router meldet beide Slots (kalt), Praxis erreicht den Router
über `GPU_ROUTER_URL=http://127.0.0.1:8080`. Ein Chat in Praxis wächst
dann über die GpuRouterClient-Kette (X-Router-Wait → impliziter Wake →
Proxy 200), s. pgpu-README „Praxis-Anbindung".

## Betrieb

- **Updates:** Images neu bauen/pushen ( jeweiliges Repo:
  `bash deploy/build.sh --push`), dann auf dem VPS `docker compose pull &&
  docker compose -f vps-compose.yml up -d`.
- **Backup:** `/data` des Routers (SQLite) + die Praxis-Volumes.
  ```sh
  docker run --rm -v praxis-vps_praxis-data:/d -v $PWD:/b alpine \
    tar czf /b/praxis-data-$(date +%F).tgz -C /d .
  ```
- **Logs:** `docker compose -f vps-compose.yml logs -f router|praxis|stt`.
- **Rollback:** vorheriges Image-Tag pinnen statt `:latest`.

## Stolperfallen

- **`/dev/net/tun` fehlt** (einige LXC/KVM-Hosts): `ls /dev/net/tun` muss
  existieren; sonst Provider fragen oder TUN aktivieren.
- **NetBird-IP ändert sich** nach Neu-Enrollment (gelöschtes Volume):
  Router-Fehlerbild „bind_ip=auto: keine NetBird-IP". `netbird-config`-
  Volume nicht löschen; Peer in tgrid behält seine IP bei gleichem Key.
- **`vps/master_key` verlieren** = Secret-Store unwiderruflich verloren
  (Argon2/AES). Die Datei außerhalb des Compose-Verzeichnisses sichern
  (Backup-Tresor). Sie muss root:root 0600 sein; wird sie als normaler
  User erzeugt, bleibt sie auf der Host-Ebene für uid 1000 lesbar → auf
  dem VPS `sudo bash vps/gen-secrets.sh` verwenden.
- **Zwei Router mit demselben Vast-Key** = Doppel-Miete/Chaos: beim
  Umstieg den alten Host-Router zuerst `docker compose down` fahren.
- **GATEWAY_API_KEY + Dashboard-Passwort** sind die einzigen
  Auth-Schichten FÜR Praxis selbst — echte Secrets verwenden (der alte
  Test-Wert `gateway_api_key_test` darf nie auf einen VPS).
- **Speicher:** STT lädt de+ja ≈ 6 GB RAM; mit `fr` dazu ≈ 8 GB. Bei
  kleineren VPS: `STT_LANGUAGES=de` und/oder Small-Models.
