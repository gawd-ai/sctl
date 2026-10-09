#!/usr/bin/env bash
# Tests for ramboot-refresh.sh, the one-time move of a WE826 whose agent
# predates managed upgrades (TRD-8). The unit, the fakes and the release are
# in refresh-fixtures.sh: the init files and ramboot.sh run as released. The
# stage is written by refresh-stage.sh from a release.json, as rundev.sh
# writes it.
#
# Load-bearing: nothing that picks the version at boot moves before the new
# version proved itself from RAM; a power cut at any step boots one version
# whole, by IP; a rollback puts back every file byte for byte, the old
# payloads back in the cache, starts the old version with its tunnel, and
# needs no network.

set -u

HERE=$(cd -- "$(dirname -- "$0")" && pwd)
# shellcheck source=refresh-fixtures.sh
. "$HERE/refresh-fixtures.sh"

# Every flash state the run went through boots one version whole: the old one
# without an install.json naming the loader, or the new one; by IP.
cuts_boot() {
    local want=$1 got="" cut v bad=""
    for cut in "$T"/cuts/*; do
        [ -d "$cut" ] || continue
        v=$(boot_cut "$cut")
        got="$got${cut##*/}=$v "
        if [ "$v" != "$OLD_VERSION" ] && [ "$v" != "$NEW_VERSION" ]; then bad="$bad ${cut##*/}:nothing"; fi
        if [ "$v" = "$OLD_VERSION" ] && grep -qs helper_prefix "$cut/sctl/install.json"; then bad="$bad ${cut##*/}:old-with-loader-install.json"; fi
        case $(sed -n "s/^SERVER_URL='http:\/\/\([^:/]*\).*/\1/p" "$cut/sctl/ramboot.conf") in
            192.0.2.10) ;;
            *) bad="$bad ${cut##*/}:mirror-not-by-ip" ;;
        esac
    done
    CUTS="$got$bad"
    CUTS_OK=0
    if [ -z "$bad" ] && [ "$got" = "$want" ]; then CUTS_OK=1; fi
}
CUTS=
CUTS_OK=0

# A move refused before anything was fetched or stopped.
refused() {
    local label=$1 says=$2
    if [ "$STATE" = failed ] && unchanged && [ ! -s "$T/rc.log" ] && [ ! -s "$T/curl.log" ] && nothing_left &&
        [ "$(running)" = "$OLD_VERSION" ] && in_log "$says"; then
        t_ok "$label: refused before any fetch, nothing changed"
    else
        t_bad "$label: refused before any fetch, nothing changed" "state=$STATE rc.log=$(rc_seq) curl=$(wc -l < "$T/curl.log") $(log_tail)"
    fi
}

rolled_back_checks() {
    check "state rolled_back, exit 1" '[ "$STATE" = rolled_back ] && [ "$RC" = 1 ]' "state=$STATE rc=$RC $(log_tail)"
    check "every old file is back byte for byte (modes, state dir and cache included), install.json gone" unchanged \
        "$(diff "$T/before" "$T/after" | tr '\n' '|')"
    check "the old version runs again, from flash" '[ "$(running)" = "$OLD_VERSION" ] && [ "$(agent_field 4)" = flash ]' "running=$(running)"
    check "the rollback needed no network: the old agent started from the cache" '[ ! -s "$T/net.log" ]' "$(cat "$T/net.log")"
    check "no trial conf, work dir or payload copies left; the in-flight marker is gone" \
        '[ ! -e "$R/tmp/sctl/ramboot.trial.conf" ] && [ ! -e "$R/tmp/sctl-we826-refresh.work" ] && [ ! -e "$R/tmp/sctl-we826-refresh-rollback/cache/sctl-server.payload" ] && [ ! -e "$R/tmp/sctl-we826-refresh-rollback/in-flight" ]'
}

