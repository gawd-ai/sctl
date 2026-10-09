# Shared by ramboot-refresh-test.sh and upgrade-remote-test.sh: a WE826 as a
# directory tree, the fakes it runs with, and the release it moves to.
# Sourced; the caller sets HERE (its own directory) and `set -u`.
#
# The script runs for real under BusyBox ash (and dash), with a PATH that
# holds what the WE826's stock firmware has (busybox applets, openssl, curl)
# and not what it lacks (sha256sum, setsid, mktemp). The unit is a directory
# tree (REFRESH_ROOT): its files on "flash" (etc/), payloads in the RAM cache,
# the agent running.
#
# What runs for real on the unit: the init files and ramboot.sh as released
# (taken from git: Bus 01 carries c97ad7a's init and the 05-31 ramboot.sh),
# so the supervisor, its pid files, its signal handling, SCTL_RAMBOOT_CONFIG
# and the old ramboot.sh's /tmp check at every start are the real ones.
# What is fake:
#   rc        /etc/rc.common: sources the init file and runs the action, with the
#             init's absolute paths moved into the tree and its respawn delay
#             1 s; no procd, as on the unit
#   wrapper   the init's WRAPPER: /etc/sctl/ramboot.sh as it is on "flash"
#             at that moment, its absolute paths moved into the tree
#             (tree.sed, treeconf), run by the unit's shell
#   df        /tmp is a tmpfs of FAKE_TMP_KB (what the tree's tmp/ holds is
#             used), the overlay has FAKE_FLASH_KB free
#   curl      the mirror (payload downloads by name; ramboot.sh's own
#             downloads, `curl -fLk`, are logged apart: net.log) and
#             /api/health for the running agent (answers only while its pid
#             lives); FAKE_NEW_HEALTH shapes the new version's answers,
#             FAKE_OLD_TUNNEL the old one's tunnel
#   reboot    a cold boot: the agent killed, the tree's tmp/ emptied, the
#             init started again from whatever flash holds
#   payloads  gzipped shell scripts: the server prints its version, target
#             and an install-info report, and `serve` records itself and
#             execs a daemon named sctl-daemon (coreutils sleep); libc.so is
#             a loader that execs its program
# Every URL is on 192.0.2.10 (TEST-NET-1, never routed) or an .invalid host,
# and the fake curl answers them all: nothing here can reach a network.

DEVICE_DIR=$(cd -- "$HERE/.." && pwd)
COMMON_DIR=$(cd -- "$HERE/../../common" && pwd)
REPO_DIR=$(cd -- "$HERE/../../.." && pwd)
SCRIPT="$DEVICE_DIR/ramboot-refresh.sh"
STAGER="$DEVICE_DIR/refresh-stage.sh"
OLD_VERSION=0.6.2.142
NEW_VERSION=0.6.11.196
OLD_MIRROR=http://192.0.2.10:8081/we826-qwd
NEW_MIRROR=http://192.0.2.10:8081/artifacts/$NEW_VERSION
NAMED_MIRROR=http://relay.invalid:8081/artifacts/$NEW_VERSION

for tool in busybox dash jq openssl git xxd; do
    command -v "$tool" >/dev/null 2>&1 || { echo "$tool is required"; exit 1; }
done
case $(readlink -f "$(command -v sleep)") in
    *busybox*) echo "a coreutils sleep is required (the fake daemon)"; exit 1 ;;
esac

TOP=$(mktemp -d)
cleanup() {
    stop_trees "$TOP"
    if [ -n "${KEEP_WORK:-}" ]; then echo "kept $TOP"; else rm -rf "$TOP"; fi
}
trap cleanup EXIT

