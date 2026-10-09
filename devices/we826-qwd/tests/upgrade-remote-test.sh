#!/usr/bin/env bash
# Tests for the WE826 branch of `rundev.sh device upgrade-remote` (TRD-8),
# end to end against a fake unit: the function is taken from rundev.sh as it
# is, and runs with the helpers that talk to a device and the relay's mirror
# replaced by fakes. Nothing here reads the operator's device list or reaches
# a network: every URL is on an .invalid host and answered by the curl
# function below.
#
# What is real: do_device_upgrade_remote_we826 and we826_trusted_keys from
# rundev.sh, scripts/release-sign.sh (an ed25519 key made here, trusted through
# SCTL_TRUST_KEY), refresh-stage.sh, the exact prep and launch commands rundev
# sends (run under busybox sh with the stage paths moved into the unit's
# tree), and ramboot-refresh.sh on the unit (refresh-fixtures.sh).
# What is fake: the mirror and the device API (curl), the relay exec and file
# routes, and start-stop-daemon, which starts the script in the unit's
# environment.

set -u

HERE=$(cd -- "$(dirname -- "$0")" && pwd)
# shellcheck source=refresh-fixtures.sh
. "$HERE/refresh-fixtures.sh"
REPO_DIR=$(cd -- "$HERE/../../.." && pwd)
DASH=$(command -v dash) || { echo "dash is required (the launch shell of these tests)"; exit 1; }
DEVICE_URL=https://relay.invalid/d/WE826-TEST
API_KEY=unit-key

# rundev.sh's own pieces, as they are.
eval "$(sed -n '/^WE826_STAGE=/p; /^WE826_LOG=/p' "$REPO_DIR/rundev.sh")"
eval "$(sed -n '/^we826_trusted_keys() {/,/^}/p; /^do_device_upgrade_remote_we826() {/,/^}/p' "$REPO_DIR/rundev.sh")"
declare -F do_device_upgrade_remote_we826 >/dev/null || { echo "rundev.sh has no do_device_upgrade_remote_we826"; exit 1; }
log()  { echo "==> $*"; }
ok()   { echo "==> $*"; }
err()  { echo "==> $*" >&2; }
warn() { echo "==> $*" >&2; }
RELAY_REMOTE_CONFIG=/etc/sctl/relay.toml
CONFIG_FILE=$TOP/devices.json

# The release, signed with a key made here.
openssl genpkey -algorithm ed25519 -out "$TOP/release-key.pem" 2>/dev/null
cp "$FIX/release.json" "$FIX/mirror/release.json"
"$REPO_DIR/scripts/release-sign.sh" sign "$FIX/mirror/release.json" "$TOP/release-key.pem" >/dev/null
BENCH_KEY=$("$REPO_DIR/scripts/release-sign.sh" pubkey "$TOP/release-key.pem" | sed -n 's/^public_key_hex=//p')

# The shell the launch runs in: busybox, and a start-stop-daemon that starts
# the script the way the unit would, in the unit's environment.
LAUNCH_BIN=$FIX/launchbin
mkdir -p "$LAUNCH_BIN"
for applet in sh cat mkdir rm kill test '[' echo printf sleep tail; do
    ln -s "$(command -v busybox)" "$LAUNCH_BIN/$applet"
done
cat > "$LAUNCH_BIN/start-stop-daemon" <<'FAKE'
#!/bin/bash
PATH=/usr/bin:/bin
. "$E2E_ENV"
echo "start-stop-daemon $*" >> "$T/launch.log"
pidfile=
while [ $# -gt 0 ]; do
    case $1 in
        -p) pidfile=$2; shift 2 ;;
        --) shift; break ;;
        *) shift ;;
    esac
done
script=$1; shift
env -i PATH="$FIX/bin" HOME=/ REFRESH_ROOT="$R" REFRESH_INIT_CMD="$FIX/fake/rc" \
    REFRESH_POLL_SECS=1 REFRESH_PAUSE_SECS=0 REFRESH_FETCH_ATTEMPTS=2 REFRESH_FETCH_RETRY_SECS=0 \
    FAKE_ROOT="$R" FAKE_WORK="$T" FAKE_MIRROR="$FIX/mirror" FAKE_NEW_VERSION="$NEW_VERSION" \
    FAKE_NEW_HEALTH="$FAKE_NEW_HEALTH" "$UNIT_SH" "$script" "$@" </dev/null >/dev/null 2>&1 &
echo $! > "$pidfile"
FAKE
chmod 0755 "$LAUNCH_BIN/start-stop-daemon"

# --- the fakes rundev's function talks through ---------------------------------

