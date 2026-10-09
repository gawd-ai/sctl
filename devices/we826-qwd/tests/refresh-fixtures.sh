# Shared by ramboot-refresh-test.sh and upgrade-remote-test.sh: a WE826 as a
# directory tree, the fakes it runs with, and the release it moves to.
# Sourced; the caller sets HERE (its own directory) and `set -u`.
#
# The script runs for real under BusyBox ash, with a PATH that holds what the
# WE826's stock firmware has (busybox applets, openssl) and not what it lacks
# (sha256sum, setsid, mktemp). The unit is a directory tree (REFRESH_ROOT): the
# 0.6.2-era files on "flash", payloads in the RAM cache, the expanded agent.
# Three fakes stand in for the rest:
#   rc        the init (`rc <init> start|stop`): start does what ramboot.sh
#             does, from the cache only, and records any payload it would have
#             had to download (the unit is offline for it); it runs the cached
#             server through the cached loader to learn the version, then
#             "runs" it (two sleeps as supervisor and daemon, pid files)
#   curl      the mirror (payload downloads by name) and /api/health for the
#             running fake agent; FAKE_NEW_HEALTH shapes the new version's
#             answers (ok, never: tunnel down, once: one good answer then none)
#   payloads  gzipped shell scripts: the server prints its version, target
#             and an install-info report; libc.so is a loader that execs its
#             program, as `libc.so --library-path <dir> <bin>` does
# Every URL is on an .invalid host: nothing here can reach a network.

DEVICE_DIR=$(cd -- "$HERE/.." && pwd)
COMMON_DIR=$(cd -- "$HERE/../../common" && pwd)
SCRIPT="$DEVICE_DIR/ramboot-refresh.sh"
STAGER="$DEVICE_DIR/refresh-stage.sh"
OLD_VERSION=0.6.2.142
NEW_VERSION=0.6.11.196
OLD_MIRROR=http://relay.invalid:8081/we826-qwd
NEW_MIRROR=http://relay.invalid:8081/artifacts/$NEW_VERSION

command -v busybox >/dev/null 2>&1 || { echo "busybox is required (it is the WE826's shell)"; exit 1; }
command -v jq >/dev/null 2>&1 || { echo "jq is required"; exit 1; }
command -v openssl >/dev/null 2>&1 || { echo "openssl is required"; exit 1; }

