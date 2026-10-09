#!/bin/sh
# Moves a ZBT WE826-Q-WD that cannot take a managed upgrade (an agent older
# than 0.6.7 with no /etc/sctl/install.json, or an install.json without the
# musl loader's helper_prefix) onto the current ramboot layout and a current
# release, and puts the old layout back unless the new agent comes back
# healthy. `rundev.sh device upgrade-remote` stages it (TRD-8); from then on
# the agent upgrades itself when the fleet asks (docs/upgrade.md).
#
# Usage: ramboot-refresh.sh <expected_version> [wait_secs]
#
# The stage directory (this script's own) holds sctl.init and ramboot.sh (the
# current devices/common files), ramboot.conf (the version's mirror URLs and
# SHA-256s), install.json (layout ramboot, the musl loader as helper_prefix)
# and files.sha256 (what was staged, checked first). The script restarts the
# agent whose exec launched it, so it is started detached and reports through
# <stage>/state, one word: started, fetching, installing, restarting
# (rolling_back), then done, rolled_back, failed or needs_hands. The log is
# /tmp/sctl-we826-refresh.log.
#
#   failed       nothing on flash moved and the old agent was never stopped
#   done         the new version answered healthy twice; the old files stay
#                in /tmp/sctl-we826-refresh-rollback until the next boot
#   rolled_back  every old file is back byte for byte, the old payloads are
#                back in the cache, and the old version answers again
#   needs_hands  the restore was incomplete, or the old version did not
#                answer after it; the supervisor keeps trying
#
# Order, so that nothing on flash moves before the new agent is proven here,
# and so that a rollback never needs the network (a bus loses its link):
#   1. the layout is recognised or refused: the shared ramboot init, a
#      ramboot.conf naming the four payloads at the paths the current layout
#      uses, sctl.toml, no install.json (or a ramboot one without the loader
#      prefix), not disabled, the musl loader in /tmp/sctl/lib, and the old
#      agent answering /api/health with its version;
#   2. /tmp has room, at least the old ramboot.conf's MIN_TMP_KB, which an old
#      ramboot.sh checks at every start;
#   3. the four new payloads are fetched into a work directory and verified;
#      the four old ones are copied to RAM, verified against the old
#      ramboot.conf (fetched from the old URLs when the cache no longer holds
#      them), so a rollback starts the old agent from cache;
#   4. the new server runs here through the new musl loader: it prints the
#      version and target, and reads this unit's sctl.toml and the staged
#      install.json;
#   5. init, ramboot.sh, ramboot.conf, sctl.toml and install.json (when
#      present) are copied to /tmp: the overlay (832 KB) has no room;
#   6. the old agent is stopped with the init that started it; the new files
#      are written beside their targets and renamed over them, ramboot.conf
#      last, since it is what switches the version; the new payloads replace
#      the old ones in the cache; the new init starts the new agent once;
#   7. healthy means /api/health answers with the expected version and, when
#      sctl.toml has a [tunnel] url, a connected tunnel, twice in a row within
#      WAIT_SECS; otherwise the new agent is stopped with the new init, every
#      file and the old payloads go back, and the old init starts the old
#      agent, which must answer with its version.
# A power cut during 6 boots a consistent unit: until ramboot.conf is renamed
# the boot runs the old version, after it the new one.
#
# BusyBox ash on uClibc: no sha256sum (openssl), no setsid, no mktemp, no
# local, no \| in sed. The launcher detaches it (start-stop-daemon -b where
# there is one). It catches HUP and TERM with a handler and never ignores
# them: an ignored signal is inherited by the supervisor the init starts from
# here, ash cannot trap a signal ignored on entry, and `stop` would no longer
# stop sctl. A caught signal is reset to the default in anything it execs.
#
# Every REFRESH_* variable is for the tests (devices/we826-qwd/tests/).

ROOT=${REFRESH_ROOT:-}
LOG=$ROOT/tmp/sctl-we826-refresh.log
exec </dev/null >>"$LOG" 2>&1
trap 'say "ignored SIGHUP"' HUP
trap 'say "ignored SIGTERM"' TERM

