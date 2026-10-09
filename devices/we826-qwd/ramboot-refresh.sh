#!/bin/sh
# Moves a ZBT WE826-Q-WD that cannot take a managed upgrade (an agent older
# than 0.6.7 with no /etc/sctl/install.json, or an install.json without the
# musl loader's helper_prefix) onto the current ramboot layout and a current
# release, and puts the old layout back unless the new agent proves healthy.
# `rundev.sh device upgrade-remote` stages it (TRD-8); from then on the agent
# upgrades itself when the fleet asks (docs/upgrade.md).
#
# Usage: ramboot-refresh.sh <expected_version> [wait_secs] [premove_secs]
#
# The stage directory (this script's own) holds sctl.init and ramboot.sh (the
# current devices/common files), ramboot.conf (the version's mirror URLs and
# SHA-256s), install.json (layout ramboot, the musl loader as helper_prefix),
# sizes (each payload's bytes) and files.sha256 (what was staged, checked
# first). The script stops the agent whose exec launched it, so it is started
# detached and reports through <stage>/state, one word: started, fetching,
# installing, trial, promoting, restarting (rolling_back, rebooting), then
# done, rolled_back, failed or needs_hands. The log is
# /tmp/sctl-we826-refresh.log.
#
#   failed       nothing on flash moved and the old agent was never stopped
#   done         the new version proved healthy from RAM, then again from
#                flash; the old files stay in /tmp/sctl-we826-refresh-rollback
#                until the next boot
#   rolled_back  every old file is back byte for byte, the old payloads are
#                back in the cache, and the old version answers again (with
#                its tunnel, when it had one)
#   rebooting    the restore is complete but the old agent did not come back:
#                the unit reboots into the old layout, which fetches the old
#                payloads from the URLs this run proved are served
#   needs_hands  the restore was incomplete; the supervisor keeps trying
#
# Order: nothing on flash that picks the version at boot moves until the new
# version has proved itself from RAM, and a rollback never needs the network.
#   1. the layout is recognised or refused: the shared ramboot init, a
#      ramboot.conf naming the four payloads at the paths the current layout
#      uses, sctl.toml, no install.json (or a ramboot one without the loader
#      prefix), not disabled, the musl loader in /tmp/sctl/lib, the old agent
#      answering /api/health with its version, its tunnel up when sctl.toml
#      names one (and down only when it names none), netage-wanpref absent or
#      running, relay_route off unless the old agent owns the route, and the
#      new URLs by IP when the old ones are;
#   2. /tmp has room for what this run adds on top of the MIN_TMP_KB an old
#      ramboot.sh checks at every start, re-checked before each step;
#   3. the four new payloads are fetched into a work directory and verified;
#      the new server runs here through the new musl loader (version, target,
#      this unit's sctl.toml and the staged install.json); the four old
#      payloads are fetched from the old URLs and verified, which proves a cold
#      boot of the old layout can still fetch them, and kept in RAM so a
#      rollback starts the old agent from cache; all before PREMOVE_SECS;
#   4. init, ramboot.sh, ramboot.conf, sctl.toml, install.json (when present)
#      and the agent's state_dir are copied to /tmp (the overlay is 832 KB);
#   5. the old agent is stopped with the init that started it; ramboot.sh and
#      the init are written (both start the old version from the old
#      ramboot.conf, so a power cut from here boots the old version); the new
#      payloads go into the cache;
#   6. trial: the new init starts the new version from a copy of the new
#      ramboot.conf in RAM (SCTL_RAMBOOT_CONFIG); healthy is /api/health
#      answering the version and, when the old agent had a tunnel, a connected
#      tunnel whose reconnect count holds for STABLE_SECS (60 s), within
#      WAIT_SECS of the start;
#   7. promotion: ramboot.conf, then install.json, each renamed whole and
#      synced; a power cut between the two boots the new version without
#      install.json, which a second run finishes;
#   8. the agent restarts without SCTL_RAMBOOT_CONFIG (the supervisor would
#      keep it, and the first managed upgrade's restart would boot the RAM
#      copy) and must be healthy again, from flash, reporting layout ramboot;
#   9. otherwise (6, 7 or 8 failed): install.json and ramboot.conf go back
#      first (so a power cut from there boots the old version), the new agent
#      is stopped with the new init, every other file, the state_dir and the
#      old payloads go back, and the old init starts the old agent, which must
#      answer with its version and its tunnel; if it does not after two
#      starts, the unit reboots into the restored layout.
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
PREMOVE_SECS=${3:-1800}
# The stage is this script's directory, found without dirname: a script that
# cannot find its stage must not write a state file anywhere else.
case $0 in */*) STAGE=${0%/*} ;; *) STAGE=. ;; esac
STAGE=$(cd "$STAGE" 2>/dev/null && pwd) || exit 1
# Nothing this starts (the supervisor, sctl) keeps the stage as its cwd.
cd / || exit 1
INIT=$ROOT/etc/init.d/sctl
CONF_DIR=$ROOT/etc/sctl
TOML=$CONF_DIR/sctl.toml
LIVE_RUN=$ROOT/tmp/sctl
LIVE_CACHE=$LIVE_RUN/cache
TRIAL_CONF=$LIVE_RUN/ramboot.trial.conf
PID_FILE=$ROOT/var/run/sctl.pid
SUP_PID_FILE=$ROOT/var/run/sctl-supervisor.pid
WANPREF=$ROOT/etc/init.d/netage-wanpref
WANPREF_PID=$ROOT/var/run/netage-wanpref.pid
WORK=$ROOT/tmp/sctl-we826-refresh.work
BACKUP=$ROOT/tmp/sctl-we826-refresh-rollback
INIT_CMD=${REFRESH_INIT_CMD:-}
DF=${REFRESH_DF:-df}
REBOOT_CMD=${REFRESH_REBOOT_CMD:-reboot}
CHECKPOINTS=${REFRESH_CHECKPOINTS:-}
FAULTS=${REFRESH_FAULTS:-}
POLL_SECS=${REFRESH_POLL_SECS:-5}
PAUSE_SECS=${REFRESH_PAUSE_SECS:-3}
STABLE_SECS=${REFRESH_STABLE_SECS:-60}
FETCH_TRIES=${REFRESH_FETCH_ATTEMPTS:-6}
FETCH_GAP=${REFRESH_FETCH_RETRY_SECS:-15}
FETCH_MAX=${REFRESH_FETCH_MAX_TIME:-300}
NEED_TMP_KB=${REFRESH_NEED_TMP_KB:-24576}
NEED_FLASH_KB=${REFRESH_NEED_FLASH_KB:-64}
MAX_STATE_KB=2048
ROLES="SERVER PLUGIN MUSL_LIBC LIBGCC"
# What each file is called in the backup, and where it lives. A rollback puts
# install.json and ramboot.conf back first, the rest after the stop.
FILES="init:$INIT ramboot.sh:$CONF_DIR/ramboot.sh ramboot.conf:$CONF_DIR/ramboot.conf sctl.toml:$TOML install.json:$CONF_DIR/install.json"
# The paths the current layout and install.json assume, as ramboot.conf names
# them (an absent key takes ramboot.sh's default, which is the same).
LAYOUT_PATHS="RUN_DIR=/tmp/sctl CACHE_DIR=/tmp/sctl/cache BIN=/tmp/sctl/sctl-server PLUGIN=/tmp/sctl/lib/libsctl_comms_quectel.so MUSL_LIBC=/tmp/sctl/lib/libc.so LIBGCC=/tmp/sctl/lib/libgcc_s.so.1 LOADER=/tmp/sctl/lib/libc.so SCTL_CONFIG=/etc/sctl/sctl.toml"
HELPER_PREFIX='"helper_prefix":["/tmp/sctl/lib/libc.so","--library-path","/tmp/sctl/lib"]'
TUN_UP='"tunnel":{"connected":true'
TUN_OBJ='"tunnel":{'

say() { echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) $*"; }
state() { echo "$1" > "$STAGE/state"; say "state: $1"; }
have() { command -v "$1" >/dev/null 2>&1; }

# Seconds since boot: a clock that NTP does not move.
now() {
    awk '{print int($1)}' /proc/uptime 2>/dev/null || date +%s
}

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

kb_of_bytes() {
    echo $((($1 + 1023) / 1024))
}

bytes_of() {
    bo=$(wc -c < "$1" 2>/dev/null) || return 1
    echo $((bo + 0))
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

# One value of sctl.toml: <section> <key>, a basic string or a bare word.
# A header may carry blanks inside its brackets and a comment after them.
toml_get() {
    awk -v want="$1" -v key="$2" '
        /^[ \t]*\[/ { s = $0; sub(/^[ \t]*\[[ \t]*/, "", s); sub(/[ \t]*\].*$/, "", s); next }
        s == want {
            line = $0
            if (line !~ "^[ \t]*" key "[ \t]*=") next
            sub(/^[^=]*=[ \t]*/, "", line)
            if (substr(line, 1, 1) == "\"") { line = substr(line, 2); sub(/".*$/, "", line) }
            else { sub(/[ \t]*#.*$/, "", line); sub(/[ \t]+$/, "", line) }
            print line
            exit
        }' "$TOML"
}

# The host of a URL, without user, port or path.
url_host() {
    uh=${1#*://}
    uh=${uh%%/*}
    uh=${uh##*@}
    printf '%s' "${uh%:*}"
}

is_ipv4() {
    case $1 in '' | *[!0-9.]*) return 1 ;; *.*.*.*) return 0 ;; esac
    return 1
}

# A ramboot.conf this script can stand on: the four payloads with a URL and a
# SHA-256 each, every path the one the current layout uses. Sourcing it (as
# ramboot.sh does at every start) is its parse.
check_conf() {
    cc_file=$1
    cc_what=$2
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
        curl -fsSL --connect-timeout 10 --max-time "$3" -o "$2" "$1"
    else
        wget -q -T "$3" -O "$2" "$1"
    fi
}

# Seconds left before the move must have started; nothing stops the old agent
# after that.
premove_left() {
    echo $((PREMOVE_END - $(now)))
}

# KB free in /tmp, and the floor this run keeps: what an old ramboot.sh
# checks at every start of the old agent while it still runs.
tmp_free() {
    $DF -k "$ROOT/tmp" 2>/dev/null | awk '{a = $(NF - 2)} END {print a}'
}

room_for() {
    rf_free=$(tmp_free)
    is_number "$rf_free" && [ "$rf_free" -ge $((TMP_FLOOR_KB + $1)) ] && return 0
    refuse "/tmp has ${rf_free:-unknown} KB free; $2 needs $1 KB on top of the $TMP_FLOOR_KB KB the old agent's ramboot.sh keeps free"
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

# The tunnel's reconnect count: the first "reconnects" inside the tunnel
# object (nothing nested before it carries that key).
reconnects_of() {
    ro=${1#*"$TUN_OBJ"}
    [ "$ro" != "$1" ] || return 1
    ro2=${ro#*\"reconnects\":}
    [ "$ro2" != "$ro" ] || return 1
    ro2=${ro2%%[!0-9]*}
    is_number "$ro2" || return 1
    printf '%s' "$ro2"
}

# One look at the new agent: its version, the layout when $1 is 1, and the
# tunnel when the old agent had one. Sets HR to the reconnect count.
check_health() {
    hb=$(health) || hb=
    hv=$(version_of "$hb") || hv=
    case $hb in *"$TUN_UP"*) ht=up ;; *) ht=down ;; esac
    HR=$(reconnects_of "$hb") || HR=
    case $hb in *'"layout":"ramboot"'*) hl=ramboot ;; *) hl=none ;; esac
    say "health: version ${hv:-none}, tunnel $ht${HR:+ (reconnects $HR)}, layout $hl"
    [ "$hv" = "$VERSION" ] || return 1
    [ "$1" != 1 ] || [ "$hl" = ramboot ] || return 1
    [ "$NEED_TUNNEL" = 1 ] || return 0
    [ "$ht" = up ] && [ -n "$HR" ]
}

# Healthy: every look good, with the same reconnect count, for STABLE_SECS
# (at least two looks), starting within WAIT_SECS. A bad look or a
# reconnect starts the window again.
wait_healthy() {
    wh_end=$(($(now) + WAIT_SECS + STABLE_SECS))
    wh_from=
    wh_rc=
    wh_n=0
    while [ "$(now)" -lt "$wh_end" ]; do
        sleep "$POLL_SECS"
        if check_health "$2"; then
            if [ -z "$wh_from" ] || [ "$HR" != "$wh_rc" ]; then
                [ -z "$wh_from" ] || say "the tunnel reconnected ($wh_rc, now $HR): the window starts again"
                wh_from=$(now)
                wh_rc=$HR
                wh_n=0
            fi
            wh_n=$((wh_n + 1))
            if [ "$wh_n" -ge 2 ] && [ $(($(now) - wh_from)) -ge "$STABLE_SECS" ]; then
                say "$1: healthy for $(($(now) - wh_from))s ($wh_n looks)"
                return 0
            fi
        else
            [ -z "$wh_from" ] || say "not healthy: the window starts again"
            wh_from=
            wh_n=0
        fi
    done
    say "$1: not healthy for ${STABLE_SECS}s within ${WAIT_SECS}s"
    return 1
}

# The old agent, after a rollback: its version, and its tunnel when it had one.
wait_old() {
    wo_end=$(($(now) + WAIT_SECS))
    wv=
    while [ "$(now)" -lt "$wo_end" ]; do
        sleep "$POLL_SECS"
        hb=$(health) || hb=
        wv=$(version_of "$hb") || wv=
        [ "$wv" = "$OLD_VERSION" ] || continue
        if [ "$NEED_TUNNEL" = 1 ]; then
            case $hb in *"$TUN_UP"*) ;; *) continue ;; esac
        fi
        say "the old agent answers: $OLD_VERSION$([ "$NEED_TUNNEL" = 1 ] && echo ', tunnel up')"
        return 0
    done
    say "the old agent: version ${wv:-none}$([ "$NEED_TUNNEL" = 1 ] && echo ', tunnel not up') after ${WAIT_SECS}s"
    return 1
}

# One payload, verified against its SHA-256; an earlier copy that verifies is
# kept. No attempt starts, or runs, past the pre-move deadline.
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
        f_left=$(premove_left)
        [ "$f_left" -gt 0 ] || { say "$f_label: the ${PREMOVE_SECS}s before the move are spent"; return 1; }
        [ "$f_left" -le "$FETCH_MAX" ] || f_left=$FETCH_MAX
        if download "$f_url" "$f_dst.tmp" "$f_left"; then
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

# KB a payload takes expanded, measured without writing it.
expanded_kb() {
    if [ "$(gzip_flag "$1")" = 1 ]; then
        ek=$(gzip -dc "$WORK/new/$(cache_name "$1")" 2>/dev/null | wc -c) || return 1
    else
        ek=$(bytes_of "$WORK/new/$(cache_name "$1")") || return 1
    fi
    kb_of_bytes $((ek + 0))
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
    out=$(SCTL_INSTALL_JSON="$STAGE/install.json" $run install-info --config "$TOML" 2>&1) || {
        say "the new server cannot read this unit's sctl.toml or the staged install.json: $out"
        return 1
    }
    out=$(printf '%s' "$out" | tr -d ' \n\r\t')
    case $out in *'"layout":"ramboot"'*) ;; *) say "install-info is not ramboot: $out"; return 1 ;; esac
    case $out in *"$HELPER_PREFIX"*) ;; *) say "install-info has no loader prefix: $out"; return 1 ;; esac
    say "the new server runs here: sctl $VERSION, mips_24kc, reads sctl.toml and install.json"
    rm -rf "$p"
}

# A pid this script may signal: a live process of sctl's (the supervisor is a
# shell running the init, the daemon runs from /tmp/sctl), never this script
# and never a pid reused by anything else.
sctl_pid() {
    is_number "$1" && [ "$1" != "$$" ] || return 1
    kill -0 "$1" 2>/dev/null || return 1
    sp_cmd=$(tr '\000' ' ' < "/proc/$1/cmdline" 2>/dev/null) || return 1
    case $sp_cmd in *ramboot-refresh*) return 1 ;; *sctl*) return 0 ;; esac
    return 1
}

pid_in() {
    pi=$(cat "$1" 2>/dev/null) || return 1
    sctl_pid "$pi" || return 1
    echo "$pi"
}

# Wait up to $2 seconds for pid $1 to end, then SIGKILL it.
gone_within() {
    [ -n "$1" ] || return 0
    gw_n=0
    while sctl_pid "$1"; do
        if [ "$gw_n" -ge "$2" ]; then
            say "pid $1 still runs ${2}s after stop: SIGKILL"
            kill -9 "$1" 2>/dev/null
            sleep 1
            if sctl_pid "$1"; then
                say "pid $1 survives SIGKILL"
                return 1
            fi
            return 0
        fi
        gw_n=$((gw_n + 1))
        sleep 1
    done
    return 0
}

# Stop sctl with the init now in place, and make sure it is gone: the
# supervisor within 2 s (before its 10 s respawn), the daemon within 10 s, any
# daemon it started meanwhile, then /api/health silent.
stop_agent() {
    sa_sup=$(pid_in "$SUP_PID_FILE") || sa_sup=
    sa_dmn=$(pid_in "$PID_FILE") || sa_dmn=
    $INIT_CMD "$INIT" stop
    gone_within "$sa_sup" 2 || return 1
    gone_within "$sa_dmn" 10 || return 1
    sa_late=$(pid_in "$PID_FILE") || sa_late=
    if [ -n "$sa_late" ] && [ "$sa_late" != "$sa_dmn" ]; then
        say "a daemon started during the stop: pid $sa_late"
        gone_within "$sa_late" 0 || return 1
    fi
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

# Copy beside the target, sync (a rename can reach flash before the data it
# names), rename over it, and check what landed.
put() {
    ! fault "put:${2##*/}" || return 1
    cp "$1" "$2.refresh-new" && chmod "$3" "$2.refresh-new" && sync && mv -f "$2.refresh-new" "$2" || {
        rm -f "$2.refresh-new"
        return 1
    }
    [ "$(sha256_of "$2")" = "$(sha256_of "$1")" ]
}

# For the tests: a write, a cache move or a restore that fails.
fault() {
    case " $FAULTS " in *" $1 "*) say "test fault: $1"; return 0 ;; esac
    return 1
}

# For the power-cut test: what flash holds at each step.
checkpoint() {
    [ -n "$CHECKPOINTS" ] || return 0
    CP_N=$((CP_N + 1))
    cp -a "$ROOT/etc" "$CHECKPOINTS/$CP_N-$1"
}

backup_sum() {
    awk -v n="$1" '$1 == n {print $2}' "$BACKUP/sums"
}

restore_file() {
    ! fault "restore:$1" || return 1
    rf_sum=$(backup_sum "$1")
    if [ "$rf_sum" = absent ]; then
        rm -f "$2"
        [ ! -e "$2" ]
        return
    fi
    is_sha256 "$rf_sum" || return 1
    if [ "$(sha256_of "$2")" != "$rf_sum" ]; then
        cp -p "$BACKUP/$1" "$2.refresh-old" && sync && mv -f "$2.refresh-old" "$2" || {
            rm -f "$2.refresh-old"
            return 1
        }
    fi
    [ "$(sha256_of "$2")" = "$rf_sum" ]
}

restore_named() {
    for spec in $FILES; do
        [ "${spec%%:*}" = "$1" ] || continue
        restore_file "$1" "${spec#*:}" && return 0
        say "could not restore ${spec#*:}"
        return 1
    done
    return 1
}

# Every file under a directory with its SHA-256, by relative path.
tree_sums() {
    for ts_e in "$1"/* "$1"/.[!.]*; do
        [ -e "$ts_e" ] || continue
        if [ -d "$ts_e" ]; then
            echo "d $2${ts_e##*/}"
            tree_sums "$ts_e" "$2${ts_e##*/}/"
        else
            echo "f $2${ts_e##*/} $(sha256_of "$ts_e")"
        fi
    done
}

# The agent's state_dir as it was: the new agent writes there (upgrade state,
# Infra config, TLS pins).
restore_state_dir() {
    [ -n "$SD" ] || return 0
    if [ ! -d "$BACKUP/state_dir" ]; then
        rm -rf "$SD"
        [ ! -e "$SD" ]
        return
    fi
    [ "$(tree_sums "$SD" "")" != "$(cat "$BACKUP/state_dir.sums")" ] || return 0
    rm -rf "$SD.refresh-old" "$SD.refresh-trial"
    cp -a "$BACKUP/state_dir" "$SD.refresh-old" && sync || return 1
    if [ -e "$SD" ]; then
        mv "$SD" "$SD.refresh-trial" || return 1
    fi
    mv "$SD.refresh-old" "$SD" || return 1
    rm -rf "$SD.refresh-trial"
    [ "$(tree_sums "$SD" "")" = "$(cat "$BACKUP/state_dir.sums")" ]
}

# Before the old agent was stopped: say why, leave nothing of this run behind.
refuse() {
    say "$*"
    [ "$OWN_DIRS" != 1 ] || rm -rf "$WORK" "$BACKUP"
    state failed
    exit 1
}

# After the old agent was stopped: the files that pick the version at boot
# first, then the stop, every other file, the state_dir and the old payloads;
# the old init starts the old agent. If it does not come back, a reboot into
# the restored layout, which fetches what step 3 proved is served.
rollback() {
    state rolling_back
    boot_ok=1
    complete=1
    restore_named install.json || boot_ok=0
    restore_named ramboot.conf || boot_ok=0
    sync
    checkpoint rollback-conf
    stop_agent || say "the new agent did not stop cleanly"
    rm -f "$TRIAL_CONF"
    for name in init ramboot.sh sctl.toml; do
        restore_named "$name" || boot_ok=0
    done
    restore_state_dir || { say "could not restore $SD"; complete=0; }
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
    checkpoint rollback-files
    [ "$boot_ok" = 1 ] || complete=0
    # An incomplete restore keeps the marker: no later move may replace this
    # backup until a person has looked.
    [ "$complete" != 1 ] || rm -f "$BACKUP/in-flight"
    $INIT_CMD "$INIT" start
    if wait_old || { say "restarting the old agent"; stop_agent; $INIT_CMD "$INIT" start; wait_old; }; then
        if [ "$complete" = 1 ]; then
            say "the old layout is back byte for byte and $OLD_VERSION answers"
            state rolled_back
        else
            say "$OLD_VERSION answers, but the restore is incomplete"
            state needs_hands
        fi
        exit 1
    fi
    if [ "$boot_ok" = 1 ]; then
        say "the old agent is not back after two starts; the old layout is on flash: rebooting into it"
        state rebooting
        sleep "$PAUSE_SECS"
        sync
        $REBOOT_CMD
        sleep 300
        say "still running 300s after $REBOOT_CMD"
    else
        say "the restore is incomplete and the old agent is not back: not rebooting into a mixed layout"
    fi
    state needs_hands
    exit 1
}

# ---------------------------------------------------------------------------

OWN_DIRS=0
CP_N=0
SD=
[ -n "$VERSION" ] || {
    say "usage: ramboot-refresh.sh <expected_version> [wait_secs] [premove_secs]"
    state failed
    exit 1
}
mkdir "$STAGE/lock" 2>/dev/null || {
    say "another refresh holds $STAGE/lock"
    exit 1
}
echo $$ > "$STAGE/pid"
is_number "$WAIT_SECS" || WAIT_SECS=300
is_number "$PREMOVE_SECS" || PREMOVE_SECS=1800
is_number "$STABLE_SECS" || STABLE_SECS=60
is_number "$POLL_SECS" && [ "$POLL_SECS" -ge 1 ] || POLL_SECS=1
PREMOVE_END=$(($(now) + PREMOVE_SECS))
state started
say "refresh to the ramboot layout, version $VERSION, wait ${WAIT_SECS}s, stable ${STABLE_SECS}s, move starts within ${PREMOVE_SECS}s"
# Let the exec that launched this answer before anything else.
sleep "$PAUSE_SECS"

# 0. The staged files are the ones that were sent.
for f in sctl.init ramboot.sh ramboot.conf install.json sizes files.sha256; do
    [ -r "$STAGE/$f" ] || refuse "missing $STAGE/$f"
done
is_sha256 "$(sha256_of "$STAGE/files.sha256")" || refuse "no SHA-256 tool here (sha256sum or openssl)"
if have sha256sum; then say "SHA-256 by sha256sum"; else say "SHA-256 by openssl"; fi
while read -r sum name; do
    [ -n "$name" ] || continue
    [ "$(sha256_of "$STAGE/$name")" = "$sum" ] || refuse "staged $name is not what was sent"
done < "$STAGE/files.sha256"
for f in sctl.init ramboot.sh ramboot.conf install.json sizes; do
    grep -q " $f\$" "$STAGE/files.sha256" || refuse "files.sha256 does not cover $f"
done
check_conf "$STAGE/ramboot.conf" "the staged ramboot.conf" || refuse "the staged ramboot.conf is not usable"
NEW_KB=0
for role in $ROLES; do
    b=$(awk -v r="$role" '$1 == r {print $2}' "$STAGE/sizes")
    is_number "$b" || refuse "the staged sizes name no $role"
    NEW_KB=$((NEW_KB + $(kb_of_bytes "$b")))
done
for t in gzip awk sed tr wc du; do have "$t" || refuse "no $t here"; done
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
[ -f "$TOML" ] || refuse "no $TOML"
if [ -f "$CONF_DIR/install.json" ]; then
    ij=$(tr -d ' \n\r\t' < "$CONF_DIR/install.json")
    case $ij in *'"layout":"ramboot"'*) ;; *) refuse "unknown layout: install.json is not ramboot: $ij" ;; esac
    case $ij in *'"target":"mips_24kc"'*) ;; *) refuse "unknown layout: install.json is not mips_24kc: $ij" ;; esac
    case $ij in *helper_prefix*) refuse "install.json already names the loader: the current layout; upgrade it through the fleet" ;; esac
    say "install.json without the loader prefix: its helper cannot start"