suite() {
P=$1
printf '\n%s: environment\n' "$P"
if [ "$P" = dash ]; then
    check "the unit's PATH has no sha256sum, setsid or mktemp, as on the WE826" \
        "[ -x '$UNIT_SH' ] && ! env -i PATH='$FIX/bin' '$UNIT_SH' -c 'command -v sha256sum || command -v setsid || command -v mktemp' >/dev/null"
fi

# --- 1. the happy path, Bus 01 as it is -----------------------------------------
printf '\n%s: happy path (c97ad7a init, 05-31 ramboot.sh, mirror by IP)\n' "$P"
new_unit "$P-happy"
echo "HAND_ADDED=1" >> "$R/etc/sctl/ramboot.conf"
boot_unit
OLD_INIT=$(init_sum "$R/etc/init.d/sctl")
NEW_INIT=$(init_sum "$COMMON_DIR/sctl-ramboot.init")
refresh FAKE_NEW_HEALTH=ok
check "state done, exit 0" '[ "$STATE" = done ] && [ "$RC" = 0 ]' "state=$STATE rc=$RC $(log_tail)"
check "the new version runs from flash and reports layout ramboot" \
    '[ "$(running)" = "$NEW_VERSION" ] && [ "$(agent_field 4)" = flash ] && [ "$(agent_field 3)" = ramboot ]' "$(cat "$T/agent")"
check "it ran first from the RAM copy of ramboot.conf, then from flash" \
    '[ "$(agents_seq)" = "$NEW_VERSION trial $NEW_VERSION flash " ] && [ "$(tr "\n" " " < "$T/confs.log")" = "/tmp/sctl/ramboot.trial.conf /etc/sctl/ramboot.conf " ]' \
    "agents: $(agents_seq) confs: $(tr '\n' ' ' < "$T/confs.log")"
check "stopped with the old init, the trial and the restart with the new one" \
    '[ "$(rc_seq)" = "stop $OLD_INIT flash start $NEW_INIT /tmp/sctl/ramboot.trial.conf stop $NEW_INIT flash start $NEW_INIT flash " ]' \
    "rc.log: $(rc_seq)"
check "init and ramboot.sh are the current shared files" \
    'same_file "$R/etc/init.d/sctl" "$COMMON_DIR/sctl-ramboot.init" && same_file "$R/etc/sctl/ramboot.sh" "$COMMON_DIR/sctl-ramboot.sh"'
check "install.json names the layout, the target and the loader" \
    'grep -qx "{\"v\":1,\"layout\":\"ramboot\",\"target\":\"mips_24kc\",\"helper_prefix\":\[\"/tmp/sctl/lib/libc.so\",\"--library-path\",\"/tmp/sctl/lib\"\]}" "$R/etc/sctl/install.json"' \
    "$(cat "$R/etc/sctl/install.json" 2>/dev/null)"
check "ramboot.conf names the new payloads on the release mirror, by IP" \
    'grep -qx "SERVER_URL='"'"'$NEW_MIRROR/sctl-server-mips_24kc.gz'"'"'" "$R/etc/sctl/ramboot.conf" && grep -qx "LIBGCC_SHA256='"'"'$(sha "$FIX/mirror/libgcc_s-mips_24kc.so.1.gz")'"'"'" "$R/etc/sctl/ramboot.conf"'
check "ramboot.conf keeps this unit's FETCH_ATTEMPTS=60, and is 0600" \
    'grep -qx FETCH_ATTEMPTS=60 "$R/etc/sctl/ramboot.conf" && [ "$(stat -c %a "$R/etc/sctl/ramboot.conf")" = 600 ]'
check "the log names what was kept and what was not carried" \
    'in_log "keeping this unit'"'"'s FETCH_ATTEMPTS=60" && ! in_log "keeping this unit'"'"'s MIN_TMP_KB" && in_log "not carried from the old ramboot.conf: HAND_ADDED"'
check "a [tunnel] header with a comment is read: the tunnel is required" \
    'in_log "the tunnel up (wss://192.0.2.10/api/tunnel/register) and steady"'
check "sctl.toml and the state dir are untouched" \
    'grep " etc/sctl/sctl.toml$" "$T/before" | cmp -s - <(grep " etc/sctl/sctl.toml$" "$T/after") && grep "etc/sctl/state/" "$T/before" | cmp -s - <(grep "etc/sctl/state/" "$T/after")'
check "the cache holds the new payloads" \
    'same_file "$R/tmp/sctl/cache/sctl-server.payload" "$FIX/mirror/sctl-server-mips_24kc.gz" && same_file "$R/tmp/sctl/cache/libgcc.payload" "$FIX/mirror/libgcc_s-mips_24kc.so.1.gz"'
check "both starts came from the cache: no download by ramboot.sh" '[ ! -s "$T/net.log" ]' "$(cat "$T/net.log")"
check "the old payloads were fetched from their URLs before anything moved" \
    'grep -q "GET $OLD_MIRROR/sctl-server-mips_24kc-a9227a8.gz" "$T/curl.log" && grep -q "GET $OLD_MIRROR/libgcc_s-mips_24kc-a9227a8.so.1.gz" "$T/curl.log" && [ "$(grep -n "a9227a8" "$T/curl.log" | head -n1 | cut -d: -f1)" -gt 4 ]'
check "the old files stay in RAM; the payload copies, the work dir, the trial conf and the marker are gone" \
    '( for f in init ramboot.sh ramboot.conf sctl.toml; do [ -f "$R/tmp/sctl-we826-refresh-rollback/$f" ] || exit 1; done ) && [ -d "$R/tmp/sctl-we826-refresh-rollback/state_dir" ] && grep -qx "install.json absent" "$R/tmp/sctl-we826-refresh-rollback/sums" && [ ! -e "$R/tmp/sctl-we826-refresh-rollback/cache" ] && [ ! -e "$R/tmp/sctl-we826-refresh.work" ] && [ ! -e "$R/tmp/sctl/ramboot.trial.conf" ] && [ ! -e "$R/tmp/sctl-we826-refresh-rollback/in-flight" ]'
check "the backup ramboot.conf is the old one byte for byte" \
    '[ "$(sha "$R/tmp/sctl-we826-refresh-rollback/ramboot.conf")" = "$(awk '"'"'$3 == "etc/sctl/ramboot.conf" {print $2}'"'"' "$T/before")" ]'
check "the log names the init and ramboot.sh as released" \
    'in_log "(init, supervised with log cap, c97ad7a, supervised)" && in_log "(ramboot.sh, 05-31)"'
cuts_boot "1-stopped=$OLD_VERSION 2-wrapper-and-init=$OLD_VERSION 3-conf=$NEW_VERSION 4-install-json=$NEW_VERSION "
check "a power cut at each step boots one version whole, by IP: old, old, new, new" '[ "$CUTS_OK" = 1 ]' "$CUTS"

# --- 2. a payload whose SHA-256 does not match --------------------------------
printf '\n%s: payload hash mismatch\n' "$P"
new_unit "$P-mismatch"
boot_unit
refresh FAKE_CORRUPT=libc-mips_24kc.so.gz
check "state failed" '[ "$STATE" = failed ] && [ "$RC" != 0 ]' "state=$STATE $(log_tail)"
check "the mismatch is named in the log" 'in_log "new MUSL_LIBC SHA-256 mismatch"'
check "nothing changed on flash or in the cache" unchanged "$(diff "$T/before" "$T/after" | tr '\n' '|')"
check "the old agent was never stopped and still runs" '[ ! -s "$T/rc.log" ] && [ "$(running)" = "$OLD_VERSION" ]'
check "no work dir or backup left behind" nothing_left

# --- 3. layouts and conditions it does not take ---------------------------------
printf '\n%s: refusals\n' "$P"
# label, a change made after the unit booted, what the log says, then
# VAR=value settings for the run.
refuse_case() {
    local label=$1 mutate=$2 says=$3
    shift 3
    new_unit "$P-refuse-$(printf %s "$label" | tr -c "A-Za-z0-9" -)"
    boot_unit
    eval "$mutate"
    snapshot "$T/before"
    refresh "$@"
    refused "$label" "$says"
}
refuse_case "BIN elsewhere" \
    'sed -i "s|^BIN=.*|BIN='"'"'/tmp/other/sctl'"'"'|" "$R/etc/sctl/ramboot.conf"' \
    "sets BIN=/tmp/other/sctl"
refuse_case "no musl payload (a RUT241-style conf)" \
    'sed -i "/^MUSL_LIBC_/d" "$R/etc/sctl/ramboot.conf"' \
    "names no MUSL_LIBC_URL"
refuse_case "gz-tmp install.json" \
    'printf "{\"v\":1,\"layout\":\"gz-tmp\",\"target\":\"mips_24kc\"}\n" > "$R/etc/sctl/install.json"' \
    "install.json is not ramboot"
refuse_case "already the current layout" \
    'cp "$STAGE/install.json" "$R/etc/sctl/install.json"' \
    "already names the loader"
refuse_case "not the ramboot init" \
    'sed -i "s|^WRAPPER=.*|WRAPPER=/usr/bin/sctl-wrapper|" "$R/etc/init.d/sctl"' \
    "is not the shared ramboot init"
refuse_case "disabled" \
    ': > "$R/etc/sctl/disabled"' \
    "the init would not start sctl"
refuse_case "a tunnel in sctl.toml that is down" \
    ':' \
    "but the old agent's is down" FAKE_OLD_TUNNEL=down
refuse_case "a tunnel up that sctl.toml does not name" \
    'sed -i "/^\[tunnel\]/,\$d" "$R/etc/sctl/sctl.toml"' \
    "finds no \[tunnel\] url"
refuse_case "the new mirror by name while the old is by IP" \
    'stage_release "$NAMED_MIRROR"' \
    "stage the mirror by IP"
refuse_case "relay_route that the old agent does not run" \
    'printf "relay_route = \"prefer\"\nrelay_route_prefer = [\"eth0\", \"usb0\"]\n" >> "$R/etc/sctl/sctl.toml"' \
    "sets relay_route = prefer"
refuse_case "netage-wanpref installed, not running" \
    'cp "$FIX/fake/wanpref" "$R/etc/init.d/netage-wanpref"; echo 999999 > "$R/var/run/netage-wanpref.pid"' \
    "netage-wanpref is installed but not running"
refuse_case "netage-wanpref running, not enabled at boot" \
    'cp "$FIX/fake/wanpref" "$R/etc/init.d/netage-wanpref"; sleep 300 & echo $! > "$R/var/run/netage-wanpref.pid"' \
    "not enabled at boot"
kill "$(cat "$R/var/run/netage-wanpref.pid")" 2>/dev/null
new_unit "$P-gone"
boot_unit
refresh FAKE_GONE=libgcc_s-mips_24kc-a9227a8.so.1.gz
check "an old payload no longer served at its URL: failed after the fetch, nothing changed, never stopped" \
    '[ "$STATE" = failed ] && unchanged && [ ! -s "$T/rc.log" ] && nothing_left && [ "$(running)" = "$OLD_VERSION" ] && in_log "put it back on the mirror first"' \
    "state=$STATE $(log_tail)"

# --- 4. the new agent never healthy -------------------------------------------
printf '\n%s: new agent never healthy\n' "$P"
new_unit "$P-never"
boot_unit
refresh FAKE_NEW_HEALTH=never
rolled_back_checks
check "stop, trial with the new init, stop with it, start with the old" \
    '[ "$(rc_seq)" = "stop $OLD_INIT flash start $NEW_INIT /tmp/sctl/ramboot.trial.conf stop $NEW_INIT flash start $OLD_INIT flash " ]' "rc.log: $(rc_seq)"
check "the runs were the new version on trial, then the old from flash" \
    '[ "$(agents_seq)" = "$NEW_VERSION trial $OLD_VERSION flash " ]' "$(agents_seq)"
check "the new agent answered with its version but the tunnel down" 'in_log "health: version $NEW_VERSION, tunnel down"'
cuts_boot "1-stopped=$OLD_VERSION 2-wrapper-and-init=$OLD_VERSION 3-rollback-conf=$OLD_VERSION 4-rollback-files=$OLD_VERSION "
check "a power cut at each step boots the old version" '[ "$CUTS_OK" = 1 ]' "$CUTS"

# --- 5. healthy on the first look only -----------------------------------------
printf '\n%s: new agent healthy once, then silent\n' "$P"
new_unit "$P-once"
boot_unit
refresh FAKE_NEW_HEALTH=once
rolled_back_checks
check "one healthy answer was seen and was not enough" \
    'in_log "health: version $NEW_VERSION, tunnel up" && [ "$(cat "$T/newchecks")" -ge 2 ]'

# --- 6. a tunnel that keeps reconnecting -----------------------------------------
printf '\n%s: new agent connected, but reconnecting\n' "$P"
new_unit "$P-flap"
boot_unit
refresh FAKE_NEW_HEALTH=flap
rolled_back_checks
check "every reconnect started the window again" 'in_log "the tunnel reconnected"'

# --- 7. healthy on trial, not from flash ------------------------------------------
printf '\n%s: healthy from RAM, not from flash\n' "$P"
new_unit "$P-flash"
boot_unit
refresh FAKE_NEW_HEALTH=trial_only
rolled_back_checks
check "the runs: trial, from flash, then the old version" \
    '[ "$(agents_seq)" = "$NEW_VERSION trial $NEW_VERSION flash $OLD_VERSION flash " ]' "$(agents_seq)"
cuts_boot "1-stopped=$OLD_VERSION 2-wrapper-and-init=$OLD_VERSION 3-conf=$NEW_VERSION 4-install-json=$NEW_VERSION 5-rollback-conf=$OLD_VERSION 6-rollback-files=$OLD_VERSION "
check "a power cut at each step boots one version whole: old, old, new, new, then old, old" '[ "$CUTS_OK" = 1 ]' "$CUTS"

# --- 8. an older unit: the single-shot init ----------------------------------------
printf '\n%s: single-shot init of 05-31, never healthy\n' "$P"
new_unit "$P-single" init-0531 ramboot-0531
boot_unit
OLD0531=$(init_sum "$R/etc/init.d/sctl")
refresh FAKE_NEW_HEALTH=never
rolled_back_checks
check "stopped and restarted with the 05-31 init, the new one in between" \
    '[ "$(rc_seq)" = "stop $OLD0531 flash start $NEW_INIT /tmp/sctl/ramboot.trial.conf stop $NEW_INIT flash start $OLD0531 flash " ]' "rc.log: $(rc_seq)"
check "the new init's supervisor is gone" \
    '! { [ -r "$R/var/run/sctl-supervisor.pid" ] && kill -0 "$(cat "$R/var/run/sctl-supervisor.pid")" 2>/dev/null; }'

# --- 9. netage-wanpref already running, 8be5f2b init, 6af8aa1 ramboot.sh -------------
printf '\n%s: netage-wanpref running and enabled\n' "$P"
new_unit "$P-wanpref" init-8be5f2b ramboot-6af8aa1
cp "$FIX/fake/wanpref" "$R/etc/init.d/netage-wanpref"
ln -s ../init.d/netage-wanpref "$R/etc/rc.d/S99netage-wanpref"
sleep 300 &
echo $! > "$R/var/run/netage-wanpref.pid"
boot_unit
refresh FAKE_NEW_HEALTH=ok
check "state done" '[ "$STATE" = done ]' "state=$STATE $(log_tail)"
check "the log says wanpref runs and is unchanged" 'in_log "netage-wanpref runs (pid"'
kill "$(cat "$R/var/run/netage-wanpref.pid")" 2>/dev/null

# --- 10. the cache no longer holds an old payload -----------------------------------
printf '\n%s: an old payload not cached\n' "$P"
new_unit "$P-uncached"
boot_unit
rm -f "$R/tmp/sctl/cache/musl-libc.payload"
snapshot "$T/before"
refresh FAKE_NEW_HEALTH=never
check "state rolled_back" '[ "$STATE" = rolled_back ]' "state=$STATE $(log_tail)"
check "the rollback put the fetched copy in the cache: no network at start" \
    '[ ! -s "$T/net.log" ] && [ "$(running)" = "$OLD_VERSION" ] && same_file "$R/tmp/sctl/cache/musl-libc.payload" "$FIX/mirror/libc-mips_24kc-a9227a8.so.gz"'

# --- 11. the probe ---------------------------------------------------------------
printf '\n%s: probe\n' "$P"
new_unit "$P-toml"
boot_unit
refresh FAKE_TOML_BAD=1
check "an sctl.toml the new server cannot read: refused before anything moved" \
    '[ "$STATE" = failed ] && unchanged && [ ! -s "$T/rc.log" ] && nothing_left && in_log "cannot read this unit"' \
    "state=$STATE $(log_tail)"
new_unit "$P-wrongversion"
boot_unit
ARGS="0.6.12.200 4" refresh
check "a payload that is not the version asked for: failed, nothing changed" \
    '[ "$STATE" = failed ] && unchanged && [ ! -s "$T/rc.log" ] && in_log "is not 0.6.12.200"'

# --- 12. guards -------------------------------------------------------------------
printf '\n%s: guards\n' "$P"
new_unit "$P-space"
boot_unit
used=$(du -sk "$R/tmp" | cut -f1)
refresh FAKE_TMP_KB=$((24576 + used + 100))
check "/tmp short of the floor plus this run: failed before any fetch, nothing changed" \
    '[ "$STATE" = failed ] && unchanged && [ ! -s "$T/curl.log" ] && in_log "on top of the 24576 KB"' "state=$STATE $(log_tail)"
new_unit "$P-flash"
boot_unit
refresh FAKE_FLASH_KB=10
check "not enough room on the overlay: failed before any fetch, nothing changed" \
    '[ "$STATE" = failed ] && unchanged && [ ! -s "$T/curl.log" ] && in_log "the overlay has"'
new_unit "$P-silent"
boot_unit
env -i $(unit_env) "$FIX/fake/rc" "$R/etc/init.d/sctl" stop > /dev/null 2>&1
: > "$T/rc.log"
refresh
check "no agent answering /api/health: failed, nothing changed" \
    '[ "$STATE" = failed ] && unchanged && [ ! -s "$T/rc.log" ] && in_log "does not answer"'
new_unit "$P-tampered"
boot_unit
echo "# changed in transit" >> "$STAGE/ramboot.sh"
refresh
check "a staged file that is not what was sent: failed, nothing changed" \
    '[ "$STATE" = failed ] && unchanged && in_log "staged ramboot.sh is not what was sent"'
new_unit "$P-inflight"
boot_unit
mkdir -p "$R/tmp/sctl-we826-refresh-rollback"
: > "$R/tmp/sctl-we826-refresh-rollback/in-flight"
refresh
check "an unfinished earlier move: refused, its backup kept" \
    '[ "$STATE" = failed ] && unchanged && [ -e "$R/tmp/sctl-we826-refresh-rollback/in-flight" ]'
new_unit "$P-relaunch"
boot_unit
mkdir "$STAGE/lock"
refresh
check "a second launch while one runs does nothing" \
    '[ -z "$STATE" ] && unchanged && [ ! -s "$T/rc.log" ]' "state=$STATE"
new_unit "$P-deadline"
boot_unit
ARGS="$NEW_VERSION 4 0" refresh
check "the time before the move spent: failed, the old agent never stopped" \
    '[ "$STATE" = failed ] && unchanged && [ ! -s "$T/rc.log" ] && nothing_left && in_log "before the move are spent"' "state=$STATE $(log_tail)"

# --- 13. the old agent cannot be stopped ------------------------------------------------
printf '\n%s: the old agent does not stop\n' "$P"
new_unit "$P-nostop"
boot_unit
refresh FAKE_STRAY_AGENT=1
check "failed: nothing written, the payload copies freed before the init started the old agent again" \
    '[ "$STATE" = failed ] && unchanged && nothing_left && [ "$(rc_seq)" = "stop $OLD_INIT flash start $OLD_INIT flash " ] && in_log "could not be stopped cleanly"' \
    "state=$STATE rc.log=$(rc_seq) $(log_tail)"
check "the old agent runs again" 'agent_up && [ "$(running)" = "$OLD_VERSION" ]'

# --- 14. no tunnel at all -----------------------------------------------------------------
printf '\n%s: a unit with no tunnel\n' "$P"
new_unit "$P-notunnel"
sed -i '/^\[tunnel\]/,$d' "$R/etc/sctl/sctl.toml"
boot_unit
refresh FAKE_OLD_TUNNEL=none FAKE_NEW_HEALTH=never
check "healthy is the version alone: done" \
    '[ "$STATE" = done ] && [ "$(running)" = "$NEW_VERSION" ] && in_log "(no tunnel configured)"' "state=$STATE $(log_tail)"

# --- 15. failures inside the move ------------------------------------------------------------
printf '\n%s: a write, a cache move and a restore that fail\n' "$P"
new_unit "$P-failwrite"
boot_unit
refresh FAKE_NEW_HEALTH=ok REFRESH_FAULTS=put:install.json
check "install.json could not be written: rolled back, every file as it was" \
    '[ "$STATE" = rolled_back ] && unchanged && [ "$(running)" = "$OLD_VERSION" ] && in_log "could not write install.json"' \
    "state=$STATE $(diff "$T/before" "$T/after" | tr '\n' '|') $(log_tail)"
new_unit "$P-failcache"
boot_unit
refresh FAKE_NEW_HEALTH=ok REFRESH_FAULTS=cache:LIBGCC
check "a payload could not go into the cache: rolled back before any trial" \
    '[ "$STATE" = rolled_back ] && unchanged && [ "$(running)" = "$OLD_VERSION" ] && ! grep -q trial "$T/agents.log"' \
    "state=$STATE $(agents_seq) $(log_tail)"
new_unit "$P-failrestore"
boot_unit
refresh FAKE_NEW_HEALTH=never REFRESH_FAULTS=restore:init
check "the init could not be restored: needs_hands, the marker kept, no reboot" \
    '[ "$STATE" = needs_hands ] && [ -e "$R/tmp/sctl-we826-refresh-rollback/in-flight" ] && [ ! -e "$T/reboot.log" ] && in_log "could not restore"' \
    "state=$STATE $(log_tail)"
check "  the old version answers all the same (the old ramboot.conf is back)" '[ "$(running)" = "$OLD_VERSION" ]'

# --- 16. the old agent does not come back after a rollback ----------------------------------
printf '\n%s: the old agent without its tunnel after the rollback\n' "$P"
new_unit "$P-reboot"
boot_unit
grep '^[0-9]* [0-9a-f]* etc/' "$T/before" > "$T/before.etc"
refresh FAKE_NEW_HEALTH=never FAKE_OLD_TUNNEL=down_after_move
check "the unit rebooted into the old layout" \
    '[ -s "$T/reboot.log" ] && grep -q "state: rebooting" "$T/log-before-reboot" && grep -q "rebooting into it" "$T/log-before-reboot"' \
    "$(tail -n 8 "$T/log-before-reboot" 2>/dev/null | tr '\n' '|')"
check "flash held the old files, byte for byte" 'grep " etc/" "$T/after" | cmp -s - "$T/before.etc"' \
    "$(grep ' etc/' "$T/after" | diff "$T/before.etc" - | tr '\n' '|')"
check "the cold boot fetched the old payloads from their URLs and runs the old version" \
    'agent_up && [ "$(running)" = "$OLD_VERSION" ] && grep -q "GET $OLD_MIRROR/sctl-server-mips_24kc-a9227a8.gz" "$T/net.log"' \
    "running=$(running) net=$(tr '\n' ' ' < "$T/net.log")"

}

printf '\nthe table of released files\n'
check "the current shared init and ramboot.sh are named in the script's table" \
    'grep -q "^ *$(sha "$COMMON_DIR/sctl-ramboot.init")) " "$SCRIPT" && grep -q "^ *$(sha "$COMMON_DIR/sctl-ramboot.sh")) " "$SCRIPT"'
check "so are the released files the tests install" \
    '( for f in init-0531 init-8be5f2b init-c97ad7a ramboot-0531 ramboot-6af8aa1; do grep -q "^ *$(sha "$FIX/released/$f")) " "$SCRIPT" || exit 1; done )'

suite busybox-ash
UNIT_SH=$(command -v dash)
suite dash
check "under dash the unit's script hashed with openssl" 'grep -q "SHA-256 by openssl" "$TOP/dash-happy/root/tmp/sctl-we826-refresh.log"'

printf '\n  %d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