# A device command, as the relay's exec route would run it on the unit: only
# the shapes the WE826 path sends, with the stage paths moved into the tree.
device_exec() {
    case $1 in
        'd=/tmp/sctl-we826-refresh; '*|'cat /tmp/sctl-we826-refresh/state'|'rm -rf /tmp/sctl-we826-refresh'|'tail -n '[0-9]*' /tmp/sctl-we826-refresh.log') ;;
        *) echo "$1" >> "$TOP/unexpected"; return 1 ;;
    esac
    [ -r "$T/agent" ] || return 7
    echo "$1" >> "$T/exec.log"
    # dash, not busybox: busybox ash would run its own start-stop-daemon
    # applet before the fake. REFRESH_ROOT keeps even a misrouted launch
    # inside the unit's tree.
    env -i PATH="$LAUNCH_BIN" E2E_ENV="$TOP/e2e.env" REFRESH_ROOT="$R" REFRESH_INIT_CMD="$FIX/fake/rc" \
        "$DASH" -c "${1//\/tmp\/sctl-we826-refresh/$R/tmp/sctl-we826-refresh}"
}

remote_exec_stdout_trimmed() {
    local out
    out=$(device_exec "$3" 2>/dev/null | tr -d '[:space:]') || true
    [ -n "$out" ] || return 1
    printf '%s' "$out"
}

remote_exec_json() {
    jq -n --arg out "$(device_exec "$3" 2>/dev/null)" '{stdout: $out}'
}

remote_put_file() {
    [ -r "$T/agent" ] || return 1
    case $4 in /tmp/sctl-we826-refresh/*) ;; *) echo "put $4" >> "$TOP/unexpected"; return 1 ;; esac
    mkdir -p "$R$(dirname "$4")" && cp "$3" "$R$4" && chmod "$5" "$R$4"
    echo "$4 $5" >> "$T/put.log"
}

health_json() {
    local v layout=null target=
    v=$(cat "$T/agent" 2>/dev/null) || return 7
    if [ "$v" = "$NEW_VERSION" ]; then
        target=mips_24kc
        [ ! -f "$R/etc/sctl/install.json" ] || layout='"ramboot"'
    fi
    [ -z "${E2E_LAYOUT:-}" ] || layout="\"$E2E_LAYOUT\""
    jq -cn --arg v "$v" --arg t "$target" --argjson l "$layout" \
        '{version: $v, layout: $l, tunnel: {connected: true}} + (if $t == "" then {} else {target: $t} end)'
}

curl() {
    local out= url= data=
    while [ $# -gt 0 ]; do
        case $1 in
            -o) out=$2; shift 2 ;;
            -d) data=$2; shift 2 ;;
            -H|-X|--max-time|--connect-timeout|-m) shift 2 ;;
            -*) shift ;;
            *) url=$1; shift ;;
        esac
    done
    case $url in
        "$NEW_MIRROR"/*)
            local f="$FIX/mirror/${url##*/}"
            [ -f "$f" ] || return 22
            if [ "${url##*/}" = "${E2E_BAD_NAME:-}" ]; then
                { cat "$f"; echo tampered; } > "${out:-/dev/stdout}"
            elif [ -n "$out" ]; then
                cp "$f" "$out"
            else
                cat "$f"
            fi
            ;;
        "$DEVICE_URL/api/health") health_json ;;
        "$DEVICE_URL/api/exec")
            local cmd
            cmd=$(jq -r '.command' <<< "$data")
            [ -r "$T/agent" ] || return 7
            jq -n --arg out "$(device_exec "$cmd" 2>/dev/null)" '{stdout: $out}'
            ;;
        *) echo "curl $url" >> "$TOP/unexpected"; return 7 ;;
    esac
}
ssh() { echo "ssh $*" >> "$TOP/unexpected"; return 255; }
sleep() { command sleep 1; }

# One run of the WE826 path against a fresh unit; the arguments are VAR=value
# settings for that run (FAKE_NEW_HEALTH for the unit, the rest for rundev).
upgrade() {
    local unit=$1 a health=ok
    shift
    new_unit "$unit"
    echo '{"devices": {"bus": {"sctl_version": "0.6.2.142"}}}' > "$CONFIG_FILE"
    : > "$T/exec.log"; : > "$T/put.log"; : > "$T/launch.log"
    for a in "$@"; do
        case $a in FAKE_NEW_HEALTH=*) health=${a#*=} ;; esac
    done
    cat > "$TOP/e2e.env" <<EOF
R='$R'
T='$T'
FIX='$FIX'
UNIT_SH='$UNIT_SH'
NEW_VERSION='$NEW_VERSION'
FAKE_NEW_HEALTH='$health'
EOF
    rm -f "$TOP/unexpected"
    (
        for a in "$@"; do export "${a?}"; done
        WE826_WAIT_SECS=4
        WE826_WATCH_SECS=60
        do_device_upgrade_remote_we826 bus "$DEVICE_URL" "$API_KEY" "${VERSION_ASKED:-$NEW_VERSION}" "$NEW_MIRROR"
    ) > "$T/out" 2>&1
    RC=$?
    snapshot "$T/after"
}
out() { tr '\n' '|' < "$T/out"; }
no_strays() { [ ! -e "$TOP/unexpected" ]; }

