#!/bin/sh
# NETBIRD_BIN-Wrapper für die 2. (jfinx-)NetBird-Instanz im HOST-Netzwerk:
#   * `up` bekommt --interface-name wt1 — wt0 gehört der nativen tgrid-Instanz
#     im selben Netz-Namespace (Host-Netz-Container!).
#   * --wireguard-port 51821 — die native Instanz lauscht auf 51820.
#   * --disable-dns — DNS (resolv.conf/FQDNs) verwaltet weiter die native
#     tgrid-Instanz; die jfinx-Instanz ist reiner IP-Zugang.
# Alle anderen Aufrufe (service run, status, …) gehen unverändert durch.
if [ "$1" = "up" ]; then
  shift
  exec /usr/local/bin/netbird up --interface-name wt1 --disable-dns --wireguard-port 51821 "$@"
fi
exec /usr/local/bin/netbird "$@"