# Every supervisor first (so nothing respawns), then every daemon.
stop_trees() {
    local pf
    for pf in "$1"/*/root/var/run/sctl-supervisor.pid "$1"/*/*/root/var/run/sctl-supervisor.pid; do
        [ -r "$pf" ] && kill "$(cat "$pf")" 2>/dev/null
    done
    for pf in "$1"/*/root/var/run/*.pid "$1"/*/*/root/var/run/*.pid; do
        [ -r "$pf" ] && kill "$(cat "$pf")" 2>/dev/null
    done
    return 0
}

PASS=0; FAIL=0
t_ok()  { PASS=$((PASS+1)); printf '  ok    %s\n' "$1"; }
t_bad() { FAIL=$((FAIL+1)); printf '  FAIL  %s\n     %s\n' "$1" "$2"; }
check() { if eval "$2"; then t_ok "$1"; else t_bad "$1" "${3:-$2}"; fi; }

# --- shared fixtures: payloads, the mirror, the fakes, the restricted PATH ----

FIX=$TOP/fixtures
mkdir -p "$FIX/mirror" "$FIX/bin" "$FIX/fake" "$FIX/src" "$FIX/released"

# The shared files as released, from git.
released() {
    git -C "$REPO_DIR" show "$1:devices/common/$2" > "$FIX/released/$3" 2>/dev/null ||
        { echo "git history is required: no $1:devices/common/$2"; exit 1; }
}
released 3d05058 sctl-ramboot.init init-0531
released 8be5f2b sctl-ramboot.init init-8be5f2b
released c97ad7a sctl-ramboot.init init-c97ad7a
released 3d05058 sctl-ramboot.sh ramboot-0531
released 6af8aa1 sctl-ramboot.sh ramboot-6af8aa1

ln -s "$(command -v sleep)" "$FIX/fake/sctl-daemon"

server_script() {
    cat <<EOF
#!/bin/sh
case "\$1" in
    --version) echo "sctl $1" ;;
    target) echo mips_24kc ;;
    install-info)
        [ -z "\${FAKE_TOML_BAD:-}" ] || { echo "Failed to parse config file \$3" >&2; exit 101; }
        [ "\$2" = --config ] && [ -r "\$3" ] && [ -r "\$SCTL_INSTALL_JSON" ] || exit 1
        if grep -q helper_prefix "\$SCTL_INSTALL_JSON"; then
            printf '{\n  "layout": "ramboot",\n  "target": "mips_24kc",\n  "helper_prefix": [\n    "/tmp/sctl/lib/libc.so",\n    "--library-path",\n    "/tmp/sctl/lib"\n  ]\n}\n'
        else
            printf '{\n  "layout": "ramboot",\n  "target": "mips_24kc",\n  "helper_prefix": []\n}\n'
        fi
        ;;
    serve)
        layout=none
        grep -qs '"ramboot"' "\$FAKE_ROOT/etc/sctl/install.json" && layout=ramboot
        conf=flash
        [ -z "\${SCTL_RAMBOOT_CONFIG:-}" ] || conf=trial
        echo "$1 \$\$ \$layout \$conf" > "\$FAKE_WORK/agent"
        echo "$1 \$conf" >> "\$FAKE_WORK/agents.log"
        exec "\$FAKE_FIX/fake/sctl-daemon" 300
        ;;
esac
exit 0
EOF
    # Bulk, so the /tmp arithmetic has something to count.
    head -c "$2" /dev/urandom | base64 -w 76 | sed 's/^/# /'
}
loader_script() {
    printf '#!/bin/sh\n# %s\n[ "$1" = --library-path ] && shift 2\nexec "$@"\n' "$1"
}

server_script "$OLD_VERSION" 98304 > "$FIX/src/server-old"
server_script "$NEW_VERSION" 131072 > "$FIX/src/server-new"
loader_script "musl old" > "$FIX/src/libc-old"
loader_script "musl new" > "$FIX/src/libc-new"
printf 'libgcc old\n' > "$FIX/src/libgcc-old"
printf 'libgcc new\n' > "$FIX/src/libgcc-new"
printf 'plugin old\n' > "$FIX/src/plugin-old"
printf 'plugin new\n' > "$FIX/src/plugin-new"
gz() { gzip -9 -n -c "$1" > "$2"; }
gz "$FIX/src/server-old" "$FIX/mirror/sctl-server-mips_24kc-a9227a8.gz"
gz "$FIX/src/plugin-old" "$FIX/mirror/sctl-comms-quectel-mips_24kc-a9227a8.so.gz"
gz "$FIX/src/libc-old" "$FIX/mirror/libc-mips_24kc-a9227a8.so.gz"
gz "$FIX/src/libgcc-old" "$FIX/mirror/libgcc_s-mips_24kc-a9227a8.so.1.gz"
gz "$FIX/src/server-new" "$FIX/mirror/sctl-server-mips_24kc.gz"
gz "$FIX/src/plugin-new" "$FIX/mirror/sctl-comms-quectel-mips_24kc.so.gz"
gz "$FIX/src/libc-new" "$FIX/mirror/libc-mips_24kc.so.gz"
gz "$FIX/src/libgcc-new" "$FIX/mirror/libgcc_s-mips_24kc.so.1.gz"
sha() { sha256sum "$1" | cut -d' ' -f1; }
size() { stat -c %s "$1"; }

# The release's manifest, as CI writes it (only the fields the stage reads).
m=$FIX/mirror
jq -n --arg v "$NEW_VERSION" \
    --arg s "$(sha "$m/sctl-server-mips_24kc.gz")" --argjson sz "$(size "$m/sctl-server-mips_24kc.gz")" \
    --arg p "$(sha "$m/sctl-comms-quectel-mips_24kc.so.gz")" --argjson pz "$(size "$m/sctl-comms-quectel-mips_24kc.so.gz")" \
    --arg c "$(sha "$m/libc-mips_24kc.so.gz")" --argjson cz "$(size "$m/libc-mips_24kc.so.gz")" \
    --arg g "$(sha "$m/libgcc_s-mips_24kc.so.1.gz")" --argjson gz "$(size "$m/libgcc_s-mips_24kc.so.1.gz")" \
    '{v: 1, version: $v, channel: "stable", targets: {mips_24kc: {files: [
        {role: "server", name: "sctl-server-mips_24kc.gz", size: $sz, sha256: $s, gzip: true},
        {role: "plugin", name: "sctl-comms-quectel-mips_24kc.so.gz", size: $pz, sha256: $p, gzip: true},
        {role: "libc", name: "libc-mips_24kc.so.gz", size: $cz, sha256: $c, gzip: true},
        {role: "libgcc", name: "libgcc_s-mips_24kc.so.1.gz", size: $gz, sha256: $g, gzip: true}]}}}' \
    > "$FIX/release.json"

cat > "$FIX/fake/rc" <<'FAKE'
#!/bin/sh
# /etc/rc.common <init> <action>, run by the unit's shell.
exec "$FAKE_SH" "$FAKE_FIX/fake/rc.common" "$@"
FAKE

cat > "$FIX/fake/rc.common" <<'FAKE'
# rc.common for the ramboot init, inside the unit's tree: the init file as it
# is, sourced, its absolute paths moved into the tree; no procd, so start
# takes the init's legacy path, as on the WE826. Run by the unit's shell.
init=$1
action=$2
R=$FAKE_ROOT
W=$FAKE_WORK
echo "$action $(openssl dgst -sha256 "$init" | awk '{print substr($NF, 1, 12)}') ${SCTL_RAMBOOT_CONFIG:-flash}" >> "$W/rc.log"
. "$init"
WRAPPER=$FAKE_FIX/fake/wrapper
CONFIG=$R/etc/sctl/ramboot.conf
DISABLED=$R/etc/sctl/disabled
PID_FILE=$R/var/run/sctl.pid
SUP_PID_FILE=$R/var/run/sctl-supervisor.pid
LOG_FILE=$R/tmp/sctl/ramboot.log
RESPAWN_SECS=1
case $action in boot) action=start ;; esac
$action
FAKE

cat > "$FIX/fake/wrapper" <<'FAKE'
#!/bin/sh
# The init's WRAPPER inside the unit's tree: /etc/sctl/ramboot.sh as it is
# on "flash" right now, its absolute paths moved into the tree.
w=$FAKE_WORK/wrapper.$$.sh
sed -e "s|@R@|$FAKE_ROOT|g" -e "s|@FIX@|$FAKE_FIX|g" "$FAKE_FIX/fake/tree.sed" > "$w.sed"
sed -f "$w.sed" "$FAKE_ROOT/etc/sctl/ramboot.sh" > "$w"
if grep -v '^[[:space:]]*#' "$w" | grep '/etc/sctl\|/tmp/sctl\|/var/run' | grep -qv -F "$FAKE_ROOT"; then
    echo "fake wrapper: ramboot.sh names a path outside the tree" >&2
    exit 1
fi
exec "$FAKE_SH" "$w"
FAKE

cat > "$FIX/fake/tree.sed" <<'FAKE'
s#^CONFIG=\${SCTL_RAMBOOT_CONFIG:-/etc/sctl/ramboot.conf}$#CONFIG=$(@FIX@/fake/treeconf "${SCTL_RAMBOOT_CONFIG:-@R@/etc/sctl/ramboot.conf}")#
s#^SCTL_CONFIG=\${SCTL_CONFIG:-/etc/sctl/sctl.toml}$#SCTL_CONFIG=${SCTL_CONFIG:-@R@/etc/sctl/sctl.toml}#
s#^RUN_DIR=\${RUN_DIR:-/tmp/sctl}$#RUN_DIR=${RUN_DIR:-@R@/tmp/sctl}#
s#df -k /tmp #@FIX@/fake/df -k @R@/tmp #
s#/etc/init.d/netage-wanpref#@R@/etc/init.d/netage-wanpref#g
FAKE

cat > "$FIX/fake/treeconf" <<'FAKE'
#!/bin/sh
# ramboot.sh's CONFIG inside the unit's tree: a copy of the conf with its
# paths moved into the tree; the file on "flash" stays as it is.
p=$1
case $p in "$FAKE_ROOT"/*) ;; *) p=$FAKE_ROOT$p ;; esac
echo "${p#"$FAKE_ROOT"}" >> "$FAKE_WORK/confs.log"
if [ ! -r "$p" ]; then
    echo "$p"
    exit 0
fi
out=$FAKE_WORK/conf.$$
sed "s#='/#='$FAKE_ROOT/#" "$p" > "$out"
echo "$out"
FAKE

cat > "$FIX/fake/df" <<'FAKE'
#!/bin/bash
PATH=/usr/bin:/bin
path=${!#}
case $path in
    "$FAKE_ROOT"/tmp*)
        total=${FAKE_TMP_KB:-28672}
        used=$(du -sk "$FAKE_ROOT/tmp" | cut -f1)
        fs=tmpfs mnt=/tmp
        ;;
    *)
        total=832
        used=$((832 - ${FAKE_FLASH_KB:-400}))
        fs=overlayfs:/overlay mnt=/
        ;;
esac
avail=$((total - used))
[ "$avail" -ge 0 ] || avail=0
echo "Filesystem           1K-blocks      Used Available Use% Mounted on"
printf '%-20s %9d %9d %9d %3d%% %s\n' "$fs" "$total" "$used" "$avail" $((used * 100 / total)) "$mnt"
FAKE

cat > "$FIX/bin/curl" <<'FAKE'
#!/bin/bash
PATH=/usr/bin:/bin
W=$FAKE_WORK
out= url= boot=
while [ $# -gt 0 ]; do
    case $1 in
        -o) out=$2; shift 2 ;;
        -m|--max-time|--connect-timeout) shift 2 ;;
        -fLk) boot=1; shift ;;
        -*) shift ;;
        *) url=$1; shift ;;
    esac
done
if [ -n "$out" ]; then
    if [ -n "$boot" ]; then echo "GET $url" >> "$W/net.log"; else echo "GET $url" >> "$W/curl.log"; fi
    [ -z "${FAKE_OFFLINE:-}" ] || exit 7
    name=${url##*/}
    [ -f "$FAKE_MIRROR/$name" ] || exit 22
    [ "$name" != "${FAKE_GONE:-}" ] || exit 22
    if [ "$name" = "${FAKE_CORRUPT:-}" ]; then
        { cat "$FAKE_MIRROR/$name"; echo tampered; } > "$out"
    else
        cp "$FAKE_MIRROR/$name" "$out"
    fi
    exit 0