printf '\nrundev.sh upgrade-remote, WE826 branch\n'

# --- 1. the happy path ----------------------------------------------------------
upgrade e2e-happy FAKE_NEW_HEALTH=ok SCTL_TRUST_KEY="$BENCH_KEY"
check "exit 0 and the move reported complete" '[ "$RC" = 0 ] && grep -q "Move complete for '"'"'bus'"'"'" "$T/out"' "rc=$RC $(out)"
check "the unit runs the new version on the current layout" \
    '[ "$(running)" = "$NEW_VERSION" ] && grep -q helper_prefix "$R/etc/sctl/install.json"'
check "the device list records the version" '[ "$(jq -r .devices.bus.sctl_version "$CONFIG_FILE")" = "$NEW_VERSION" ]'
check "the six stage files were written with their modes" \
    '[ "$(cut -d" " -f2 "$T/put.log" | tr "\n" " ")" = "0755 0755 0600 0644 0755 0644 " ]' "$(tr '\n' ' ' < "$T/put.log")"
check "the launch went through start-stop-daemon with the version and the wait" \
    'grep -q -- "-S -b -m -p $R/tmp/sctl-we826-refresh/pid -x /bin/sh -- $R/tmp/sctl-we826-refresh/ramboot-refresh.sh $NEW_VERSION 4" "$T/launch.log"' \
    "$(cat "$T/launch.log")"
check "the stage directory is removed after done" '[ ! -e "$R/tmp/sctl-we826-refresh" ]'
check "nothing outside the known device commands and URLs" no_strays "$(cat "$TOP/unexpected" 2>/dev/null)"

# --- 2. the new agent never healthy --------------------------------------------
upgrade e2e-never FAKE_NEW_HEALTH=never SCTL_TRUST_KEY="$BENCH_KEY"
check "exit 1, rolled back, the log tail shown" \
    '[ "$RC" = 1 ] && grep -q "was not healthy: the old files are back" "$T/out" && grep -q "state: rolled_back" "$T/out" && grep -q "not healthy twice in a row" "$T/out"' "rc=$RC $(out)"
check "the unit runs the old version with its old files" '[ "$(running)" = "$OLD_VERSION" ] && unchanged' \
    "$(diff "$T/before" "$T/after" | tr '\n' '|')"
check "the device list is unchanged" '[ "$(jq -r .devices.bus.sctl_version "$CONFIG_FILE")" = "$OLD_VERSION" ]'

# --- 3. a manifest no trusted key signed ------------------------------------------
upgrade e2e-unsigned
check "refused before the device is touched" \
    '[ "$RC" = 1 ] && grep -q "is not signed by a key the agent trusts" "$T/out" && [ ! -s "$T/exec.log" ] && [ ! -s "$T/put.log" ] && unchanged' "rc=$RC $(out)"

# --- 4. the mirror serves a payload that is not the manifest's --------------------
upgrade e2e-mirror SCTL_TRUST_KEY="$BENCH_KEY" E2E_BAD_NAME=libc-mips_24kc.so.gz
check "refused before the device is touched" \
    '[ "$RC" = 1 ] && grep -q "does not serve libc-mips_24kc.so.gz" "$T/out" && [ ! -s "$T/put.log" ] && unchanged' "rc=$RC $(out)"

# --- 5. a manifest for another version ---------------------------------------------
upgrade e2e-version SCTL_TRUST_KEY="$BENCH_KEY" VERSION_ASKED=0.6.12.1
check "refused: the mirror's manifest is not the version asked for" \
    '[ "$RC" = 1 ] && grep -q "is version '"'"'$NEW_VERSION'"'"', not 0.6.12.1" "$T/out" && [ ! -s "$T/put.log" ]' "rc=$RC $(out)"

# --- 6. a unit whose install.json is read ---------------------------------------
upgrade e2e-managed SCTL_TRUST_KEY="$BENCH_KEY" E2E_LAYOUT=ramboot
check "refused: it takes managed upgrades" \
    '[ "$RC" = 1 ] && grep -q "takes managed upgrades" "$T/out" && [ ! -s "$T/put.log" ] && unchanged' "rc=$RC $(out)"

printf '\n  %d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
