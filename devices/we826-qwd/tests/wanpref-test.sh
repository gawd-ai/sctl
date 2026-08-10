#!/bin/sh
# Unit tests for netage-wanpref.init route logic.
#
# These run anywhere with a POSIX sh + awk — no OpenWrt, no device, no network.
# The script under test is sourced with every external tool pointed at a fake, so
# the real assertions are about which routes it mutates and in what ORDER.
#
# The load-bearing test is `invariant_default_route_always_exists`: it replays the
# mutation log prefix-by-prefix and asserts a default route survives after every
# single step. That is the machine-checkable form of "this can never strand a
# device", which is the entire safety argument for shipping this to a vehicle.

set -u

HERE=$(cd -- "$(dirname -- "$0")" && pwd)
INIT="$HERE/../netage-wanpref.init"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

PASS=0; FAIL=0
ok()   { PASS=$((PASS+1)); printf '  ok    %s\n' "$1"; }
bad()  { FAIL=$((FAIL+1)); printf '  FAIL  %s\n     %s\n' "$1" "$2"; }

# The routing table exactly as observed on the bus with both uplinks up.
# quectel-CM's rogue route prints with no `metric` token, i.e. metric 0.
OBSERVED='default via 100.119.100.16 dev usb0
default via 192.168.50.1 dev eth0 metric 3
default via 100.119.100.16 dev usb0 metric 5'

setup() {
    FAKE_TABLE="$WORK/table"; FAKE_LOG="$WORK/log"
    printf '%s\n' "$1" > "$FAKE_TABLE"
    : > "$FAKE_LOG"
    export FAKE_TABLE FAKE_LOG
    export FAKE_IPV4="eth0 usb0"
}

# Source the init script with all externals faked. `set -- ` clears rc.common's
# arg handling so sourcing does not try to dispatch a verb.
load_script() {
    IP="$HERE/fake-ip"
    CURL="$HERE/fake-curl"
    PING=/bin/true
    UCI=/bin/false          # forces restore_wan's metric default of 3
    SYSNET="$WORK/sys"
    UPTIME_FILE="$WORK/uptime"
    CONF="$WORK/conf"
    STATE="$WORK/state"
    export IP CURL PING UCI SYSNET UPTIME_FILE CONF STATE
    mkdir -p "$SYSNET/eth0" "$SYSNET/usb0"
    echo 1 > "$SYSNET/eth0/carrier"; echo up > "$SYSNET/eth0/operstate"
    echo 1 > "$SYSNET/usb0/carrier"; echo up > "$SYSNET/usb0/operstate"
    echo "1234.56 1000.00" > "$WORK/uptime"
    : > "$CONF"
    # Pull in only the function definitions: everything below the rc verbs is
    # inert without an explicit call.
    # shellcheck disable=SC1090
    . "$INIT" 2>/dev/null || true
    WAN_IF=eth0; LTE_IF=usb0
    LTE_FALLBACK_METRIC=100; WAN_DEMOTED_METRIC=200
}

table() { cat "$FAKE_TABLE"; }
log()   { cat "$FAKE_LOG"; }
defaults_count() { grep -c '^default' "$FAKE_TABLE" 2>/dev/null || echo 0; }

# ---------------------------------------------------------------------------
printf '\nnetage-wanpref route logic\n'

# --- 1. the real-world case: exactly one deletion, no gratuitous adds ---------
setup "$OBSERVED"; load_script
assert_wired >/dev/null 2>&1
if grep -q 'route del default via 100.119.100.16 dev usb0 metric 0' "$FAKE_LOG"; then
    ok "assert_wired deletes the rogue metric-0 route"
else
    bad "assert_wired deletes the rogue metric-0 route" "log: $(log | tr '\n' ';')"
fi
if grep -q 'route replace' "$FAKE_LOG"; then
    bad "assert_wired adds nothing when metric 5 already qualifies" "log: $(log | tr '\n' ';')"
else
    ok "assert_wired adds nothing when metric 5 already qualifies"
fi
if [ "$(defaults_count)" -eq 2 ] && grep -q 'dev eth0 metric 3' "$FAKE_TABLE"; then
    ok "wired route now wins (eth0/3 beats usb0/5)"
else
    bad "wired route now wins" "table: $(table | tr '\n' ';')"
fi