fi
case $url in
    */api/health)
        [ -r "$W/agent" ] && read -r v pid layout conf < "$W/agent" || exit 7
        if ! kill -0 "$pid" 2>/dev/null; then
            [ -n "${FAKE_STRAY_AGENT:-}" ] || exit 7
        fi
        connected=true reconnects=0 relay=
        if [ "$v" = "$FAKE_NEW_VERSION" ]; then
            n=$(( $(cat "$W/newchecks" 2>/dev/null || echo 0) + 1 ))
            echo "$n" > "$W/newchecks"
            case ${FAKE_NEW_HEALTH:-ok} in
                never) connected=false ;;
                once) [ "$n" -le 1 ] || exit 7 ;;
                flap) reconnects=$n ;;
                trial_only) [ "$conf" = trial ] || connected=false ;;
            esac
        else
            case ${FAKE_OLD_TUNNEL:-up} in
                down) connected=false ;;
                none) connected=false ;;
                down_after_move)
                    # Up for the run new_unit started; down after a rollback
                    # until the unit has rebooted.
                    if grep -q "^$v " "$W/agents.log" 2>/dev/null && [ ! -e "$W/rebooted" ]; then
                        connected=false
                    fi
                    ;;
            esac
            [ -z "${FAKE_OLD_RELAY_ROUTE:-}" ] || relay=',"relay_route":{"dev":"eth0","mode":"prefer","suspect":[]}'
        fi
        l=null
        [ "$layout" != ramboot ] || l='"ramboot"'
        printf '{"layout":%s,"status":"ok","tunnel":{"connected":%s,"recent_events":[{"detail":"x","event":"connected","time":"1s ago"}],"reconnects":%s%s,"uptime_secs":3},"upgrade":{"from_version":"x","phase":"idle"},"uptime_secs":3,"version":"%s"}' \
            "$l" "$connected" "$reconnects" "$relay" "$v"
        ;;
    *) exit 22 ;;
