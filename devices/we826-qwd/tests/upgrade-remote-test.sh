#!/usr/bin/env bash
# Tests for the WE826 branch of `rundev.sh device upgrade-remote` (TRD-8),
# end to end against a fake unit: the function is taken from rundev.sh as it
# is, and runs with the helpers that talk to a device and the relay's mirror
# replaced by fakes. Nothing here reads the operator's device list or reaches
# a network: every URL is on 192.0.2.10 (TEST-NET-1) or an .invalid host and
# answered by the curl function below.
#
# What is real: do_device_upgrade_remote_we826 and we826_trusted_keys from
# rundev.sh, scripts/release-sign.sh (an ed25519 key made here, trusted through
# SCTL_TRUST_KEY), refresh-stage.sh, the exact prep, launch and read commands
# rundev sends (run under dash, whose PATH has no start-stop-daemon applet to
# shadow the fake, with the unit's paths moved into its tree),
# ramboot-refresh.sh on the unit and the unit's init files and ramboot.sh
# (refresh-fixtures.sh). Also XE300_LAYOUT_PROBE, the dispatcher's question,
# under busybox against fake trees of every layout.
# What is fake: the mirror and the device API (curl), the relay exec and file
# routes, and start-stop-daemon, which starts the script in the unit's
# environment.

set -u

HERE=$(cd -- "$(dirname -- "$0")" && pwd)
# shellcheck source=refresh-fixtures.sh
. "$HERE/refresh-fixtures.sh"
DASH=$(command -v dash)
DEVICE_URL=https://relay.invalid/d/WE826-TEST
API_KEY=unit-key

# rundev.sh's own pieces, as they are.
eval "$(sed -n '/^WE826_STAGE=/p; /^WE826_LOG=/p; /^XE300_LAYOUT_PROBE=/p' "$REPO_DIR/rundev.sh")"
eval "$(sed -n '/^we826_trusted_keys() {/,/^}/p; /^we826_url_host() {/,/^}/p; /^do_device_upgrade_remote_we826() {/,/^}/p' "$REPO_DIR/rundev.sh")"
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
BENCH_KEY_ID=$("$REPO_DIR/scripts/release-sign.sh" pubkey "$TOP/release-key.pem" | sed -n 's/^key_id=//p')

# The shell the launch runs in: busybox applets, and a start-stop-daemon that
# starts the script the way the unit would, in the unit's environment.
LAUNCH_BIN=$FIX/launchbin
mkdir -p "$LAUNCH_BIN"
for applet in sh cat mkdir rm kill test '[' echo printf sleep tail sed; do
    ln -s "$(command -v busybox)" "$LAUNCH_BIN/$applet"
done
cat > "$LAUNCH_BIN/start-stop-daemon" <<'FAKE'
#!/bin/bash
PATH=/usr/bin:/bin
mapfile -t envs < "$E2E_ENV"
work=$(sed -n 's/^FAKE_WORK=//p' "$E2E_ENV")
sh=$(sed -n 's/^FAKE_SH=//p' "$E2E_ENV")
echo "start-stop-daemon $*" >> "$work/launch.log"
pidfile=
while [ $# -gt 0 ]; do
    case $1 in
        -p) pidfile=$2; shift 2 ;;
        --) shift; break ;;
        *) shift ;;
    esac
done
script=$1; shift
env -i "${envs[@]}" "$sh" "$script" "$@" </dev/null >/dev/null 2>&1 &
echo $! > "$pidfile"
FAKE
chmod 0755 "$LAUNCH_BIN/start-stop-daemon"

# --- the fakes rundev's function talks through ---------------------------------