fi
[ ! -e "$CONF_DIR/disabled" ] || refuse "$CONF_DIR/disabled exists: the init would not start sctl"
[ -f "$LIVE_RUN/lib/libc.so" ] || refuse "no musl loader at $LIVE_RUN/lib/libc.so: not a WE826 ramboot unit"
if grep -q '^SUP_PID_FILE=' "$INIT"; then INIT_KIND=supervised; else INIT_KIND=single-shot; fi
say "init $(sha256_of "$INIT") ($(known_as "$(sha256_of "$INIT")"), $INIT_KIND)"
say "ramboot.sh $(sha256_of "$CONF_DIR/ramboot.sh") ($(known_as "$(sha256_of "$CONF_DIR/ramboot.sh")"))"
OLD_URL=$(conf_get "$CONF_DIR/ramboot.conf" SERVER_URL)
say "ramboot.conf $(sha256_of "$CONF_DIR/ramboot.conf"), server $OLD_URL"

# The new URLs by IP when the old ones are: the bus's resolver has timed out
# on the relay's name, and a cold boot must fetch without it.
if is_ipv4 "$(url_host "$OLD_URL")"; then
    for role in $ROLES; do
        nu=$(conf_get "$STAGE/ramboot.conf" "${role}_URL")
        is_ipv4 "$(url_host "$nu")" ||
            refuse "the old ramboot.conf names its mirror by IP ($(url_host "$OLD_URL")); the new ${role}_URL names '$(url_host "$nu")': stage the mirror by IP"
    done