esac
FAKE

cat > "$FIX/fake/reboot" <<'FAKE'
#!/bin/bash
# A cold boot: the agent gone, /tmp empty, the init started from flash with
# the unit's own PATH (its fake curl).
UNIT_PATH=$PATH
PATH=/usr/bin:/bin
R=$FAKE_ROOT W=$FAKE_WORK
echo reboot >> "$W/reboot.log"
cp "$R/tmp/sctl-we826-refresh.log" "$W/log-before-reboot" 2>/dev/null
for pf in "$R/var/run/sctl-supervisor.pid" "$R/var/run/sctl.pid"; do
    [ -r "$pf" ] && kill -9 "$(cat "$pf")" 2>/dev/null
    rm -f "$pf"
done
rm -f "$W/agent"
: > "$W/rebooted"
rm -rf "${R:?}/tmp"
mkdir -p "$R/tmp"
unset SCTL_RAMBOOT_CONFIG
PATH=$UNIT_PATH "$FAKE_FIX/fake/rc" "$R/etc/init.d/sctl" boot </dev/null >/dev/null 2>&1
kill -9 "$PPID"
FAKE

cat > "$FIX/fake/wanpref" <<'FAKE'
#!/bin/sh
echo "$1" >> "$FAKE_WORK/wanpref.log"
FAKE
chmod 0755 "$FIX/fake/rc" "$FIX/fake/wrapper" "$FIX/fake/treeconf" "$FIX/fake/df" "$FIX/bin/curl" "$FIX/fake/reboot" "$FIX/fake/wanpref"

