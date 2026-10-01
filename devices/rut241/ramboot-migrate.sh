#!/bin/sh
# Moves a RUT241 from the gz-tmp layout (payloads on its 4 MB overlay) to the
# ramboot layout (payloads fetched into /tmp at boot from the relay's mirror,
# docs/upgrade.md), and puts the old layout back unless the agent comes back
# healthy. `rundev.sh device upgrade-remote` stages it; from then on the agent
# upgrades itself by rewriting ramboot.conf.
#
# Usage: ramboot-migrate.sh <expected_version>
#
# The stage directory (this script's own) holds sctl.init (the shared ramboot
# init), ramboot.sh, ramboot.conf (the mirror URLs and SHA-256s of the
# version) and install.json. The script restarts the agent whose exec
# launched it, so it is started detached and reports through <stage>/state,
# one word: started, fetching, installing, restarting (rolling_back), then
# done, rolled_back or failed. The log is /tmp/sctl-rut241-ramboot.log.
#
# Order, so that nothing on flash moves before the new agent is in RAM:
#   1. both payloads are fetched into the ramboot cache and verified, so the
#      first ramboot start reads them from cache and does not depend on the
#      network at the moment of the swap;
#   2. the current init, install.json and the gz payloads are copied to a
#      backup in /tmp (the overlay has no room for one);
#   3. the ramboot files are written, the gz payloads removed (that is what
#      frees the overlay), the agent restarted once;
#   4. healthy means /api/health answers with the expected version and a
#      connected tunnel twice in a row within WAIT_SECS; otherwise the old
#      init and payloads go back and the agent is restarted again.
# A rollback leaves the overlay as it was: the payloads removed in 3 fit back
# in the space they freed.

exec </dev/null >>/tmp/sctl-rut241-ramboot.log 2>&1

VERSION=$1
STAGE=$(cd "$(dirname "$0")" && pwd)
PAYLOAD_DIR=/usr/local/lib/sctl
SERVER=sctl-server-mipsel_24kc.gz
PLUGIN=sctl-comms-quectel-mipsel_24kc.so.gz
INIT=/etc/init.d/sctl
CONF_DIR=/etc/sctl
RUN_DIR=/tmp/sctl
CACHE_DIR=$RUN_DIR/cache
BACKUP=/tmp/sctl-rut241-rollback
HEALTH=http://127.0.0.1:1337/api/health
WAIT_SECS=240

say() { echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) $*"; }
state() { echo "$1" > "$STAGE/state"; say "state: $1"; }

healthy() {
    body=$(curl -sf -m 5 "$HEALTH" 2>/dev/null) || return 1
    body=$(echo "$body" | tr -d ' ')
    case $body in *'"version":"'"$VERSION"'"'*) ;; *) return 1 ;; esac
    case $body in *'"tunnel":{"connected":true'*) return 0 ;; esac
    return 1
}

sha256_of() { sha256sum "$1" | cut -d' ' -f1; }

# One payload into the cache, verified against ramboot.conf's SHA-256.
fetch() {
    url=$1; expected=$2; dst=$3; label=$4
    if [ -r "$dst" ] && [ "$(sha256_of "$dst")" = "$expected" ]; then
        say "$label already cached"
        return 0
    fi
    rm -f "$dst.tmp"
    attempt=1
    while [ "$attempt" -le 5 ]; do
        if curl -fL --connect-timeout 10 --max-time 180 -o "$dst.tmp" "$url"; then
            if [ "$(sha256_of "$dst.tmp")" = "$expected" ]; then
                mv -f "$dst.tmp" "$dst"
                say "$label fetched and verified"
                return 0
            fi
            say "$label SHA-256 mismatch, attempt $attempt"
        else
            say "$label download failed, attempt $attempt"
        fi
        rm -f "$dst.tmp"
        attempt=$((attempt + 1))
        sleep 10
    done
    return 1
}

rollback() {
    state rolling_back
    cp -p "$BACKUP/sctl.init" "$INIT"
    cp -p "$BACKUP/$SERVER" "$PAYLOAD_DIR/$SERVER"
    cp -p "$BACKUP/$PLUGIN" "$PAYLOAD_DIR/$PLUGIN"
    if [ -f "$BACKUP/install.json" ]; then
        cp -p "$BACKUP/install.json" "$CONF_DIR/install.json"
    else
        rm -f "$CONF_DIR/install.json"
    fi
    rm -f "$CONF_DIR/ramboot.sh" "$CONF_DIR/ramboot.conf"
    sync
    "$INIT" restart
    state rolled_back
    exit 1
}