# --- 2. add-before-delete when there is no surviving cellular default --------
setup 'default via 100.119.100.16 dev usb0
default via 192.168.50.1 dev eth0 metric 3'
load_script
assert_wired >/dev/null 2>&1
add_line=$(grep -n 'route replace default via 100.119.100.16 dev usb0 metric 100' "$FAKE_LOG" | cut -d: -f1)
del_line=$(grep -n 'route del default via 100.119.100.16 dev usb0 metric 0' "$FAKE_LOG" | cut -d: -f1)
if [ -n "$add_line" ] && [ -n "$del_line" ] && [ "$add_line" -lt "$del_line" ]; then
    ok "ADD (metric 100) precedes DELETE (metric 0) — the safety ordering"
else
    bad "ADD precedes DELETE" "add=$add_line del=$del_line log: $(log | tr '\n' ';')"
fi

# --- 3. demote is required for fallback, and round-trips ---------------------
setup "$OBSERVED"; load_script
demote_wan >/dev/null 2>&1
if grep -q 'dev eth0 metric 200' "$FAKE_TABLE" && ! grep -q 'dev eth0 metric 3' "$FAKE_TABLE"; then
    ok "demote_wan moves eth0 to metric 200 (link-up-but-dead cannot blackhole)"
else
    bad "demote_wan moves eth0 to 200" "table: $(table | tr '\n' ';')"
fi
restore_wan >/dev/null 2>&1
if grep -q 'dev eth0 metric 3' "$FAKE_TABLE" && ! grep -q 'dev eth0 metric 200' "$FAKE_TABLE"; then
    ok "restore_wan round-trips eth0 back to its UCI metric"
else
    bad "restore_wan round-trips" "table: $(table | tr '\n' ';')"
fi

# --- 3b. THE SEQUENCE the loop actually runs on demote -----------------------
# Regression: the loop's demote branch used to be `demote_wan; assert_lte`, and
# assert_lte calls restore_wan — which put eth0 straight back to metric 3 and
# deleted the metric-200 route, silently undoing the demote. Testing demote_wan
# in isolation (test 3 above) cannot catch that; only the sequence can.
#
# Why it matters: with eth0 back at metric 3 it beats usb0's metric 5, so the
# moment quectel-CM's metric-0 route is absent (every redial, and permanently if
# the PDN is down) all traffic blackholes out the dead wired link — precisely
# the outcome demote_wan exists to prevent.
setup "$OBSERVED"; load_script
demote_wan >/dev/null 2>&1
assert_lte_routes >/dev/null 2>&1
if grep -q 'dev eth0 metric 200' "$FAKE_TABLE" && ! grep -q 'dev eth0 metric 3' "$FAKE_TABLE"; then
    ok "demote survives the loop's follow-up call (eth0 stays at 200)"
else
    bad "demote survives assert_lte_routes" "table: $(table | tr '\n' ';')"
fi
# ...while the full revert still restores it, for stop() and ENABLED=0.
setup "$OBSERVED"; load_script
demote_wan >/dev/null 2>&1
assert_lte >/dev/null 2>&1
if grep -q 'dev eth0 metric 3' "$FAKE_TABLE" && ! grep -q 'dev eth0 metric 200' "$FAKE_TABLE"; then
    ok "assert_lte still fully reverts eth0 (stop() / ENABLED=0 path)"
else
    bad "assert_lte fully reverts eth0" "table: $(table | tr '\n' ';')"
fi

# --- 3c. the demote must not LATCH ------------------------------------------
# Regression: probe_wan looked its gateway up with gw_of, which excludes our
# synthetic metrics. After a demote the only wired default IS synthetic (metric
# 200), so gw_of returned empty, probe_wan returned 1 forever, and the device
# could never climb back onto the wired uplink — it sat on metered cellular
# until a DHCP renew happened to reinstate netifd's route.
setup 'default via 100.119.100.16 dev usb0
default via 192.168.50.1 dev eth0 metric 200
default via 100.119.100.16 dev usb0 metric 5'
load_script
HAVE_CURL_IFACE=1
FAKE_CURL_CODE=200; export FAKE_CURL_CODE
if probe_wan >/dev/null 2>&1; then
    ok "probe_wan still works while wired is demoted (recovery is possible)"
else
    bad "probe_wan fails on a demoted wired iface" "demote would latch forever"
fi
# Control: the gateway must be found via gw_any, since gw_of is empty here.
if [ -z "$(gw_of eth0)" ] && [ -n "$(gw_any eth0)" ]; then
    ok "control: gw_of is empty on a demoted iface, gw_any is not"
else
    bad "control setup wrong" "gw_of=$(gw_of eth0) gw_any=$(gw_any eth0)"
fi
unset FAKE_CURL_CODE