VERSION=${1:-}
WAIT_SECS=${2:-300}
# The stage is this script's directory, found without dirname: a script that
# cannot find its stage must not write a state file anywhere else.
case $0 in */*) STAGE=${0%/*} ;; *) STAGE=. ;; esac
STAGE=$(cd "$STAGE" 2>/dev/null && pwd) || exit 1
# Nothing this starts (the supervisor, sctl) keeps the stage as its cwd.
cd / || exit 1
INIT=$ROOT/etc/init.d/sctl
CONF_DIR=$ROOT/etc/sctl
LIVE_RUN=$ROOT/tmp/sctl
LIVE_CACHE=$LIVE_RUN/cache
PID_FILE=$ROOT/var/run/sctl.pid
SUP_PID_FILE=$ROOT/var/run/sctl-supervisor.pid
WORK=$ROOT/tmp/sctl-we826-refresh.work
BACKUP=$ROOT/tmp/sctl-we826-refresh-rollback
INIT_CMD=${REFRESH_INIT_CMD:-}
POLL_SECS=${REFRESH_POLL_SECS:-5}
PAUSE_SECS=${REFRESH_PAUSE_SECS:-3}
FETCH_TRIES=${REFRESH_FETCH_ATTEMPTS:-6}
FETCH_GAP=${REFRESH_FETCH_RETRY_SECS:-15}
FETCH_MAX=${REFRESH_FETCH_MAX_TIME:-300}
NEED_TMP_KB=${REFRESH_NEED_TMP_KB:-24576}
NEED_FLASH_KB=${REFRESH_NEED_FLASH_KB:-64}
ROLES="SERVER PLUGIN MUSL_LIBC LIBGCC"
# What each file is called in the backup, and where it lives.
FILES="init:$INIT ramboot.sh:$CONF_DIR/ramboot.sh ramboot.conf:$CONF_DIR/ramboot.conf sctl.toml:$CONF_DIR/sctl.toml install.json:$CONF_DIR/install.json"
# The paths the current layout and install.json assume, as ramboot.conf names
# them (an absent key takes ramboot.sh's default, which is the same).
LAYOUT_PATHS="RUN_DIR=/tmp/sctl CACHE_DIR=/tmp/sctl/cache BIN=/tmp/sctl/sctl-server PLUGIN=/tmp/sctl/lib/libsctl_comms_quectel.so MUSL_LIBC=/tmp/sctl/lib/libc.so LIBGCC=/tmp/sctl/lib/libgcc_s.so.1 LOADER=/tmp/sctl/lib/libc.so SCTL_CONFIG=/etc/sctl/sctl.toml"
HELPER_PREFIX='"helper_prefix":["/tmp/sctl/lib/libc.so","--library-path","/tmp/sctl/lib"]'

say() { echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) $*"; }
state() { echo "$1" > "$STAGE/state"; say "state: $1"; }
have() { command -v "$1" >/dev/null 2>&1; }

sha256_of() {
    if have sha256sum; then
        sha256sum "$1" 2>/dev/null | awk '{print $1}'
    elif have openssl; then
        openssl dgst -sha256 "$1" 2>/dev/null | awk '{print $NF}'
    else
        return 1
    fi
}

is_sha256() {
    case $1 in '' | *[!0-9a-f]*) return 1 ;; esac
    [ "${#1}" -eq 64 ]
}

is_number() {
    case $1 in '' | *[!0-9]*) return 1 ;; esac
    return 0
}

# Released versions of the shared files (their SHA-256s as committed), named
# in the log.
known_as() {
    case $1 in
        1e0caaa8e01ed57066ae08f9c70102e37f64025810a65c352c631952cc643cf3) echo "init, single-shot, 05-31" ;; # secret-scan: allow
        a43dbb0d8af0a8eacb40973e00969bae68d21787d17a82c43282e0197ead0ee0) echo "init, supervised, 8be5f2b" ;; # secret-scan: allow
        f0b8c0b88694507ca1d072a324de8ba795ef8a67068f8492a3ee4bbbc835ffb2) echo "init, supervised with log cap, c97ad7a" ;; # secret-scan: allow
        3e5eea75efcb574a2341818ed324ab384f82ac8ffe2b7992f2566a19ca6a578b) echo "ramboot.sh, 05-31" ;; # secret-scan: allow
        5ba9fbe2ff26bfb2fa9f978fc1cec0d6bba2f7669fbdf083dcaa317be2f8ac32) echo "ramboot.sh with wanpref, 6af8aa1" ;; # secret-scan: allow
        ce83b20ac07a7ccc2a1da47503d2513302a71f0ab531d09367091543c88108f7) echo "ramboot.sh with cache gate, c97ad7a" ;; # secret-scan: allow
        *) echo "not a released version" ;;
    esac
}

cache_name() {
    case $1 in
        SERVER) echo sctl-server.payload ;;
        PLUGIN) echo sctl-comms.payload ;;
        MUSL_LIBC) echo musl-libc.payload ;;
        LIBGCC) echo libgcc.payload ;;
    esac
}

# One value of a ramboot.conf, read the way ramboot.sh reads it (sourced), in
# a subshell, with this script's own variable of that name out of the way.
conf_get() {
    (
        unset "$2"
        . "$1" >/dev/null 2>&1 || exit 1
        eval "printf '%s' \"\${$2:-}\""
    )
}

# A ramboot.conf this script can stand on: shell, the four payloads with a URL
# and a SHA-256 each, every path the one the current layout uses.
check_conf() {
    cc_file=$1
    cc_what=$2
    sh -n "$cc_file" 2>/dev/null || { say "$cc_what does not parse as shell"; return 1; }
    for role in $ROLES; do
        [ -n "$(conf_get "$cc_file" "${role}_URL")" ] || { say "$cc_what names no ${role}_URL"; return 1; }
        is_sha256 "$(conf_get "$cc_file" "${role}_SHA256")" || { say "$cc_what has no ${role}_SHA256"; return 1; }
    done
    for pair in $LAYOUT_PATHS; do
        key=${pair%%=*}
        want=${pair#*=}
        got=$(conf_get "$cc_file" "$key")
        [ -z "$got" ] || [ "$got" = "$want" ] || {
            say "$cc_what sets $key=$got; the ramboot layout uses $want"
            return 1
        }
    done
    return 0
}

http_get() {
    if have curl; then
        curl -sf -m 5 "$1"
    else
        wget -q -T 5 -O - "$1"
    fi
}

download() {
    if have curl; then
        curl -fsSL --connect-timeout 10 --max-time "$FETCH_MAX" -o "$2" "$1"
    else
        wget -q -O "$2" "$1"
    fi
}

# The /api/health body, blanks removed. serde_json writes keys sorted, so the
# tunnel object starts with "connected".
health() {
    http_get "$HEALTH_URL" 2>/dev/null | tr -d ' \n\r\t'
}

version_of() {
    vo=${1#*\"version\":\"}
    [ "$vo" != "$1" ] || return 1
    printf '%s' "${vo%%\"*}"
}

healthy() {
    hb=$(health) || hb=
    hv=$(version_of "$hb") || hv=
    case $hb in *'"tunnel":{"connected":true'*) ht=connected ;; *) ht=down ;; esac
    [ "$NEED_TUNNEL" = 1 ] || ht="$ht (not required)"
    say "health: version ${hv:-none}, tunnel $ht"
    [ "$hv" = "$VERSION" ] || return 1
    [ "$NEED_TUNNEL" = 1 ] || return 0
    case $hb in *'"tunnel":{"connected":true'*) return 0 ;; esac
    return 1
}

# One payload, verified against its SHA-256; an earlier copy that verifies is
# kept.
fetch() {
    f_url=$1
    f_sha=$2
    f_dst=$3
    f_label=$4
    if [ -r "$f_dst" ] && [ "$(sha256_of "$f_dst")" = "$f_sha" ]; then
        say "$f_label already here"
        return 0
    fi
    rm -f "$f_dst.tmp"
    attempt=1
    while [ "$attempt" -le "$FETCH_TRIES" ]; do
        if download "$f_url" "$f_dst.tmp"; then
            got=$(sha256_of "$f_dst.tmp")
            if [ "$got" = "$f_sha" ]; then
                mv -f "$f_dst.tmp" "$f_dst"
                say "$f_label fetched and verified"
                return 0
            fi
            say "$f_label SHA-256 mismatch (got $got, want $f_sha), attempt $attempt"
        else
            say "$f_label download failed ($f_url), attempt $attempt"
        fi
        rm -f "$f_dst.tmp"
        attempt=$((attempt + 1))
        [ "$attempt" -gt "$FETCH_TRIES" ] || sleep "$FETCH_GAP"
    done
    return 1
}

expand() {
    rm -f "$2"
    if [ "$3" = 1 ]; then
        gzip -dc "$1" > "$2" || { rm -f "$2"; return 1; }
    else
        cp "$1" "$2" || return 1
    fi
}

gzip_flag() {
    g=$(conf_get "$WORK/ramboot.conf" "$1_GZIP")
    [ "$g" = 0 ] && echo 0 || echo 1
}

# The new server, run here through the new loader before anything moves.
probe() {
    p=$WORK/probe
    mkdir -p "$p/lib" || return 1
    expand "$WORK/new/sctl-server.payload" "$p/sctl-server" "$(gzip_flag SERVER)" &&
        expand "$WORK/new/musl-libc.payload" "$p/lib/libc.so" "$(gzip_flag MUSL_LIBC)" &&
        expand "$WORK/new/libgcc.payload" "$p/lib/libgcc_s.so.1" "$(gzip_flag LIBGCC)" ||
        { say "the new payloads do not expand"; return 1; }
    if [ "$(gzip_flag PLUGIN)" = 1 ]; then
        gzip -dc "$WORK/new/sctl-comms.payload" > /dev/null || { say "the new plugin does not expand"; return 1; }
    fi
    chmod 0755 "$p/sctl-server" "$p/lib/libc.so"
    run="$p/lib/libc.so --library-path $p/lib $p/sctl-server"
    out=$($run --version 2>&1 | head -n 1)
    [ "$out" = "sctl $VERSION" ] || { say "the new server does not run here or is not $VERSION: '$out'"; return 1; }
    out=$($run target 2>&1 | head -n 1)
    [ "$out" = mips_24kc ] || { say "the new server is built for '$out', not mips_24kc"; return 1; }
    out=$(SCTL_INSTALL_JSON="$STAGE/install.json" $run install-info --config "$CONF_DIR/sctl.toml" 2>&1) || {
        say "the new server cannot read this unit's sctl.toml or the staged install.json: $out"
        return 1
    }
    out=$(printf '%s' "$out" | tr -d ' \n\r\t')
    case $out in *'"layout":"ramboot"'*) ;; *) say "install-info is not ramboot: $out"; return 1 ;; esac
    case $out in *"$HELPER_PREFIX"*) ;; *) say "install-info has no loader prefix: $out"; return 1 ;; esac
    say "the new server runs here: sctl $VERSION, mips_24kc, reads sctl.toml and install.json"
    rm -rf "$p"
}

live_pid() {
    lp=$(cat "$1" 2>/dev/null) || return 1
    is_number "$lp" || return 1
    kill -0 "$lp" 2>/dev/null || return 1
    echo "$lp"
}

# Stop sctl with the init now in place, and make sure it is gone: the pids it
# recorded, then /api/health silent.
stop_agent() {
    pids=
    for pf in "$SUP_PID_FILE" "$PID_FILE"; do
        lp=$(live_pid "$pf") && pids="$pids $lp"
    done
    $INIT_CMD "$INIT" stop
    sa_n=0
    while :; do
        alive=
        for lp in $pids; do
            kill -0 "$lp" 2>/dev/null && alive="$alive $lp"
        done
        [ -n "$alive" ] || break
        sa_n=$((sa_n + 1))
        if [ "$sa_n" -ge 10 ]; then
            say "still running after stop:$alive; SIGKILL"
            for lp in $alive; do kill -9 "$lp" 2>/dev/null; done
            sleep 1
            for lp in $alive; do
                kill -0 "$lp" 2>/dev/null && { say "pid $lp survives SIGKILL"; return 1; }
            done
            break
        fi
        sleep 1
    done
    sa_n=0
    while [ -n "$(health)" ]; do
        sa_n=$((sa_n + 1))
        [ "$sa_n" -lt 10 ] || { say "/api/health still answers after stop: an agent this init does not run"; return 1; }
        sleep 1
    done
    return 0
}

# The expanded payloads; ramboot.sh writes them from the cache at every start,
# and an old ramboot.sh checks MIN_TMP_KB of free /tmp before it does.
drop_expanded() {
    rm -f "$LIVE_RUN/sctl-server" "$LIVE_RUN/lib/libsctl_comms_quectel.so" \
        "$LIVE_RUN/lib/libc.so" "$LIVE_RUN/lib/libgcc_s.so.1"
}

# Copy beside the target, rename over it, and check what landed.
put() {
    cp "$1" "$2.refresh-new" && chmod "$3" "$2.refresh-new" && mv -f "$2.refresh-new" "$2" || {
        rm -f "$2.refresh-new"
        return 1
    }
    [ "$(sha256_of "$2")" = "$(sha256_of "$1")" ]
}

backup_sum() {
    awk -v n="$1" '$1 == n {print $2}' "$BACKUP/sums"
}

restore_file() {
    rf_sum=$(backup_sum "$1")
    if [ "$rf_sum" = absent ]; then
        rm -f "$2"
        [ ! -e "$2" ]
        return
    fi
    is_sha256 "$rf_sum" || return 1
    if [ "$(sha256_of "$2")" != "$rf_sum" ]; then
        cp -p "$BACKUP/$1" "$2.refresh-old" && mv -f "$2.refresh-old" "$2" || {
            rm -f "$2.refresh-old"
            return 1
        }
    fi
    [ "$(sha256_of "$2")" = "$rf_sum" ]
}

wait_old() {
    wo_tries=0
    while [ "$wo_tries" -lt "$MAX_TRIES" ]; do
        sleep "$POLL_SECS"
        wo_tries=$((wo_tries + 1))
        wv=$(version_of "$(health)") || wv=
        if [ "$wv" = "$OLD_VERSION" ]; then
            say "the old agent answers: $OLD_VERSION"
            return 0
        fi
    done
    return 1
}

# Before anything moved: say why, leave nothing of this run behind.
refuse() {
    say "$*"
    [ "$OWN_DIRS" != 1 ] || rm -rf "$WORK" "$BACKUP"
    state failed
    exit 1
}

# After the old agent was stopped: every file and payload back, the old init
# starts the old agent.
rollback() {
    state rolling_back
    stop_agent || say "the new agent did not stop cleanly"
    complete=1
    for spec in $FILES; do
        name=${spec%%:*}
        path=${spec#*:}
        restore_file "$name" "$path" || { say "could not restore $path"; complete=0; }
    done
    for role in $ROLES; do
        n=$(cache_name "$role")
        want=$(conf_get "$BACKUP/ramboot.conf" "${role}_SHA256")
        if [ -f "$BACKUP/cache/$n" ]; then
            mv -f "$BACKUP/cache/$n" "$LIVE_CACHE/$n" || complete=0
        fi
        [ "$(sha256_of "$LIVE_CACHE/$n")" = "$want" ] || { say "the old $role is not back in the cache"; complete=0; }
    done
    drop_expanded
    rm -rf "$WORK"
    sync
    # An incomplete restore keeps the marker: no later refresh may replace
    # this backup until a person has looked.
    [ "$complete" != 1 ] || rm -f "$BACKUP/in-flight"
    $INIT_CMD "$INIT" start
    if wait_old || { say "starting the old agent again"; $INIT_CMD "$INIT" start; wait_old; }; then
        if [ "$complete" = 1 ]; then
            say "the old layout is back byte for byte and $OLD_VERSION answers"
            state rolled_back
        else
            say "$OLD_VERSION answers, but the restore is incomplete"
            state needs_hands
        fi
    else
        say "the old agent does not answer ${WAIT_SECS}s after the restore, twice"
        state needs_hands
    fi
    exit 1
}

# ---------------------------------------------------------------------------

OWN_DIRS=0
[ -n "$VERSION" ] || {
    say "usage: ramboot-refresh.sh <expected_version> [wait_secs]"
    state failed
    exit 1
}
mkdir "$STAGE/lock" 2>/dev/null || {
    say "another refresh holds $STAGE/lock"
    exit 1
}
echo $$ > "$STAGE/pid"
is_number "$WAIT_SECS" || WAIT_SECS=300
is_number "$POLL_SECS" && [ "$POLL_SECS" -ge 1 ] || POLL_SECS=1
MAX_TRIES=$((WAIT_SECS / POLL_SECS))
[ "$MAX_TRIES" -ge 2 ] || MAX_TRIES=2
state started
say "refresh to the ramboot layout, version $VERSION, wait ${WAIT_SECS}s"
# Let the exec that launched this answer before anything else.
sleep "$PAUSE_SECS"

# 0. The staged files are the ones that were sent.
for f in sctl.init ramboot.sh ramboot.conf install.json files.sha256; do
    [ -r "$STAGE/$f" ] || refuse "missing $STAGE/$f"
done
is_sha256 "$(sha256_of "$STAGE/files.sha256")" || refuse "no SHA-256 tool here (sha256sum or openssl)"
if have sha256sum; then say "SHA-256 by sha256sum"; else say "SHA-256 by openssl"; fi
while read -r sum name; do
    [ -n "$name" ] || continue
    [ "$(sha256_of "$STAGE/$name")" = "$sum" ] || refuse "staged $name is not what was sent"
done < "$STAGE/files.sha256"
for f in sctl.init ramboot.sh; do
    sh -n "$STAGE/$f" 2>/dev/null || refuse "staged $f does not parse as shell"
done
check_conf "$STAGE/ramboot.conf" "the staged ramboot.conf" || refuse "the staged ramboot.conf is not usable"
for t in gzip awk sed tr; do have "$t" || refuse "no $t here"; done
have curl || have wget || refuse "neither curl nor wget here"

# 1. The layout this script knows how to move, or nothing.
[ ! -d "$BACKUP" ] || [ ! -e "$BACKUP/in-flight" ] ||
    refuse "an earlier refresh did not finish; its backup is in $BACKUP: restore it by hand first"
[ -f "$INIT" ] || refuse "no $INIT"
grep -q '^WRAPPER=/etc/sctl/ramboot.sh$' "$INIT" && grep -q '^CONFIG=/etc/sctl/ramboot.conf$' "$INIT" &&
    grep -q '^PID_FILE=/var/run/sctl.pid$' "$INIT" || refuse "$INIT is not the shared ramboot init"
[ -f "$CONF_DIR/ramboot.sh" ] || refuse "no $CONF_DIR/ramboot.sh"
[ -f "$CONF_DIR/ramboot.conf" ] || refuse "no $CONF_DIR/ramboot.conf: not the ramboot layout"
check_conf "$CONF_DIR/ramboot.conf" "$CONF_DIR/ramboot.conf" || refuse "unknown layout: $CONF_DIR/ramboot.conf"
[ -f "$CONF_DIR/sctl.toml" ] || refuse "no $CONF_DIR/sctl.toml"
if [ -f "$CONF_DIR/install.json" ]; then
    ij=$(tr -d ' \n\r\t' < "$CONF_DIR/install.json")
    case $ij in *'"layout":"ramboot"'*) ;; *) refuse "unknown layout: install.json is not ramboot: $ij" ;; esac
    case $ij in *'"target":"mips_24kc"'*) ;; *) refuse "unknown layout: install.json is not mips_24kc: $ij" ;; esac
    case $ij in *helper_prefix*) refuse "install.json already names the loader: the current layout; upgrade it through the fleet" ;; esac
    say "install.json without the loader prefix: its helper cannot start"
fi
[ ! -e "$CONF_DIR/disabled" ] || refuse "$CONF_DIR/disabled exists: the init would not start sctl"
[ -f "$LIVE_RUN/lib/libc.so" ] || refuse "no musl loader at $LIVE_RUN/lib/libc.so: not a WE826 ramboot unit"
say "init $(sha256_of "$INIT") ($(known_as "$(sha256_of "$INIT")"))"
say "ramboot.sh $(sha256_of "$CONF_DIR/ramboot.sh") ($(known_as "$(sha256_of "$CONF_DIR/ramboot.sh")"))"
say "ramboot.conf $(sha256_of "$CONF_DIR/ramboot.conf"), server $(conf_get "$CONF_DIR/ramboot.conf" SERVER_URL)"

port=$(awk -F'"' '/^\[/ {s = $0} s == "[server]" && $1 ~ /^[ \t]*listen[ \t]*=/ {print $2; exit}' "$CONF_DIR/sctl.toml")
port=${port##*:}
is_number "$port" || port=1337
HEALTH_URL=${REFRESH_HEALTH_URL:-http://127.0.0.1:$port/api/health}
NEED_TUNNEL=0
awk '/^\[/ {s = $0} s == "[tunnel]" && /^[ \t]*url[ \t]*=/ {f = 1} END {exit !f}' "$CONF_DIR/sctl.toml" && NEED_TUNNEL=1
OLD_VERSION=$(version_of "$(health)") || OLD_VERSION=
[ -n "$OLD_VERSION" ] || refuse "the running agent does not answer $HEALTH_URL with a version"
say "running $OLD_VERSION; healthy after the move needs $VERSION$([ "$NEED_TUNNEL" = 1 ] && echo ' and the tunnel up')"
[ "$OLD_VERSION" != "$VERSION" ] || say "the running agent is already $VERSION: only the layout changes"

# 2. Room in /tmp, for this and for a rollback.
free_kb=$(df -k "$ROOT/tmp" 2>/dev/null | awk '{a = $(NF - 2)} END {print a}')
need_kb=$NEED_TMP_KB
old_min=$(conf_get "$CONF_DIR/ramboot.conf" MIN_TMP_KB)
! is_number "$old_min" || [ "$old_min" -le "$need_kb" ] || need_kb=$old_min
is_number "$free_kb" && [ "$free_kb" -ge "$need_kb" ] || refuse "/tmp has ${free_kb:-unknown} KB free, need $need_kb KB"
# And on flash, for the new files written beside the old ones (a few KB).
flash_kb=$(df -k "$CONF_DIR" 2>/dev/null | awk '{a = $(NF - 2)} END {print a}')
is_number "$flash_kb" && [ "$flash_kb" -ge "$NEED_FLASH_KB" ] ||
    refuse "the overlay has ${flash_kb:-unknown} KB free, need $NEED_FLASH_KB KB"
say "free: /tmp ${free_kb} KB, overlay ${flash_kb} KB"

# 3. Both payload sets, in RAM, verified.
state fetching
rm -rf "$WORK" "$BACKUP"
OWN_DIRS=1
mkdir -p "$WORK/new" "$BACKUP/cache" || refuse "cannot create $WORK and $BACKUP"
cp "$STAGE/ramboot.conf" "$WORK/ramboot.conf" || refuse "cannot copy the staged ramboot.conf"
# The fetch settings this unit was given stay (a bus may boot with no link
# for a while); everything about the version comes from the staged file.
for key in MIN_TMP_KB FETCH_TIMEOUT_SECS CONNECT_TIMEOUT_SECS FETCH_ATTEMPTS FETCH_RETRY_SECS; do
    v=$(conf_get "$CONF_DIR/ramboot.conf" "$key")
    is_number "$v" || continue
    grep -q "^$key=" "$WORK/ramboot.conf" || continue
    [ "$v" != "$(conf_get "$WORK/ramboot.conf" "$key")" ] || continue
    sed "s/^$key=.*/$key=$v/" "$WORK/ramboot.conf" > "$WORK/ramboot.conf.tmp" &&
        mv -f "$WORK/ramboot.conf.tmp" "$WORK/ramboot.conf" || refuse "cannot write $WORK/ramboot.conf"
    say "keeping this unit's $key=$v"