TOP=$(mktemp -d)
cleanup() {
    for pf in "$TOP"/*/root/var/run/*.pid; do
        [ -r "$pf" ] && kill "$(cat "$pf")" 2>/dev/null
    done
    if [ -n "${KEEP_WORK:-}" ]; then echo "kept $TOP"; else rm -rf "$TOP"; fi
}
trap cleanup EXIT

PASS=0; FAIL=0
t_ok()  { PASS=$((PASS+1)); printf '  ok    %s\n' "$1"; }
t_bad() { FAIL=$((FAIL+1)); printf '  FAIL  %s\n     %s\n' "$1" "$2"; }
check() { if eval "$2"; then t_ok "$1"; else t_bad "$1" "${3:-$2}"; fi; }

# --- shared fixtures: payloads, the mirror, the fakes, the restricted PATH ----

FIX=$TOP/fixtures
mkdir -p "$FIX/mirror" "$FIX/bin" "$FIX/fake" "$FIX/src"

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
    serve) exec sleep 300 ;;
esac
EOF
}
loader_script() {
    printf '#!/bin/sh\n# %s\n[ "$1" = --library-path ] && shift 2\nexec "$@"\n' "$1"
}

server_script "$OLD_VERSION" > "$FIX/src/server-old"
server_script "$NEW_VERSION" > "$FIX/src/server-new"
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

# The release's manifest, as CI writes it (only the fields the stage reads).
jq -n --arg v "$NEW_VERSION" \
    --arg s "$(sha "$FIX/mirror/sctl-server-mips_24kc.gz")" \
    --arg p "$(sha "$FIX/mirror/sctl-comms-quectel-mips_24kc.so.gz")" \
    --arg c "$(sha "$FIX/mirror/libc-mips_24kc.so.gz")" \
    --arg g "$(sha "$FIX/mirror/libgcc_s-mips_24kc.so.1.gz")" \
    '{v: 1, version: $v, channel: "stable", targets: {mips_24kc: {files: [
        {role: "server", name: "sctl-server-mips_24kc.gz", sha256: $s, gzip: true},
        {role: "plugin", name: "sctl-comms-quectel-mips_24kc.so.gz", sha256: $p, gzip: true},
        {role: "libc", name: "libc-mips_24kc.so.gz", sha256: $c, gzip: true},
        {role: "libgcc", name: "libgcc_s-mips_24kc.so.1.gz", sha256: $g, gzip: true}]}}}' \
    > "$FIX/release.json"

cat > "$FIX/fake/rc" <<'FAKE'
#!/bin/bash
PATH=/usr/bin:/bin
init=$1 verb=$2
R=$FAKE_ROOT W=$FAKE_WORK
echo "$verb $(sha256sum "$init" | cut -c1-12)" >> "$W/rc.log"
sup=$R/var/run/sctl-supervisor.pid
daemon=$R/var/run/sctl.pid
case $verb in
    stop)
        for f in "$sup" "$daemon"; do
            [ -r "$f" ] && kill "$(cat "$f")" 2>/dev/null
            rm -f "$f"
        done
        rm -f "$W/agent"
        ;;
    start)
        if [ -r "$sup" ] && kill -0 "$(cat "$sup")" 2>/dev/null; then
            echo "sctl: supervisor already running"
            exit 0
        fi
        conf=$R/etc/sctl/ramboot.conf
        for spec in SERVER:sctl-server PLUGIN:sctl-comms MUSL_LIBC:musl-libc LIBGCC:libgcc; do
            key=${spec%%:*}
            want=$( . "$conf"; eval "echo \${${key}_SHA256}" )
            got=$(sha256sum "$R/tmp/sctl/cache/${spec#*:}.payload" 2>/dev/null | cut -d' ' -f1)
            if [ "$got" != "$want" ]; then
                echo "start needs the network for $key" >> "$W/net.log"
                exit 0
            fi
        done
        c=$R/tmp/sctl/cache
        mkdir -p "$R/tmp/sctl/lib"
        gzip -dc "$c/sctl-server.payload" > "$R/tmp/sctl/sctl-server"
        gzip -dc "$c/musl-libc.payload" > "$R/tmp/sctl/lib/libc.so"
        gzip -dc "$c/libgcc.payload" > "$R/tmp/sctl/lib/libgcc_s.so.1"
        gzip -dc "$c/sctl-comms.payload" > "$R/tmp/sctl/lib/libsctl_comms_quectel.so"
        chmod 0755 "$R/tmp/sctl/sctl-server" "$R/tmp/sctl/lib/libc.so"
        v=$("$R/tmp/sctl/lib/libc.so" --library-path "$R/tmp/sctl/lib" "$R/tmp/sctl/sctl-server" --version)
        echo "${v#sctl }" > "$W/agent"
        echo "${v#sctl }" >> "$W/agents.log"
        mkdir -p "$R/var/run"
        sleep 300 </dev/null >/dev/null 2>&1 &
        echo $! > "$sup"
        sleep 300 </dev/null >/dev/null 2>&1 &
        echo $! > "$daemon"
        ;;
esac
FAKE

cat > "$FIX/bin/curl" <<'FAKE'
#!/bin/bash
PATH=/usr/bin:/bin
W=$FAKE_WORK
out= url=
while [ $# -gt 0 ]; do
    case $1 in
        -o) out=$2; shift 2 ;;
        -m|--max-time|--connect-timeout) shift 2 ;;
        -*) shift ;;
        *) url=$1; shift ;;
    esac
done
if [ -n "$out" ]; then
    echo "GET $url" >> "$W/curl.log"
    [ -z "${FAKE_OFFLINE:-}" ] || exit 7
    name=${url##*/}
    [ -f "$FAKE_MIRROR/$name" ] || exit 22
    if [ "$name" = "${FAKE_CORRUPT:-}" ]; then
        { cat "$FAKE_MIRROR/$name"; echo tampered; } > "$out"
    else
        cp "$FAKE_MIRROR/$name" "$out"
    fi
    exit 0
fi
case $url in
    */api/health)
        [ -r "$W/agent" ] || exit 7
        v=$(cat "$W/agent")
        connected=true
        if [ "$v" = "$FAKE_NEW_VERSION" ]; then
            n=$(( $(cat "$W/newchecks" 2>/dev/null || echo 0) + 1 ))
            echo "$n" > "$W/newchecks"
            case ${FAKE_NEW_HEALTH:-ok} in
                never) connected=false ;;
                once) [ "$n" -le 1 ] || exit 7 ;;
            esac
        fi
        printf '{"layout":null,"status":"ok","tunnel":{"connected":%s,"reconnects":0},"upgrade":{"from_version":"x","phase":"idle"},"uptime_secs":3,"version":"%s"}' "$connected" "$v"
        ;;
    *) exit 22 ;;
esac
FAKE
chmod 0755 "$FIX/fake/rc" "$FIX/bin/curl"

# What the WE826's firmware has: busybox, openssl. Not sha256sum, setsid, mktemp.
# Ubuntu's busybox ash runs its own applets before PATH (sha256sum included),
# so the tests run the unit's script twice: under busybox ash (the unit's
# shell) and under dash with this PATH (the unit's tools, openssl hashing).
for applet in sh awk sed grep tr cat cp mv rm mkdir chmod df date sleep kill head tail gzip \
    dirname sync wc cut env printf echo test '[' touch ls true false; do
    ln -s "$(command -v busybox)" "$FIX/bin/$applet"
