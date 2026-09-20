#!/usr/bin/env bash
# Erststart-Helfer: erzeugt starke Secrets für den Praxis-VPS-Stack und
# schreibt sie in die Env-Dateien. Idempotent — vorhandene Werte bleiben.
#
#   bash vps/gen-secrets.sh            # schreibt vps/praxis.env aus .example
#   bash vps/gen-secrets.sh --force    # überschreibt Platzhalter erneut
#
# Erzeugt/ersetzt nur Werte, die noch auf dem Beispiel-Platzhalter stehen
# ("bitte-durch-ein-echtes-…" bzw. leer) oder fehlen.
set -euo pipefail
cd "$(dirname "$0")"

ENV_FILE="praxis.env"
EXAMPLE="praxis.env.example"
MASTER_KEY_FILE="master_key"
FORCE="${1:-}"

[ -f "$ENV_FILE" ] || cp "$EXAMPLE" "$ENV_FILE"
[ -f "$MASTER_KEY_FILE" ] || {
    umask 077
    openssl rand -base64 48 | tr -d '\n/=+' | cut -c1-64 > "$MASTER_KEY_FILE"
    chmod 600 "$MASTER_KEY_FILE"
    echo "master_key erzeugt (sperrt den Secret-Store; Backups von vps/ enthalten ihn — sicher aufbewahren!)."
}
# Root-Besitz, wenn möglich: Nur der Root-Entrypoint des Containers darf
# den Key lesen — die Agent-Shell (uid 1000) nicht.
chown root:root "$MASTER_KEY_FILE" 2>/dev/null \
    && chmod 600 "$MASTER_KEY_FILE" \
    && echo "master_key: root:root 0600 (empfohlen)" \
    || echo "WARNUNG: kein root — master_key für jeden lokalen User lesbar; auf dem VPS als root ausführen!"
gen() { openssl rand -base64 32 | tr -d '\n/=+' | cut -c1-40; }

set_kv() {
    local key="$1" value="$2" file="$3"
    if grep -qE "^${key}=" "$file"; then
        sed -i -E "s|^${key}=.*$|${key}=${value}|" "$file"
    else
        printf '%s=%s\n' "$key" "$value" >> "$file"
    fi
}

needs() {
    local key="$1" file="$2"
    [ "$FORCE" = "--force" ] && return 0
    local current
    current=$(grep -E "^${key}=" "$file" | head -1 | cut -d= -f2-)
    # Beispiel-Platzhalter oder leer → neu generieren
    [[ "$current" == *bitte-durch* || -z "$current" ]] && return 0
    return 1
}

if needs GATEWAY_API_KEY "$ENV_FILE"; then
    set_kv GATEWAY_API_KEY "$(gen)" "$ENV_FILE"
    echo "GATEWAY_API_KEY erzeugt."
fi
if needs DASHBOARD_ADMIN_PASSWORD "$ENV_FILE"; then
    set_kv DASHBOARD_ADMIN_PASSWORD "$(gen)" "$ENV_FILE"
    echo "DASHBOARD_ADMIN_PASSWORD erzeugt."
fi

chmod 600 "$ENV_FILE"
echo "→ $ENV_FILE ist startklar. Noch ausfüllen:"
echo "   GPU_ROUTER_TOKEN (= ROUTER_TOKEN aus deploy/.env),"
echo "   DASHBOARD-Login erfolgt über DASHBOARD_ADMIN_PASSWORD (oben erzeugt)."
echo "   Externe Secrets (Discord/Provider) NIE hier — über das Praxis-Dashboard"
echo "   (Settings) eintragen; sie landen verschlüsselt im Store (MASTER_KEY ="
echo "   Inhalt von vps/master_key)."
echo "   Danach: docker compose -f ../vps-compose.yml up -d"