done
for key in $(sed -n 's/^\([A-Z_][A-Z0-9_]*\)=.*/\1/p' "$CONF_DIR/ramboot.conf"); do
    grep -q "^$key=" "$WORK/ramboot.conf" || say "not carried from the old ramboot.conf: $key"
done
check_conf "$WORK/ramboot.conf" "the new ramboot.conf" || refuse "the new ramboot.conf is not usable"
for role in $ROLES; do
    n=$(cache_name "$role")
    fetch "$(conf_get "$WORK/ramboot.conf" "${role}_URL")" "$(conf_get "$WORK/ramboot.conf" "${role}_SHA256")" \
        "$WORK/new/$n" "new $role" || refuse "the new $role could not be fetched and verified"
done
for role in $ROLES; do
    n=$(cache_name "$role")
    old_sha=$(conf_get "$CONF_DIR/ramboot.conf" "${role}_SHA256")
    if [ -r "$LIVE_CACHE/$n" ] && [ "$(sha256_of "$LIVE_CACHE/$n")" = "$old_sha" ]; then
        cp -p "$LIVE_CACHE/$n" "$BACKUP/cache/$n" && [ "$(sha256_of "$BACKUP/cache/$n")" = "$old_sha" ] ||
            refuse "cannot keep the old $role in $BACKUP"
        say "old $role kept from the cache"
    else
        say "the cache does not hold the old $role: fetching it, so that a rollback needs no network"
        fetch "$(conf_get "$CONF_DIR/ramboot.conf" "${role}_URL")" "$old_sha" "$BACKUP/cache/$n" "old $role" ||
            refuse "the old $role is neither cached nor fetchable: a rollback would need the network"
    fi
