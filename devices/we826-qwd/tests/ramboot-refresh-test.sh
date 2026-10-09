#!/usr/bin/env bash
# Tests for ramboot-refresh.sh, the one-time move of a WE826 whose agent
# predates managed upgrades (TRD-8). The unit, the fakes and the release are
# in refresh-fixtures.sh. The stage is written by refresh-stage.sh from a
# release.json, as rundev.sh writes it.
#
# Load-bearing: a rollback puts back every file byte for byte, the old
# payloads back in the cache, starts the old version, and needs no network.

set -u

HERE=$(cd -- "$(dirname -- "$0")" && pwd)
# shellcheck source=refresh-fixtures.sh
. "$HERE/refresh-fixtures.sh"

# Every case, under one shell.
suite() {
P=$1
printf '\n%s: environment\n' "$P"
if [ "$P" = dash ]; then
    check "the unit's PATH has no sha256sum, setsid or mktemp, as on the WE826" \
        "[ -x '$UNIT_SH' ] && ! env -i PATH='$FIX/bin' '$UNIT_SH' -c 'command -v sha256sum || command -v setsid || command -v mktemp' >/dev/null"
fi

# --- 1. the happy path --------------------------------------------------------
printf '\n%s: happy path\n' "$P"
new_unit "$P-happy"
echo "HAND_ADDED=1" >> "$R/etc/sctl/ramboot.conf"
snapshot "$T/before"
OLD_INIT=$(init_sum "$R/etc/init.d/sctl")
refresh FAKE_NEW_HEALTH=ok
check "state done, exit 0" '[ "$STATE" = done ] && [ "$RC" = 0 ]' "state=$STATE rc=$RC $(log_tail)"
check "the new version runs" '[ "$(running)" = "$NEW_VERSION" ]' "running=$(running)"
check "init and ramboot.sh are the current shared files" \
    'same_file "$R/etc/init.d/sctl" "$COMMON_DIR/sctl-ramboot.init" && same_file "$R/etc/sctl/ramboot.sh" "$COMMON_DIR/sctl-ramboot.sh"'
check "install.json names the layout, the target and the loader" \
    'grep -qx "{\"v\":1,\"layout\":\"ramboot\",\"target\":\"mips_24kc\",\"helper_prefix\":\[\"/tmp/sctl/lib/libc.so\",\"--library-path\",\"/tmp/sctl/lib\"\]}" "$R/etc/sctl/install.json"' \
    "$(cat "$R/etc/sctl/install.json" 2>/dev/null)"
check "ramboot.conf names the new payloads on the release mirror" \
    'grep -qx "SERVER_URL='"'"'$NEW_MIRROR/sctl-server-mips_24kc.gz'"'"'" "$R/etc/sctl/ramboot.conf" && grep -qx "LIBGCC_SHA256='"'"'$(sha "$FIX/mirror/libgcc_s-mips_24kc.so.1.gz")'"'"'" "$R/etc/sctl/ramboot.conf"'
check "ramboot.conf keeps this unit's FETCH_ATTEMPTS=60, and is 0600" \
    'grep -qx FETCH_ATTEMPTS=60 "$R/etc/sctl/ramboot.conf" && [ "$(stat -c %a "$R/etc/sctl/ramboot.conf")" = 600 ]'
check "the log names what was kept and what was not carried" \
    'grep -q "keeping this unit'"'"'s FETCH_ATTEMPTS=60" "$R/tmp/sctl-we826-refresh.log" && ! grep -q "keeping this unit'"'"'s MIN_TMP_KB" "$R/tmp/sctl-we826-refresh.log" && grep -q "not carried from the old ramboot.conf: HAND_ADDED" "$R/tmp/sctl-we826-refresh.log"'
check "sctl.toml and the state dir are untouched" \
    'grep " etc/sctl/sctl.toml$" "$T/before" | cmp -s - <(grep " etc/sctl/sctl.toml$" "$T/after") && grep "etc/sctl/state/" "$T/before" | cmp -s - <(grep "etc/sctl/state/" "$T/after")'
check "the cache holds the new payloads" \
    'same_file "$R/tmp/sctl/cache/sctl-server.payload" "$FIX/mirror/sctl-server-mips_24kc.gz" && same_file "$R/tmp/sctl/cache/libgcc.payload" "$FIX/mirror/libgcc_s-mips_24kc.so.1.gz"'
check "stopped with the old init, started once with the new one" \
    '[ "$(cat "$T/rc.log" | tr "\n" " ")" = "stop $OLD_INIT start $(init_sum "$COMMON_DIR/sctl-ramboot.init") " ]' "rc.log: $(tr '\n' ' ' < "$T/rc.log")"
check "the new agent started from the cache, no download at start" '[ ! -s "$T/net.log" ]' "$(cat "$T/net.log")"
check "the old files stay in RAM, the old payload copies and the work dir are gone" \
    '( for f in init ramboot.sh ramboot.conf sctl.toml; do [ -f "$R/tmp/sctl-we826-refresh-rollback/$f" ] || exit 1; done ) && grep -qx "install.json absent" "$R/tmp/sctl-we826-refresh-rollback/sums" && [ ! -e "$R/tmp/sctl-we826-refresh-rollback/cache" ] && [ ! -e "$R/tmp/sctl-we826-refresh.work" ] && [ ! -e "$R/tmp/sctl-we826-refresh-rollback/in-flight" ]'
check "the backup ramboot.conf is the old one byte for byte" \
    '[ "$(sha "$R/tmp/sctl-we826-refresh-rollback/ramboot.conf")" = "$(awk '"'"'$3 == "etc/sctl/ramboot.conf" {print $2}'"'"' "$T/before")" ]'
check "the log names the old init as not a released version" 'grep -q "not a released version" "$R/tmp/sctl-we826-refresh.log"'

# --- 2. a payload whose SHA-256 does not match --------------------------------
printf '\n%s: payload hash mismatch\n' "$P"
new_unit "$P-mismatch"
refresh FAKE_CORRUPT=libc-mips_24kc.so.gz
check "state failed" '[ "$STATE" = failed ] && [ "$RC" != 0 ]' "state=$STATE $(log_tail)"
check "the mismatch is named in the log" 'grep -q "new MUSL_LIBC SHA-256 mismatch" "$R/tmp/sctl-we826-refresh.log"'
check "nothing changed on flash or in the cache" unchanged "$(diff "$T/before" "$T/after" | tr '\n' '|')"
check "the old agent was never stopped and still runs" '[ ! -s "$T/rc.log" ] && [ "$(running)" = "$OLD_VERSION" ]'
check "no work dir or backup left behind" nothing_left

# --- 3. layouts it does not know ---------------------------------------------
printf '\n%s: unknown layouts\n' "$P"
unknown() {
    local label=$1 mutate=$2 says=$3
    new_unit "$P-unknown-$(printf %s "$label" | tr -c "A-Za-z0-9" -)"
    eval "$mutate"
    snapshot "$T/before"
    refresh
    if [ "$STATE" = failed ] && unchanged && [ ! -s "$T/rc.log" ] && [ ! -s "$T/curl.log" ] && nothing_left &&
        [ "$(running)" = "$OLD_VERSION" ] && grep -q "$says" "$R/tmp/sctl-we826-refresh.log"; then
        t_ok "$label: refused before any fetch, nothing changed"
    else
        t_bad "$label: refused before any fetch, nothing changed" "state=$STATE rc.log=$(tr '\n' ' ' < "$T/rc.log") curl=$(wc -l < "$T/curl.log") $(log_tail)"
    fi
}
unknown "BIN elsewhere" \
    'sed -i "s|^BIN=.*|BIN='"'"'/tmp/other/sctl'"'"'|" "$R/etc/sctl/ramboot.conf"' \
    "sets BIN=/tmp/other/sctl"
unknown "no musl payload (a RUT241-style conf)" \
    'sed -i "/^MUSL_LIBC_/d" "$R/etc/sctl/ramboot.conf"' \
    "names no MUSL_LIBC_URL"
unknown "gz-tmp install.json" \
    'printf "{\"v\":1,\"layout\":\"gz-tmp\",\"target\":\"mips_24kc\"}\n" > "$R/etc/sctl/install.json"' \
    "install.json is not ramboot"
unknown "already the current layout" \
    'cp "$STAGE/install.json" "$R/etc/sctl/install.json"' \
    "already names the loader"
unknown "not the ramboot init" \
    'sed -i "s|^WRAPPER=.*|WRAPPER=/usr/bin/sctl-wrapper|" "$R/etc/init.d/sctl"' \
    "is not the shared ramboot init"
unknown "disabled" \
    ': > "$R/etc/sctl/disabled"' \
    "the init would not start sctl"

# --- 4. the new agent never healthy -------------------------------------------
printf '\n%s: new agent never healthy\n' "$P"
rolled_back_checks() {
    check "state rolled_back, exit 1" '[ "$STATE" = rolled_back ] && [ "$RC" = 1 ]' "state=$STATE rc=$RC $(log_tail)"
    check "every old file is back byte for byte (modes included), install.json gone" unchanged \
        "$(diff "$T/before" "$T/after" | tr '\n' '|')"
    check "the old version runs again" '[ "$(running)" = "$OLD_VERSION" ]' "running=$(running)"
    check "old init stop, new init start, new init stop, old init start" \
        '[ "$(tr "\n" " " < "$T/rc.log")" = "stop $OLD_INIT start $NEW_INIT stop $NEW_INIT start $OLD_INIT " ]' \
        "rc.log: $(tr '\n' ' ' < "$T/rc.log")"
    check "the rollback needed no network: the old agent started from the cache" '[ ! -s "$T/net.log" ]' "$(cat "$T/net.log")"
    check "the runs were the new version, then the old" \
        '[ "$(tr "\n" " " < "$T/agents.log")" = "$NEW_VERSION $OLD_VERSION " ]' "$(tr '\n' ' ' < "$T/agents.log")"
}
new_unit "$P-never"
OLD_INIT=$(init_sum "$R/etc/init.d/sctl")
NEW_INIT=$(init_sum "$COMMON_DIR/sctl-ramboot.init")
refresh FAKE_NEW_HEALTH=never
rolled_back_checks
check "the new agent answered with its version but the tunnel down" \
    'grep -q "health: version $NEW_VERSION, tunnel down" "$R/tmp/sctl-we826-refresh.log"'

# --- 5. healthy on the first check only ---------------------------------------
printf '\n%s: new agent healthy once, then silent\n' "$P"
new_unit "$P-once"
OLD_INIT=$(init_sum "$R/etc/init.d/sctl")
refresh FAKE_NEW_HEALTH=once
rolled_back_checks
check "one healthy answer was seen and was not enough" \
    'grep -q "health: version $NEW_VERSION, tunnel connected" "$R/tmp/sctl-we826-refresh.log" && [ "$(cat "$T/newchecks")" -ge 2 ]'

# --- 6. the old payloads are no longer cached ----------------------------------
printf '\n%s: old payloads not in the cache\n' "$P"
new_unit "$P-uncached"
OLD_INIT=$(init_sum "$R/etc/init.d/sctl")
rm -f "$R/tmp/sctl/cache/sctl-server.payload" "$R/tmp/sctl/cache/musl-libc.payload"
refresh FAKE_NEW_HEALTH=never
check "state rolled_back" '[ "$STATE" = rolled_back ]' "state=$STATE $(log_tail)"
check "the old payloads were fetched before anything moved" \
    'grep -q "GET $OLD_MIRROR/sctl-server-mips_24kc-a9227a8.gz" "$T/curl.log" && grep -q "GET $OLD_MIRROR/libc-mips_24kc-a9227a8.so.gz" "$T/curl.log"'
check "the rollback found them in the cache: no network at start" \
    '[ ! -s "$T/net.log" ] && [ "$(running)" = "$OLD_VERSION" ] && same_file "$R/tmp/sctl/cache/sctl-server.payload" "$FIX/mirror/sctl-server-mips_24kc-a9227a8.gz"'
new_unit "$P-uncached-offline"
rm -f "$R/tmp/sctl/cache/libgcc.payload"
snapshot "$T/before"
refresh FAKE_OFFLINE=1
check "an old payload neither cached nor fetchable: failed, nothing changed" \
    '[ "$STATE" = failed ] && [ ! -s "$T/rc.log" ] && unchanged && nothing_left' "state=$STATE $(log_tail)"

# --- 7. the new agent cannot read this unit's sctl.toml -------------------------
printf '\n%s: probe\n' "$P"
new_unit "$P-toml"
cp "$COMMON_DIR/sctl-ramboot.init" "$R/etc/init.d/sctl"
snapshot "$T/before"
refresh FAKE_TOML_BAD=1
check "an init as released is named in the log" \
    'grep -qF "init $(sha "$COMMON_DIR/sctl-ramboot.init") (init, supervised with log cap, c97ad7a)" "$R/tmp/sctl-we826-refresh.log"'
check "refused before anything moved, the reason logged" \
    '[ "$STATE" = failed ] && unchanged && [ ! -s "$T/rc.log" ] && nothing_left && grep -q "cannot read this unit" "$R/tmp/sctl-we826-refresh.log"' \
    "state=$STATE $(log_tail)"
new_unit "$P-wrongversion"
"$STAGER" "$FIX/release.json" "$NEW_MIRROR" "$STAGE" > /dev/null
env -i PATH="$FIX/bin" HOME=/ REFRESH_ROOT="$R" REFRESH_INIT_CMD="$FIX/fake/rc" REFRESH_POLL_SECS=1 \
    REFRESH_PAUSE_SECS=0 REFRESH_FETCH_ATTEMPTS=1 FAKE_ROOT="$R" FAKE_WORK="$T" FAKE_MIRROR="$FIX/mirror" \
    FAKE_NEW_VERSION="$NEW_VERSION" "$UNIT_SH" "$STAGE/ramboot-refresh.sh" 0.6.12.200 4
snapshot "$T/after"
check "a payload that is not the version asked for: failed, nothing changed" \
    '[ "$(cat "$STAGE/state")" = failed ] && unchanged && [ ! -s "$T/rc.log" ] && grep -q "is not 0.6.12.200" "$R/tmp/sctl-we826-refresh.log"'

# --- 8. guards ----------------------------------------------------------------
printf '\n%s: guards\n' "$P"
new_unit "$P-space"
refresh REFRESH_NEED_TMP_KB=999999999999
check "not enough /tmp: failed before any fetch, nothing changed" \
    '[ "$STATE" = failed ] && unchanged && [ ! -s "$T/curl.log" ] && grep -q "/tmp has" "$R/tmp/sctl-we826-refresh.log"'
new_unit "$P-flash"
refresh REFRESH_NEED_FLASH_KB=999999999999
check "not enough room on the overlay: failed before any fetch, nothing changed" \
    '[ "$STATE" = failed ] && unchanged && [ ! -s "$T/curl.log" ] && grep -q "the overlay has" "$R/tmp/sctl-we826-refresh.log"'
new_unit "$P-silent"
FAKE_ROOT=$R FAKE_WORK=$T "$FIX/fake/rc" "$R/etc/init.d/sctl" stop
: > "$T/rc.log"
refresh
check "no agent answering /api/health: failed, nothing changed" \
    '[ "$STATE" = failed ] && unchanged && [ ! -s "$T/rc.log" ] && grep -q "does not answer" "$R/tmp/sctl-we826-refresh.log"'
new_unit "$P-tampered"
echo "# changed in transit" >> "$STAGE/ramboot.sh"
refresh
check "a staged file that is not what was sent: failed, nothing changed" \
    '[ "$STATE" = failed ] && unchanged && grep -q "staged ramboot.sh is not what was sent" "$R/tmp/sctl-we826-refresh.log"'
new_unit "$P-inflight"
mkdir -p "$R/tmp/sctl-we826-refresh-rollback"
: > "$R/tmp/sctl-we826-refresh-rollback/in-flight"
refresh
check "an unfinished earlier move: refused, its backup kept" \
    '[ "$STATE" = failed ] && unchanged && [ -e "$R/tmp/sctl-we826-refresh-rollback/in-flight" ]'
new_unit "$P-relaunch"
mkdir "$STAGE/lock"
refresh
check "a second launch while one runs does nothing" \
    '[ -z "$STATE" ] && unchanged && [ ! -s "$T/rc.log" ]' "state=$STATE"

}

printf '\nthe table of released files\n'
check "the current shared init and ramboot.sh are named in the script's table" \
    'grep -q "^ *$(sha "$COMMON_DIR/sctl-ramboot.init")) " "$SCRIPT" && grep -q "^ *$(sha "$COMMON_DIR/sctl-ramboot.sh")) " "$SCRIPT"'

suite busybox-ash
if command -v dash >/dev/null 2>&1; then
    UNIT_SH=$(command -v dash)
    suite dash
    check "under dash the unit's script hashed with openssl" 'grep -q "SHA-256 by openssl" "$TOP/dash-happy/root/tmp/sctl-we826-refresh.log"'
else
    printf '\n  (no dash here: the openssl path is not exercised)\n'
fi

printf '\n  %d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