# What the WE826's firmware has: busybox, openssl, curl. Not sha256sum, setsid,
# mktemp. Ubuntu's busybox ash runs its own applets before PATH (sha256sum
# included), so the tests run the unit's script twice: under busybox ash (the
# unit's shell) and under dash with this PATH (the unit's tools, openssl
# hashing). df and reboot are reached by absolute path for that reason.
for applet in sh awk sed grep tr cat cp mv rm mkdir chmod df du date sleep kill head tail gzip \
    dirname sync wc cut env printf echo test '[' touch ls true false; do
    ln -s "$(command -v busybox)" "$FIX/bin/$applet"
done
ln -s "$(command -v openssl)" "$FIX/bin/openssl"
UNIT_SH=$FIX/bin/sh

# --- one unit ----------------------------------------------------------------

# The environment everything on the unit runs with.
unit_env() {
    printf '%s\n' PATH="$FIX/bin" HOME=/ FAKE_ROOT="$R" FAKE_WORK="$T" FAKE_FIX="$FIX" FAKE_SH="$UNIT_SH" \
        FAKE_MIRROR="$FIX/mirror" FAKE_NEW_VERSION="$NEW_VERSION" \
        REFRESH_ROOT="$R" REFRESH_INIT_CMD="$FIX/fake/rc" REFRESH_DF="$FIX/fake/df" \
        REFRESH_REBOOT_CMD="$FIX/fake/reboot" REFRESH_POLL_SECS=1 REFRESH_PAUSE_SECS=0 REFRESH_STABLE_SECS=2 \
        REFRESH_FETCH_ATTEMPTS=2 REFRESH_FETCH_RETRY_SECS=0
}