done
ln -s "$(command -v openssl)" "$FIX/bin/openssl"
UNIT_SH=$FIX/bin/sh

# --- one unit ----------------------------------------------------------------

# A WE826 as a 0.6.2-era install left it: the shared init and ramboot.sh of
# that time (here: the current ones with a line the old ones lack, so old and
# new differ), ramboot.conf written by install.sh's write_kv with this unit's
# FETCH_ATTEMPTS=60, sctl.toml with a tunnel, no install.json, the old
# payloads cached and expanded, the old agent running.
new_unit() {
    T=$TOP/$1
    R=$T/root
    STAGE=$R/tmp/sctl-we826-refresh
    mkdir -p "$R/etc/init.d" "$R/etc/sctl/state" "$R/tmp/sctl/cache" "$R/tmp/sctl/lib" "$R/var/run"
    { cat "$COMMON_DIR/sctl-ramboot.init"; echo "# as installed 2026-08-16"; } > "$R/etc/init.d/sctl"
    { cat "$COMMON_DIR/sctl-ramboot.sh"; echo "# as installed 2026-08-07"; } > "$R/etc/sctl/ramboot.sh"
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

[tunnel]
tunnel_key = "tunnel-key"
url = "wss://relay.example/api/tunnel/register"
relay_route = "prefer"
relay_route_prefer = ["eth0", "usb0"]
EOF
    chmod 0600 "$R/etc/sctl/sctl.toml"
    echo '{"targets":[]}' > "$R/etc/sctl/state/infra-monitor.json"
    cp "$FIX/mirror/sctl-server-mips_24kc-a9227a8.gz" "$R/tmp/sctl/cache/sctl-server.payload"
    cp "$FIX/mirror/sctl-comms-quectel-mips_24kc-a9227a8.so.gz" "$R/tmp/sctl/cache/sctl-comms.payload"
    cp "$FIX/mirror/libc-mips_24kc-a9227a8.so.gz" "$R/tmp/sctl/cache/musl-libc.payload"
    cp "$FIX/mirror/libgcc_s-mips_24kc-a9227a8.so.1.gz" "$R/tmp/sctl/cache/libgcc.payload"
    : > "$T/rc.log"; : > "$T/net.log"; : > "$T/curl.log"; : > "$T/agents.log"
    FAKE_ROOT=$R FAKE_WORK=$T "$FIX/fake/rc" "$R/etc/init.d/sctl" start > /dev/null
    : > "$T/rc.log"; : > "$T/agents.log"
    "$STAGER" "$FIX/release.json" "$NEW_MIRROR" "$STAGE" > /dev/null || { echo "refresh-stage.sh failed"; exit 1; }
    snapshot "$T/before"
}

# Every file of the unit that the move may touch, with its SHA-256 and mode.
snapshot() {
    (cd "$R" && find etc tmp/sctl/cache -type f | LC_ALL=C sort | while read -r f; do
        printf '%s %s %s\n' "$(stat -c %a "$f")" "$(sha256sum "$f" | cut -d' ' -f1)" "$f"
    done) > "$1"
}

# Run the script the way the device does, under busybox ash, synchronously.
refresh() {
    env -i PATH="$FIX/bin" HOME=/ REFRESH_ROOT="$R" REFRESH_INIT_CMD="$FIX/fake/rc" \
        REFRESH_POLL_SECS=1 REFRESH_PAUSE_SECS=0 REFRESH_FETCH_ATTEMPTS=2 REFRESH_FETCH_RETRY_SECS=0 \
        FAKE_ROOT="$R" FAKE_WORK="$T" FAKE_MIRROR="$FIX/mirror" FAKE_NEW_VERSION="$NEW_VERSION" "$@" \
        "$UNIT_SH" "$STAGE/ramboot-refresh.sh" "$NEW_VERSION" 4
    RC=$?
    STATE=$(cat "$STAGE/state" 2>/dev/null)
    snapshot "$T/after"
}

running() { cat "$T/agent" 2>/dev/null; }
log_tail() { tail -n 12 "$R/tmp/sctl-we826-refresh.log" 2>/dev/null | tr '\n' '|'; }
unchanged() { cmp -s "$T/before" "$T/after"; }
same_file() { cmp -s "$1" "$2"; }
nothing_left() { [ ! -e "$R/tmp/sctl-we826-refresh.work" ] && [ ! -e "$R/tmp/sctl-we826-refresh-rollback" ]; }
init_sum() { sha256sum "$1" | cut -c1-12; }