fi

port=$(toml_get server listen)
port=${port##*:}
is_number "$port" || port=1337
HEALTH_URL=${REFRESH_HEALTH_URL:-http://127.0.0.1:$port/api/health}
OLD_HEALTH=$(health) || OLD_HEALTH=
OLD_VERSION=$(version_of "$OLD_HEALTH") || OLD_VERSION=
[ -n "$OLD_VERSION" ] || refuse "the running agent does not answer $HEALTH_URL with a version"

# The tunnel: required after the move exactly when the old agent has one up
# and sctl.toml names it; anything else is a disagreement this script will
# not guess through.
TOML_TUNNEL=$(toml_get tunnel url)
case $OLD_HEALTH in *"$TUN_UP"*) LIVE_TUNNEL=1 ;; *) LIVE_TUNNEL=0 ;; esac
if [ "$LIVE_TUNNEL" = 1 ] && [ -n "$TOML_TUNNEL" ]; then
    NEED_TUNNEL=1
elif [ "$LIVE_TUNNEL" = 1 ]; then
    refuse "the old agent's tunnel is up but this script finds no [tunnel] url in $TOML: it would not hold the new agent to its tunnel"
elif [ -n "$TOML_TUNNEL" ]; then
    refuse "sctl.toml names a tunnel ($TOML_TUNNEL) but the old agent's is down: the move judges the new agent by its tunnel"