# Wait for the agent of the tree $R to answer (its record and a live pid).
agent_up() {
    local i v pid rest
    for i in $(seq 1 50); do
        if [ -r "$T/agent" ] && read -r v pid rest < "$T/agent" && kill -0 "$pid" 2>/dev/null; then
            return 0
        fi
        sleep 0.2
    done
    return 1
}

# A WE826 as a 0.6.2-era install left it, Bus 01 by default: the init
# (released file $1, default c97ad7a's) and ramboot.sh ($2, default the
# 05-31 one), ramboot.conf as install.sh's write_kv wrote it with this unit's
# FETCH_ATTEMPTS=60 and the mirror by IP, sctl.toml with a hand-written
# [tunnel] block and state_dir, no install.json, the old payloads cached, the
# old agent running.
new_unit() {
    T=$TOP/$1
    R=$T/root
    STAGE=$R/tmp/sctl-we826-refresh
    mkdir -p "$R/etc/init.d" "$R/etc/rc.d" "$R/etc/sctl/state" "$R/tmp/sctl/cache" "$R/tmp/sctl/lib" "$R/var/run" "$T/cuts"
    cp "$FIX/released/${2:-init-c97ad7a}" "$R/etc/init.d/sctl"
    cp "$FIX/released/${3:-ramboot-0531}" "$R/etc/sctl/ramboot.sh"
    chmod 0755 "$R/etc/init.d/sctl" "$R/etc/sctl/ramboot.sh"
    cat > "$R/etc/sctl/ramboot.conf" <<EOF
RUN_DIR='/tmp/sctl'
CACHE_DIR='/tmp/sctl/cache'
BIN='/tmp/sctl/sctl-server'
PLUGIN='/tmp/sctl/lib/libsctl_comms_quectel.so'
MUSL_LIBC='/tmp/sctl/lib/libc.so'
LIBGCC='/tmp/sctl/lib/libgcc_s.so.1'
SCTL_CONFIG='/etc/sctl/sctl.toml'
SERVER_URL='$OLD_MIRROR/sctl-server-mips_24kc-a9227a8.gz'
SERVER_SHA256='$(sha "$FIX/mirror/sctl-server-mips_24kc-a9227a8.gz")'
SERVER_GZIP=1
PLUGIN_URL='$OLD_MIRROR/sctl-comms-quectel-mips_24kc-a9227a8.so.gz'
PLUGIN_SHA256='$(sha "$FIX/mirror/sctl-comms-quectel-mips_24kc-a9227a8.so.gz")'
PLUGIN_GZIP=1
MUSL_LIBC_URL='$OLD_MIRROR/libc-mips_24kc-a9227a8.so.gz'
MUSL_LIBC_SHA256='$(sha "$FIX/mirror/libc-mips_24kc-a9227a8.so.gz")'
MUSL_LIBC_GZIP=1
LIBGCC_URL='$OLD_MIRROR/libgcc_s-mips_24kc-a9227a8.so.1.gz'
LIBGCC_SHA256='$(sha "$FIX/mirror/libgcc_s-mips_24kc-a9227a8.so.1.gz")'
LIBGCC_GZIP=1
MIN_TMP_KB=24576
FETCH_TIMEOUT_SECS=90
CONNECT_TIMEOUT_SECS=10
FETCH_ATTEMPTS=60
FETCH_RETRY_SECS=15
ALLOW_UNSIGNED=0
EOF
    chmod 0600 "$R/etc/sctl/ramboot.conf"
    cat > "$R/etc/sctl/sctl.toml" <<'EOF'
[server]
listen = "0.0.0.0:1337"
data_dir = "/tmp/sctl/data"
state_dir = "/etc/sctl/state"

[auth]
api_key = "unit-key"

[device]
serial = "WE826-TEST"

[comms]
provider = "quectel-at"
library = "/tmp/sctl/lib/libsctl_comms_quectel.so"

[tunnel]  # written by hand at the bring-up
url = "wss://192.0.2.10/api/tunnel/register" # the relay by IP
tunnel_key = "tunnel-key"
EOF
    chmod 0600 "$R/etc/sctl/sctl.toml"
    echo '{"targets":[]}' > "$R/etc/sctl/state/infra-monitor.json"
    cp "$FIX/mirror/sctl-server-mips_24kc-a9227a8.gz" "$R/tmp/sctl/cache/sctl-server.payload"
    cp "$FIX/mirror/sctl-comms-quectel-mips_24kc-a9227a8.so.gz" "$R/tmp/sctl/cache/sctl-comms.payload"
    cp "$FIX/mirror/libc-mips_24kc-a9227a8.so.gz" "$R/tmp/sctl/cache/musl-libc.payload"
    cp "$FIX/mirror/libgcc_s-mips_24kc-a9227a8.so.1.gz" "$R/tmp/sctl/cache/libgcc.payload"
    : > "$T/rc.log"; : > "$T/net.log"; : > "$T/curl.log"; : > "$T/agents.log"
    stage_release "$NEW_MIRROR"
}