if [ -z "$VERSION" ]; then
    state failed
    say "usage: ramboot-migrate.sh <expected_version>"
    exit 1
fi
for f in sctl.init ramboot.sh ramboot.conf install.json; do
    [ -r "$STAGE/$f" ] || { state failed; say "missing $STAGE/$f"; exit 1; }
done
state started
say "migration to ramboot, version $VERSION"
# Let the exec that launched this answer before the agent goes away.
sleep 3

# 1. The payloads, into the cache the ramboot init reads first.
state fetching
# Only the four values this script needs: ramboot.conf also sets PLUGIN, BIN
# and RUN_DIR, which are this script's own names for other things.
eval "$(sed -n "s/^\(SERVER_URL\|SERVER_SHA256\|PLUGIN_URL\|PLUGIN_SHA256\)=/CONF_\1=/p" "$STAGE/ramboot.conf")"
[ -n "${CONF_SERVER_URL:-}" ] && [ -n "${CONF_SERVER_SHA256:-}" ] && [ -n "${CONF_PLUGIN_URL:-}" ] && [ -n "${CONF_PLUGIN_SHA256:-}" ] || { state failed; say "ramboot.conf names no server and plugin URL and SHA-256"; exit 1; }
mkdir -p "$CACHE_DIR"
fetch "$CONF_SERVER_URL" "$CONF_SERVER_SHA256" "$CACHE_DIR/sctl-server.payload" "server" || { state failed; exit 1; }
fetch "$CONF_PLUGIN_URL" "$CONF_PLUGIN_SHA256" "$CACHE_DIR/sctl-comms.payload" "plugin" || { state failed; exit 1; }
# The cached server must run here before anything on flash moves.
gzip -dc "$CACHE_DIR/sctl-server.payload" > "$RUN_DIR/sctl-server.probe" || { state failed; exit 1; }
chmod 0755 "$RUN_DIR/sctl-server.probe"
probe=$("$RUN_DIR/sctl-server.probe" --version 2>/dev/null)
rm -f "$RUN_DIR/sctl-server.probe"
case $probe in *"$VERSION"*) say "cached server runs: $probe" ;; *) state failed; say "cached server does not run or is not $VERSION: '$probe'"; exit 1 ;; esac

# 2. The backup, in RAM.
rm -rf "$BACKUP"
if ! { mkdir -p "$BACKUP" &&
    cp -p "$INIT" "$BACKUP/sctl.init" &&
    cp -p "$PAYLOAD_DIR/$SERVER" "$PAYLOAD_DIR/$PLUGIN" "$BACKUP/"; }; then
    rm -rf "$BACKUP"
    state failed
    exit 1
fi
[ ! -f "$CONF_DIR/install.json" ] || cp -p "$CONF_DIR/install.json" "$BACKUP/install.json"

# 3. The ramboot layout in place of the gz payloads.
state installing
if ! { cp "$STAGE/ramboot.sh" "$CONF_DIR/ramboot.sh" &&
    cp "$STAGE/ramboot.conf" "$CONF_DIR/ramboot.conf" &&
    cp "$STAGE/install.json" "$CONF_DIR/install.json" &&
    cp "$STAGE/sctl.init" "$INIT"; }; then
    say "could not write the ramboot files"
    rollback
fi
chmod 0755 "$CONF_DIR/ramboot.sh" "$INIT"
chmod 0600 "$CONF_DIR/ramboot.conf"
chmod 0644 "$CONF_DIR/install.json"
rm -f "$PAYLOAD_DIR/$SERVER" "$PAYLOAD_DIR/$PLUGIN"
sync

state restarting
"$INIT" restart

good=0
waited=0
while [ "$waited" -lt "$WAIT_SECS" ]; do
    sleep 5
    waited=$((waited + 5))
    if healthy; then
        good=$((good + 1))
        [ "$good" -ge 2 ] && break
    else
        good=0
    fi
done
if [ "$good" -lt 2 ]; then
    say "not healthy ${WAIT_SECS}s after the restart"
    rollback
fi
say "healthy ${waited}s after the restart; the old layout stays in $BACKUP until the next boot"
state done
