#!/bin/sh
# Applies an upgrade that `rundev.sh device upgrade-remote` staged on a GL-XE300,
# and rolls it back unless the agent comes back healthy.
#
# Usage: upgrade.sh <expected_version> <off|follow_default>
#
# The stage directory (this script's own) holds server.gz, plugin.gz and
# relay-route.sh. The script restarts the agent whose exec launched it, so it
# is started detached (start-stop-daemon) and reports through <stage>/state,
# one word: started, installing, restarting (rolling_back), then done,
# rolled_back or failed. The log is /tmp/sctl-xe300-upgrade.log.
#
# Before it changes anything, the payloads, sctl.toml and (when the agent takes
# over the relay route) the route hotplugs and their pins are copied to
# /usr/local/lib/sctl/rollback, on the overlay, where they stay afterwards.
# Taking over the relay route also makes rp_filter loose (2), now and in
# /etc/sysctl.conf; a rollback leaves that as it is.
# Healthy means /api/health on the device answers with the expected version
# and, when sctl.toml has a [tunnel] section, a connected tunnel, twice in a
# row within WAIT_SECS of the restart.

exec </dev/null >>/tmp/sctl-xe300-upgrade.log 2>&1

VERSION=$1
MODE=$2
STAGE=$(cd "$(dirname "$0")" && pwd)
PAYLOAD_DIR=/usr/local/lib/sctl
SERVER=sctl-server-mips_24kc.gz
PLUGIN=sctl-comms-quectel-mips_24kc.so.gz
CONFIG=/etc/sctl/sctl.toml
BACKUP=$PAYLOAD_DIR/rollback
HEALTH=http://127.0.0.1:1337/api/health
WAIT_SECS=180

# The staged copies of sctl.toml hold the keys.
trap 'rm -f "$STAGE/sctl.toml" "$STAGE/sctl.toml.new"' EXIT

say() { echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) $*"; }
state() { echo "$1" > "$STAGE/state"; say "state: $1"; }

healthy() {
    body=$(curl -sf -m 5 "$HEALTH" 2>/dev/null) || return 1
    body=$(echo "$body" | tr -d ' ')
    case $body in *'"version":"'"$VERSION"'"'*) ;; *) return 1 ;; esac
    [ "$NEED_TUNNEL" = 1 ] || return 0
    case $body in *'"tunnel":{"connected":true'*) return 0 ;; esac
    return 1
}

rollback() {
    state rolling_back
    cp "$BACKUP/$SERVER" "$PAYLOAD_DIR/$SERVER"
    cp "$BACKUP/$PLUGIN" "$PAYLOAD_DIR/$PLUGIN"
    cp -p "$BACKUP/sctl.toml" "$CONFIG"
    [ "$OWN" = 1 ] && sh "$STAGE/relay-route.sh" restore "$BACKUP"
    sync
    /etc/init.d/sctl restart
    state rolled_back
    exit 1
}

case $MODE in off|follow_default) ;; *) MODE= ;; esac
if [ -z "$VERSION" ] || [ -z "$MODE" ]; then
    state failed
    say "usage: upgrade.sh <expected_version> <off|follow_default>"
    exit 1
fi
state started
say "upgrade to $VERSION, relay_route $MODE"
# Let the exec that launched this answer before the agent goes away.
sleep 3

rm -rf "$BACKUP.new"
if ! { mkdir -p "$BACKUP.new" &&
    cp -p "$PAYLOAD_DIR/$SERVER" "$PAYLOAD_DIR/$PLUGIN" "$BACKUP.new/" &&
    cp -p "$CONFIG" "$BACKUP.new/sctl.toml"; }; then
    rm -rf "$BACKUP.new"
    state failed
    exit 1
fi
rm -rf "$BACKUP" && mv "$BACKUP.new" "$BACKUP" || { state failed; exit 1; }

# sctl.toml: the relay_route key in [tunnel], everything else as it is.
NEED_TUNNEL=0
OWN=0
cp -p "$CONFIG" "$STAGE/sctl.toml" || { state failed; exit 1; }
if [ "$MODE" = off ]; then
    grep -q '^[[:space:]]*\[tunnel\]' "$CONFIG" && NEED_TUNNEL=1
else
    sh "$STAGE/relay-route.sh" config "$MODE" "$CONFIG" > "$STAGE/sctl.toml.new"
    rc=$?
    case $rc in
        0)
            mv -f "$STAGE/sctl.toml.new" "$STAGE/sctl.toml"
            NEED_TUNNEL=1
            OWN=1
            ;;
        3)
            say "no [tunnel] section: sctl.toml left as it is"
            ;;
        2)
            say "[tunnel] pins bind_address, which relay_route cannot be combined with"
            state failed
            exit 1
            ;;
        *)
            say "could not edit sctl.toml (exit $rc)"
            state failed
            exit 1
            ;;
    esac
fi

# Stage next to the targets first, so a full overlay fails before anything moves.
state installing
if ! { cp "$STAGE/server.gz" "$PAYLOAD_DIR/.$SERVER.new" &&
    cp "$STAGE/plugin.gz" "$PAYLOAD_DIR/.$PLUGIN.new" &&
    cp "$STAGE/sctl.toml" "$CONFIG.new"; }; then
    rm -f "$PAYLOAD_DIR/.$SERVER.new" "$PAYLOAD_DIR/.$PLUGIN.new" "$CONFIG.new"
    state failed
    exit 1
fi
chmod 0644 "$PAYLOAD_DIR/.$SERVER.new" "$PAYLOAD_DIR/.$PLUGIN.new"
chmod 0600 "$CONFIG.new"
mv -f "$PAYLOAD_DIR/.$SERVER.new" "$PAYLOAD_DIR/$SERVER" &&
    mv -f "$PAYLOAD_DIR/.$PLUGIN.new" "$PAYLOAD_DIR/$PLUGIN" &&
    mv -f "$CONFIG.new" "$CONFIG" || rollback
# The install layout (docs/upgrade.md): from here on the agent upgrades itself.
printf '{"v":1,"layout":"gz-tmp","target":"mips_24kc"}\n' > /etc/sctl/install.json
if [ "$OWN" = 1 ]; then
    sh "$STAGE/relay-route.sh" adopt "$BACKUP" || rollback
    # Loose rp_filter, now and in /etc/sysctl.conf, lets the answer to the
    # agent's probe of an uplink the route does not use through. Not undone
    # by a rollback: it is harmless to any agent, and bind_address needs it
    # too.
    sh "$STAGE/relay-route.sh" rp-filter ||
        say "could not make rp_filter loose; a route that failed over may not come back"
fi
# The init script needs 16 MB free in /tmp to expand the payloads.
rm -f "$STAGE/server.gz" "$STAGE/plugin.gz" "$STAGE/sctl.toml"
sync

state restarting
/etc/init.d/sctl restart

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
say "healthy ${waited}s after the restart; the previous version stays in $BACKUP"
state done