# --- 4. gc_synthetic drops a stale synthetic route after a DHCP renew --------
setup 'default via 192.168.50.1 dev eth0 metric 3
default via 10.9.9.9 dev eth0 metric 200
default via 100.119.100.16 dev usb0 metric 5'
load_script
gc_synthetic >/dev/null 2>&1
if ! grep -q 'via 10.9.9.9' "$FAKE_TABLE"; then
    ok "gc_synthetic deletes a synthetic route whose gateway went stale"
else
    bad "gc_synthetic deletes stale synthetic" "table: $(table | tr '\n' ';')"
fi

# --- 5. gw_of ignores our own synthetic metrics ------------------------------
setup 'default via 192.168.50.1 dev eth0 metric 3
default via 10.9.9.9 dev eth0 metric 200'
load_script
g=$(gw_of eth0)
if [ "$g" = "192.168.50.1" ]; then
    ok "gw_of returns netifd's gateway, not our synthetic one"
else
    bad "gw_of ignores synthetic" "got '$g'"
fi

# --- 6. three cellular defaults: only those below the wired metric go --------
setup 'default via 100.119.100.16 dev usb0
default via 192.168.50.1 dev eth0 metric 3
default via 100.119.100.16 dev usb0 metric 5
default via 100.119.100.16 dev usb0 metric 100'
load_script
assert_wired >/dev/null 2>&1
if grep -q 'dev usb0 metric 5' "$FAKE_TABLE" && grep -q 'dev usb0 metric 100' "$FAKE_TABLE" \
   && ! grep -qE 'dev usb0$' "$FAKE_TABLE"; then
    ok "only cellular defaults BELOW the wired metric are removed"
else
    bad "selective removal" "table: $(table | tr '\n' ';')"
fi

# --- 7. lte-only: nothing to do, nothing destroyed ---------------------------
setup 'default via 100.119.100.16 dev usb0'
load_script
assert_wired >/dev/null 2>&1
if [ "$(defaults_count)" -ge 1 ]; then
    ok "lte-only table keeps its default route (assert_wired is a no-op)"
else
    bad "lte-only keeps a default" "table: $(table | tr '\n' ';')"
fi

# --- 8. probe refuses to clobber an existing pinned host route ---------------
setup 'default via 192.168.50.1 dev eth0 metric 3
174.138.114.209 via 100.119.100.16 dev usb0'
load_script
HAVE_CURL_IFACE=0
code=$(probe_one 174.138.114.209 8443 192.168.50.1)
if [ "$code" = "000" ] && grep -q '174.138.114.209 via 100.119.100.16 dev usb0' "$FAKE_TABLE"; then
    ok "probe refuses a /32 for a target that already has a host route"
else
    bad "probe refuses to clobber pinned /32" "code=$code table: $(table | tr '\n' ';')"
fi

# --- 9. THE INVARIANT: a default route exists after every single step ---------
# Replay the mutation log prefix-by-prefix against the starting table and assert
# the table is never left with zero default routes. This is the property that
# makes the feature safe to run on a vehicle we cannot physically reach.
invariant_check() {
    _name=$1; _start=$2; _fn=$3
    setup "$_start"; load_script
    $_fn >/dev/null 2>&1
    _steps=$(wc -l < "$FAKE_LOG")
    _i=1; _viol=""
    while [ "$_i" -le "$_steps" ]; do
        printf '%s\n' "$_start" > "$WORK/replay"
        _n=1
        while [ "$_n" -le "$_i" ]; do
            _cmd=$(sed -n "${_n}p" "$FAKE_LOG")
            # shellcheck disable=SC2086
            FAKE_TABLE="$WORK/replay" FAKE_LOG=/dev/null "$HERE/fake-ip" $_cmd >/dev/null 2>&1
            _n=$((_n + 1))
        done
        if ! grep -q '^default' "$WORK/replay"; then _viol="after step $_i"; break; fi
        _i=$((_i + 1))
    done
    if [ -z "$_viol" ]; then
        ok "invariant: default route survives every step — $_name ($_steps steps)"
    else
        bad "invariant: $_name" "no default route $_viol"
    fi
}

invariant_check "assert_wired on observed table" "$OBSERVED" assert_wired
invariant_check "assert_wired, no surviving backup" 'default via 100.119.100.16 dev usb0
default via 192.168.50.1 dev eth0 metric 3' assert_wired
invariant_check "demote_wan on observed table" "$OBSERVED" demote_wan
invariant_check "assert_lte on observed table" "$OBSERVED" assert_lte

printf '\n  %s passed, %s failed\n\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