done

# 4. The new server runs here.
probe || refuse "the new server is not proven on this unit"

# 5. The backup, in RAM.
: > "$BACKUP/sums" || refuse "cannot write $BACKUP/sums"
for spec in $FILES; do
    name=${spec%%:*}
    path=${spec#*:}
    if [ -f "$path" ]; then
        sum=$(sha256_of "$path")
        cp -p "$path" "$BACKUP/$name" && [ "$(sha256_of "$BACKUP/$name")" = "$sum" ] ||
            refuse "cannot back up $path"
        echo "$name $sum" >> "$BACKUP/sums"
    else
        echo "$name absent" >> "$BACKUP/sums"
    fi
done
say "backup: $(tr '\n' ' ' < "$BACKUP/sums")"

# 6. The move.
state installing
: > "$BACKUP/in-flight"
if ! stop_agent; then
    rm -f "$BACKUP/in-flight"
    $INIT_CMD "$INIT" start
    refuse "the old agent could not be stopped cleanly; nothing was written"
fi
say "the old agent is stopped"
put "$STAGE/ramboot.sh" "$CONF_DIR/ramboot.sh" 0755 &&
    put "$STAGE/sctl.init" "$INIT" 0755 &&
    put "$STAGE/install.json" "$CONF_DIR/install.json" 0644 &&
    put "$WORK/ramboot.conf" "$CONF_DIR/ramboot.conf" 0600 || {
    say "could not write the new layout"
    rollback
}
for role in $ROLES; do
    n=$(cache_name "$role")
    mv -f "$WORK/new/$n" "$LIVE_CACHE/$n" &&
        [ "$(sha256_of "$LIVE_CACHE/$n")" = "$(conf_get "$WORK/ramboot.conf" "${role}_SHA256")" ] || {
        say "could not put the new $role in the cache"
        rollback
    }
done
drop_expanded
sync
say "new layout written: ramboot.conf $(sha256_of "$CONF_DIR/ramboot.conf")"

state restarting
$INIT_CMD "$INIT" start

# 7. Healthy twice in a row, or back.
good=0
tries=0
while [ "$tries" -lt "$MAX_TRIES" ]; do
    sleep "$POLL_SECS"
    tries=$((tries + 1))
    if healthy; then
        good=$((good + 1))
        [ "$good" -lt 2 ] || break
    else
        good=0
    fi
done
if [ "$good" -lt 2 ]; then
    say "not healthy twice in a row within ${WAIT_SECS}s of the restart"
    rollback
fi
rm -f "$BACKUP/in-flight"
rm -rf "$WORK" "$BACKUP/cache"
say "healthy $((tries * POLL_SECS))s after the restart; the old files stay in $BACKUP until the next boot"
state done