else
    NEED_TUNNEL=0
fi
say "running $OLD_VERSION; healthy after the move needs $VERSION$([ "$NEED_TUNNEL" = 1 ] && echo ", the tunnel up ($TOML_TUNNEL) and steady for ${STABLE_SECS}s" || echo ' (no tunnel configured)')"
[ "$OLD_VERSION" != "$VERSION" ] || say "the running agent is already $VERSION: only the layout changes"

# relay_route: a 0.6.2 agent ignores it, a current one takes the relay's
# route and leaves it in place when it exits. Only an old agent that owns the
# route already (its health reports it) may keep it on.
RELAY_ROUTE=$(toml_get tunnel relay_route)
case ${RELAY_ROUTE:-off} in
    off) ;;
    *)
        case $OLD_HEALTH in
            *'"relay_route":{'*) say "relay_route = $RELAY_ROUTE, owned by the old agent already" ;;
            *) refuse "sctl.toml sets relay_route = $RELAY_ROUTE, which $OLD_VERSION does not run: the new agent would take the relay's route and keep it after a rollback; set it after the move" ;;
        esac
        ;;
esac

# netage-wanpref: today's ramboot.sh starts it at every sctl start. It may
# only already be running, and enabled at boot, so that nothing changes.
if [ -x "$WANPREF" ]; then
    wp=$(cat "$WANPREF_PID" 2>/dev/null) || wp=
    if ! is_number "$wp" || ! kill -0 "$wp" 2>/dev/null; then
        refuse "netage-wanpref is installed but not running: today's ramboot.sh would start it, and change this unit's routes; start it or remove it first"
    fi
    ls "$ROOT"/etc/rc.d/S*netage-wanpref >/dev/null 2>&1 ||
        refuse "netage-wanpref runs but is not enabled at boot: today's ramboot.sh would start it at every boot; enable it or remove it first"
    say "netage-wanpref runs (pid $wp) and is enabled at boot: unchanged by the move"