# Stage the release on the unit, as rundev does.
stage_release() {
    rm -rf "$STAGE"
    "$STAGER" "$FIX/release.json" "$1" "$STAGE" > /dev/null || { echo "refresh-stage.sh failed"; exit 1; }
}

# Start the old agent with its init, as the boot did; the logs start empty.
boot_unit() {
    env -i $(unit_env) "$@" "$FIX/fake/rc" "$R/etc/init.d/sctl" boot > /dev/null 2>&1
    agent_up || { echo "the unit's agent did not start: $(tail -n 5 "$R/tmp/sctl/ramboot.log" 2>/dev/null)"; exit 1; }
    : > "$T/rc.log"; : > "$T/agents.log"; : > "$T/net.log"; : > "$T/confs.log"
    snapshot "$T/before"
}

# Every file of the unit that the move may touch, with its SHA-256 and mode.
snapshot() {
    (cd "$R" && find etc tmp/sctl/cache -type f | LC_ALL=C sort | while read -r f; do
        printf '%s %s %s\n' "$(stat -c %a "$f")" "$(sha256sum "$f" | cut -d' ' -f1)" "$f"
    done) > "$1"
}

# Run the script the way the device does, synchronously; the arguments are
# VAR=value settings for the run. ARGS overrides the script's arguments.
refresh() {
    # In a subshell: the fake reboot kills the script, and bash would say so.
    ( env -i $(unit_env) REFRESH_CHECKPOINTS="$T/cuts" "$@" \
        "$UNIT_SH" "$STAGE/ramboot-refresh.sh" ${ARGS:-$NEW_VERSION 4} ) 2>/dev/null
    RC=$?
    STATE=$(cat "$STAGE/state" 2>/dev/null)
    snapshot "$T/after"
}

running() { [ -r "$T/agent" ] && read -r v pid _ < "$T/agent" && kill -0 "$pid" 2>/dev/null && echo "$v"; }
agent_field() { awk -v n="$1" '{print $n}' "$T/agent" 2>/dev/null; }
log_tail() { tail -n 14 "$R/tmp/sctl-we826-refresh.log" 2>/dev/null | tr '\n' '|'; }
in_log() { grep -q -- "$1" "$R/tmp/sctl-we826-refresh.log"; }
unchanged() { cmp -s "$T/before" "$T/after"; }
same_file() { cmp -s "$1" "$2"; }
nothing_left() { [ ! -e "$R/tmp/sctl-we826-refresh.work" ] && [ ! -e "$R/tmp/sctl-we826-refresh-rollback" ]; }
init_sum() { sha256sum "$1" | cut -c1-12; }
rc_seq() { tr '\n' ' ' < "$T/rc.log"; }
agents_seq() { tr '\n' ' ' < "$T/agents.log"; }

# A cold boot of what flash held at a checkpoint: a fresh tree with that etc/,
# an empty /tmp, the init started. Prints the version that came up, or
# nothing.
boot_cut() {
    local cut=$1 bt=$T/boot-${1##*/}
    local saved_T=$T saved_R=$R
    T=$bt R=$bt/root
    mkdir -p "$R/tmp" "$R/var/run"
    cp -a "$cut" "$R/etc"
    : > "$T/agents.log"; : > "$T/net.log"
    env -i $(unit_env) "$FIX/fake/rc" "$R/etc/init.d/sctl" boot > /dev/null 2>&1
    if agent_up; then agent_field 1; fi
    stop_trees "$saved_T"
    T=$saved_T R=$saved_R
}