# A device command, as the relay's exec route would run it on the unit: only
# the shapes the WE826 path sends, with the unit's paths moved into the tree.
device_exec() {
    case $1 in
        'd=/tmp/sctl-we826-refresh; '*|'cat /tmp/sctl-we826-refresh/state 2>/dev/null || echo gone'|'rm -rf /tmp/sctl-we826-refresh'|'tail -n '[0-9]*' /tmp/sctl-we826-refresh.log') ;;
        "sed -n 's/^SERVER_URL=//p' /etc/sctl/ramboot.conf") ;;
        *) echo "$1" >> "$TOP/unexpected"; return 1 ;;
    esac
    running >/dev/null || return 7
    echo "$1" >> "$T/exec.log"
    local cmd=${1//\/tmp\/sctl-we826-refresh/$R/tmp/sctl-we826-refresh}
    cmd=${cmd//\/etc\/sctl\/ramboot.conf/$R/etc/sctl/ramboot.conf}
    # dash, not busybox: busybox ash would run its own start-stop-daemon
    # applet before the fake. REFRESH_ROOT keeps even a misrouted launch
    # inside the unit's tree.
    env -i PATH="$LAUNCH_BIN" E2E_ENV="$TOP/e2e.env" REFRESH_ROOT="$R" "$DASH" -c "$cmd"
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
    running >/dev/null || return 1
    case $4 in /tmp/sctl-we826-refresh/*) ;; *) echo "put $4" >> "$TOP/unexpected"; return 1 ;; esac
    mkdir -p "$R$(dirname "$4")" && cp "$3" "$R$4" && chmod "$5" "$R$4"
    echo "$4 $5" >> "$T/put.log"
}

# /api/health as rundev reads it, from the agent the unit runs.
health_json() {
    local v layout target=
    v=$(running) || return 7
    layout=$(agent_field 3)
    [ "$v" != "$NEW_VERSION" ] || target=mips_24kc
    [ -z "${E2E_LAYOUT:-}" ] || layout=$E2E_LAYOUT
    jq -cn --arg v "$v" --arg t "$target" --arg l "$layout" \
        '{version: $v, layout: (if $l == "none" then null else $l end), tunnel: {connected: true}} + (if $t == "" then {} else {target: $t} end)'
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
        "$NEW_MIRROR"/*|"$NAMED_MIRROR"/*)
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
            running >/dev/null || return 7
            jq -n --arg out "$(device_exec "$cmd" 2>/dev/null)" '{stdout: $out}'
            ;;
        *) echo "curl $url" >> "$TOP/unexpected"; return 7 ;;
    esac
}
ssh() { echo "ssh $*" >> "$TOP/unexpected"; return 255; }
sleep() { command sleep 1; }

# One run of the WE826 path against a fresh unit. The arguments are VAR=value
# settings: FAKE_* and REFRESH_* for the unit, the rest for rundev. PREP is
# run on the unit before it boots.
upgrade() {
    local unit=$1 a
    shift
    new_unit "$unit"
    eval "${PREP:-:}"
    boot_unit
    echo '{"devices": {"bus": {"sctl_version": "0.6.2.142"}}}' > "$CONFIG_FILE"
    : > "$T/exec.log"; : > "$T/put.log"; : > "$T/launch.log"
    unit_env > "$TOP/e2e.env"
    for a in "$@"; do
        case $a in FAKE_*=*|REFRESH_*=*) echo "$a" >> "$TOP/e2e.env" ;; esac
    done
    rm -f "$TOP/unexpected"
    (
        for a in "$@"; do
            case $a in FAKE_*=*|REFRESH_*=*) ;; *) export "${a?}" ;; esac
        done
        WE826_WAIT_SECS=4
        WE826_WATCH_SECS=120
        do_device_upgrade_remote_we826 bus "$DEVICE_URL" "$API_KEY" "${VERSION_ASKED:-$NEW_VERSION}" "${SOURCE:-$NEW_MIRROR}"
    ) > "$T/out" 2>&1
    RC=$?
    snapshot "$T/after"
}
out() { tr '\n' '|' < "$T/out"; }
no_strays() { [ ! -e "$TOP/unexpected" ]; }

printf '\nrundev.sh upgrade-remote, WE826 branch\n'

# --- 1. the happy path ----------------------------------------------------------
upgrade e2e-happy SCTL_TRUST_KEY="$BENCH_KEY"
check "exit 0 and the move reported complete" '[ "$RC" = 0 ] && grep -q "Move complete for '"'"'bus'"'"'" "$T/out"' "rc=$RC $(out)"
check "the release verified with the bench key, named by its key id" 'grep -q "verifies (key id $BENCH_KEY_ID)" "$T/out"' "$(out)"
check "the unit runs the new version on the current layout, from flash" \
    '[ "$(running)" = "$NEW_VERSION" ] && [ "$(agent_field 4)" = flash ] && grep -q helper_prefix "$R/etc/sctl/install.json"'
check "the device list records the version" '[ "$(jq -r .devices.bus.sctl_version "$CONFIG_FILE")" = "$NEW_VERSION" ]'
check "the seven stage files were written with their modes" \
    '[ "$(cut -d" " -f2 "$T/put.log" | tr "\n" " ")" = "0755 0755 0600 0644 0644 0755 0644 " ]' "$(tr '\n' ' ' < "$T/put.log")"
check "the launch went through start-stop-daemon with the version, the wait and the pre-move time" \
    'grep -q -- "-S -b -m -p $R/tmp/sctl-we826-refresh/pid -x /bin/sh -- $R/tmp/sctl-we826-refresh/ramboot-refresh.sh $NEW_VERSION 4 1800" "$T/launch.log"' \
    "$(cat "$T/launch.log")"
check "the unit's mirror host was read before staging" 'grep -q "fetches from 192.0.2.10; the new mirror is 192.0.2.10" "$T/out"' "$(out)"
check "the stage directory is removed after done" '[ ! -e "$R/tmp/sctl-we826-refresh" ]'
check "nothing outside the known device commands and URLs" no_strays "$(cat "$TOP/unexpected" 2>/dev/null)"

# --- 2. the new agent never healthy --------------------------------------------
upgrade e2e-never SCTL_TRUST_KEY="$BENCH_KEY" FAKE_NEW_HEALTH=never
check "exit 1, rolled back, the log tail shown" \
    '[ "$RC" = 1 ] && grep -q "was not healthy: the old files are back" "$T/out" && grep -q "state: rolled_back" "$T/out" && grep -q "not healthy for" "$T/out"' "rc=$RC $(out)"
check "the unit runs the old version with its old files" '[ "$(running)" = "$OLD_VERSION" ] && unchanged' \
    "$(diff "$T/before" "$T/after" | tr '\n' '|')"
check "the device list is unchanged" '[ "$(jq -r .devices.bus.sctl_version "$CONFIG_FILE")" = "$OLD_VERSION" ]'

# --- 3. a manifest no trusted key signed ------------------------------------------
upgrade e2e-unsigned
check "unsigned: refused before the device is touched" \
    '[ "$RC" = 1 ] && grep -q "is not signed by a key the agent trusts" "$T/out" && [ ! -s "$T/exec.log" ] && [ ! -s "$T/put.log" ] && unchanged' "rc=$RC $(out)"

# --- 4. the mirror serves a payload that is not the manifest's --------------------
upgrade e2e-mirror SCTL_TRUST_KEY="$BENCH_KEY" E2E_BAD_NAME=libc-mips_24kc.so.gz
check "a wrong payload on the mirror: refused before the device is touched" \
    '[ "$RC" = 1 ] && grep -q "does not serve libc-mips_24kc.so.gz" "$T/out" && [ ! -s "$T/put.log" ] && unchanged' "rc=$RC $(out)"

# --- 5. a manifest for another version ---------------------------------------------
upgrade e2e-version SCTL_TRUST_KEY="$BENCH_KEY" VERSION_ASKED=0.6.12.1
check "refused: the mirror's manifest is not the version asked for" \
    '[ "$RC" = 1 ] && grep -q "is version '"'"'$NEW_VERSION'"'"', not 0.6.12.1" "$T/out" && [ ! -s "$T/put.log" ]' "rc=$RC $(out)"

# --- 6. a unit on another layout ------------------------------------------------------
upgrade e2e-gztmp SCTL_TRUST_KEY="$BENCH_KEY" E2E_LAYOUT=gz-tmp
check "a unit reporting layout gz-tmp: refused before anything is staged" \
    '[ "$RC" = 1 ] && grep -q "reports layout gz-tmp, not ramboot" "$T/out" && [ ! -s "$T/put.log" ] && unchanged' "rc=$RC $(out)"

# --- 7. a unit installed by 0.6.7 or 0.6.8 ----------------------------------------------
PREP='printf "{\"v\":1,\"layout\":\"ramboot\",\"target\":\"mips_24kc\"}\n" > "$R/etc/sctl/install.json"' \
    upgrade e2e-067 SCTL_TRUST_KEY="$BENCH_KEY"
check "layout ramboot without helper_prefix (a 0.6.7-era install) takes the move: done" \
    '[ "$RC" = 0 ] && grep -q "layout ramboot)" "$T/out" && grep -q helper_prefix "$R/etc/sctl/install.json" && [ "$(running)" = "$NEW_VERSION" ]' "rc=$RC $(out)"

# --- 8. a mirror by name for a unit that fetches by IP ------------------------------------
SOURCE=$NAMED_MIRROR upgrade e2e-named SCTL_TRUST_KEY="$BENCH_KEY"
check "a mirror by name: refused before anything is staged, with the by-IP command" \
    '[ "$RC" = 1 ] && grep -q "Pass the mirror by IP: .* http://192.0.2.10:8081/artifacts/$NEW_VERSION" "$T/out" && [ ! -s "$T/put.log" ] && unchanged' "rc=$RC $(out)"

# --- 9. the old agent does not come back: the unit reboots --------------------------------
upgrade e2e-reboot SCTL_TRUST_KEY="$BENCH_KEY" FAKE_NEW_HEALTH=never FAKE_OLD_TUNNEL=down_after_move
check "rundev reports the reboot into the old layout and the version that answers" \
    '[ "$RC" = 1 ] && grep -q "state: rebooted" "$T/out" && grep -q "rebooted into the old layout" "$T/out" && grep -q "answers again with version '"'"'$OLD_VERSION'"'"'" "$T/out"' "rc=$RC $(out)"
check "flash holds the old files" 'grep " etc/" "$T/after" | cmp -s - <(grep " etc/" "$T/before")'

# --- the dispatcher's question, against every layout -----------------------------------------
printf '\nXE300_LAYOUT_PROBE\n'
probe_tree() {
    local tree=$TOP/probe-$1
    mkdir -p "$tree/etc/init.d" "$tree/etc/sctl" "$tree/usr/local/lib/sctl" "$tree/tmp/sctl/lib"
    : > "$tree/etc/init.d/sctl"
    chmod 0755 "$tree/etc/init.d/sctl"
    eval "$2"
    local p=${XE300_LAYOUT_PROBE//\/etc\//$tree/etc/}
    p=${p//\/usr\//$tree/usr/}
    p=${p//\/tmp\/sctl\//$tree/tmp/sctl/}
    busybox sh -c "$p"
}
probe_case() {
    local got want=$3
    got=$(probe_tree "$1" "$2")
    check "$1 answers $want" '[ "$got" = "$want" ]' "got '$got'"
}
probe_case xe300 ': > "$tree/usr/local/lib/sctl/sctl-server-mips_24kc.gz"' xe300
probe_case rut241-gz-tmp ': > "$tree/usr/local/lib/sctl/sctl-server-mipsel_24kc.gz"' rut241
probe_case rut241-ramboot 'printf "SERVER_URL=x\n" > "$tree/etc/sctl/ramboot.conf"' other
probe_case bpi 'mkdir -p "$tree/usr/bin" && : > "$tree/usr/bin/sctl"' other
probe_case no-init 'rm -f "$tree/etc/init.d/sctl"' other
probe_case we826-0.6.2 'printf "MUSL_LIBC_URL=x\n" > "$tree/etc/sctl/ramboot.conf"; : > "$tree/tmp/sctl/lib/libc.so"' we826
probe_case we826-0.6.7 'printf "MUSL_LIBC_URL=x\n" > "$tree/etc/sctl/ramboot.conf"; : > "$tree/tmp/sctl/lib/libc.so"; echo "{\"layout\":\"ramboot\"}" > "$tree/etc/sctl/install.json"' we826
probe_case we826-current 'printf "MUSL_LIBC_URL=x\n" > "$tree/etc/sctl/ramboot.conf"; : > "$tree/tmp/sctl/lib/libc.so"; echo "{\"helper_prefix\":[]}" > "$tree/etc/sctl/install.json"' we826-managed
check "the probe names no path the trees did not replace" \
    '! printf "%s" "$XE300_LAYOUT_PROBE" | grep -o "/[a-z][a-z/._-]*" | grep -v "^/etc/\|^/usr/\|^/tmp/sctl/\|^/dev/null" | grep -q .'

printf '\n  %d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