else
    say "no netage-wanpref"
fi

# The agent's state_dir (else its data_dir): backed up and restored with the
# files.
SD=$(toml_get server state_dir)
[ -n "$SD" ] || SD=$(toml_get server data_dir)
[ -n "$SD" ] || SD=/var/lib/sctl
case $SD in /*) ;; *) refuse "the state dir '$SD' is not an absolute path" ;; esac
SD=$ROOT$SD
SD_KB=0
if [ -d "$SD" ]; then
    SD_KB=$(du -sk "$SD" 2>/dev/null | awk '{print $1}')
    is_number "$SD_KB" && [ "$SD_KB" -le "$MAX_STATE_KB" ] ||
        refuse "the state dir $SD holds ${SD_KB:-unknown} KB, more than the $MAX_STATE_KB KB this script copies"
    say "state dir ${SD#$ROOT}: $SD_KB KB"
else
    say "state dir ${SD#$ROOT}: none yet"
fi

# 2. Room in /tmp: the old agent's ramboot.sh keeps TMP_FLOOR_KB free at
# every start while it runs, and this run adds the new payloads, then either
# the expanded probe or the old payloads' copy.
TMP_FLOOR_KB=$(conf_get "$CONF_DIR/ramboot.conf" MIN_TMP_KB)
is_number "$TMP_FLOOR_KB" || TMP_FLOOR_KB=24576
[ "$TMP_FLOOR_KB" -ge "$NEED_TMP_KB" ] || TMP_FLOOR_KB=$NEED_TMP_KB
OLD_KB=0
for role in $ROLES; do
    b=$(bytes_of "$LIVE_CACHE/$(cache_name "$role")") || b=
    if is_number "$b"; then
        OLD_KB=$((OLD_KB + $(kb_of_bytes "$b")))
    else
        OLD_KB=$((OLD_KB + NEW_KB / 4 + 1))
    fi
done
PROBE_EST_KB=0
for f in sctl-server lib/libc.so lib/libgcc_s.so.1; do
    b=$(bytes_of "$LIVE_RUN/$f") || b=0
    PROBE_EST_KB=$((PROBE_EST_KB + $(kb_of_bytes "$b")))
done
# The old agent's expanded files predict the probe; the exact size is checked
# once the new payloads are here.
[ "$PROBE_EST_KB" -gt 0 ] || PROBE_EST_KB=$((NEW_KB * 3))
PEAK_KB=$PROBE_EST_KB
[ "$OLD_KB" -le "$PEAK_KB" ] || PEAK_KB=$OLD_KB
PEAK_KB=$((NEW_KB + PEAK_KB + 64))
room_for "$PEAK_KB" "this run (new payloads $NEW_KB KB, then the probe's ~$PROBE_EST_KB KB or the old payloads' $OLD_KB KB)"
# And on flash, for the new files written beside the old ones and a copy of
# the state dir during its restore.
flash_kb=$($DF -k "$CONF_DIR" 2>/dev/null | awk '{a = $(NF - 2)} END {print a}')
is_number "$flash_kb" && [ "$flash_kb" -ge $((NEED_FLASH_KB + SD_KB)) ] ||
    refuse "the overlay has ${flash_kb:-unknown} KB free, need $((NEED_FLASH_KB + SD_KB)) KB"
say "free: /tmp $(tmp_free) KB (floor $TMP_FLOOR_KB, this run up to $PEAK_KB), overlay ${flash_kb} KB"

# 3. The new payloads, proven here; the old ones, from their URLs.
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
PROBE_KB=0
for role in SERVER MUSL_LIBC LIBGCC; do
    k=$(expanded_kb "$role") || refuse "the new $role does not expand"
    PROBE_KB=$((PROBE_KB + k))
done
room_for "$PROBE_KB" "the probe (the new server, libc and libgcc expanded)"
probe || refuse "the new server is not proven on this unit"
room_for "$OLD_KB" "the old payloads' copy"
for role in $ROLES; do
    n=$(cache_name "$role")
    fetch "$(conf_get "$CONF_DIR/ramboot.conf" "${role}_URL")" "$(conf_get "$CONF_DIR/ramboot.conf" "${role}_SHA256")" \
        "$BACKUP/cache/$n" "old $role" ||
        refuse "the old $role is not served at its URL with its SHA-256: this unit's next cold boot could not fetch it either; put it back on the mirror first"
done
say "the old payloads are served and kept in RAM: a rollback needs no network, a cold boot of the old layout can fetch"

# 4. The backup, in RAM.
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
if [ -d "$SD" ]; then
    cp -a "$SD" "$BACKUP/state_dir" || refuse "cannot back up $SD"
    tree_sums "$SD" "" > "$BACKUP/state_dir.sums"
    [ "$(tree_sums "$BACKUP/state_dir" "")" = "$(cat "$BACKUP/state_dir.sums")" ] || refuse "the copy of $SD does not match it"
fi
say "backup: $(tr '\n' ' ' < "$BACKUP/sums")$([ -d "$SD" ] && echo "and ${SD#$ROOT}")"
[ "$(premove_left)" -gt 0 ] || refuse "the ${PREMOVE_SECS}s before the move are spent; nothing was moved"

# 5. The stop, ramboot.sh and the init (the old version still boots from the
# old ramboot.conf through them), the new payloads in the cache.
state installing
: > "$BACKUP/in-flight"
if ! stop_agent; then
    rm -rf "$WORK" "$BACKUP"
    OWN_DIRS=0
    $INIT_CMD "$INIT" start
    refuse "the old agent could not be stopped cleanly; nothing was written; its init was started again"
fi
say "the old agent is stopped"
checkpoint stopped
put "$STAGE/ramboot.sh" "$CONF_DIR/ramboot.sh" 0755 &&
    put "$STAGE/sctl.init" "$INIT" 0755 || {
    say "could not write ramboot.sh and the init"
    rollback
}
sync
checkpoint wrapper-and-init
for role in $ROLES; do
    n=$(cache_name "$role")
    ! fault "cache:$role" && mv -f "$WORK/new/$n" "$LIVE_CACHE/$n" &&
        [ "$(sha256_of "$LIVE_CACHE/$n")" = "$(conf_get "$WORK/ramboot.conf" "${role}_SHA256")" ] || {
        say "could not put the new $role in the cache"
        rollback
    }
done
drop_expanded

# 6. The trial, from RAM.
cp "$WORK/ramboot.conf" "$TRIAL_CONF" && [ "$(sha256_of "$TRIAL_CONF")" = "$(sha256_of "$WORK/ramboot.conf")" ] || {
    say "could not write $TRIAL_CONF"
    rollback
}
state trial
SCTL_RAMBOOT_CONFIG=${TRIAL_CONF#$ROOT} $INIT_CMD "$INIT" start
wait_healthy "the trial" 0 || rollback

# 7. Flash: ramboot.conf, then install.json.
state promoting
put "$WORK/ramboot.conf" "$CONF_DIR/ramboot.conf" 0600 || {
    say "could not write ramboot.conf"
    rollback
}
sync
checkpoint conf
put "$STAGE/install.json" "$CONF_DIR/install.json" 0644 || {
    say "could not write install.json"
    rollback
}
sync
checkpoint install-json
say "new layout on flash: ramboot.conf $(sha256_of "$CONF_DIR/ramboot.conf")"

# 8. Once more, from flash.
state restarting
stop_agent || {
    say "the trial agent did not stop cleanly"
    rollback
}
rm -f "$TRIAL_CONF"
drop_expanded
$INIT_CMD "$INIT" start
wait_healthy "from flash" 1 || rollback

rm -f "$BACKUP/in-flight"
rm -rf "$WORK" "$BACKUP/cache"
say "done: $VERSION on the ramboot layout; the old files stay in $BACKUP until the next boot"
state done
