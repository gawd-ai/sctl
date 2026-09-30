#!/usr/bin/env bash
# Unit tests for the [tunnel] block install.sh writes.
#
# install.sh is run for real, with ssh and scp pointed at fakes: the fake ssh
# answers the three remote reads (overlay space, replaceable files, the existing
# [tunnel] block) and swallows the final install script; the fake scp captures
# what would land on the device. The assertions are about the sctl.toml that
# would be written, since that file is the whole point of the script.
#
# The load-bearing tests are the default block (relay_route = "prefer" listing
# the wire before LTE, no bind_address) and `preserved_block_is_verbatim`: a
# re-install without TUNNEL_URL/TUNNEL_KEY must carry the device's block forward
# untouched, which is what stops a re-install from stranding a unit that is only
# reachable over the tunnel.

set -u

HERE=$(cd -- "$(dirname -- "$0")" && pwd)
INSTALL="$HERE/../install.sh"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

PASS=0; FAIL=0
ok()   { PASS=$((PASS+1)); printf '  ok    %s\n' "$1"; }
bad()  { FAIL=$((FAIL+1)); printf '  FAIL  %s\n     %s\n' "$1" "$2"; }

mkdir -p "$WORK/fake" "$WORK/cap"
cat > "$WORK/fake/ssh" <<'FAKE'
#!/usr/bin/env bash
cmd="${@: -1}"
case "$cmd" in
  # The remote commands carry their own awk, so the fake answers the number.
  *"df -k"*) echo 600;;
  *"du -k"*) echo 0;;
  *"awk"*) printf '%s\n' "${FAKE_EXISTING_TUNNEL:-}";;
  "sh -s") cat >/dev/null;;
  *) echo "fake ssh: unexpected command: $cmd" >&2; exit 9;;
esac
FAKE
cat > "$WORK/fake/scp" <<'FAKE'
#!/usr/bin/env bash
files=(); for a in "$@"; do case "$a" in -*) ;; *:*) ;; *) files+=("$a");; esac; done
cp "${files[@]}" "$CAP_DIR/"
FAKE
chmod +x "$WORK/fake/ssh" "$WORK/fake/scp"

# Run install.sh with the given extra environment; stdout goes to $WORK/out,
# stderr to $WORK/err, and the sctl.toml it would ship to $WORK/cap/sctl.toml.
run() {
    rm -f "$WORK/cap"/*
    CAP_DIR="$WORK/cap" SSH="$WORK/fake/ssh" SCP="$WORK/fake/scp" \
    API_KEY=k SERVER_URL=u PLUGIN_URL=p MUSL_LIBC_URL=m LIBGCC_URL=l ALLOW_UNSIGNED=1 \
    env "$@" "$INSTALL" root@fake > "$WORK/out" 2> "$WORK/err"
}

# The [tunnel] block as shipped, comments stripped.
tunnel_block() {
    sed -n '/^\[tunnel\]/,$p' "$WORK/cap/sctl.toml" 2>/dev/null | grep -v '^#'
}

has()      { tunnel_block | grep -qxF -- "$1"; }
summary()  { grep '^tunnel:' "$WORK/out"; }

printf '\ninstall.sh [tunnel] block\n'

# --- 1. the default: unpinned, sctl prefers the wire and keeps the route -------
run TUNNEL_URL=wss://r/api/tunnel/register TUNNEL_KEY=kk
if has 'relay_route = "prefer"' && has 'relay_route_prefer = ["eth0", "usb0"]'; then
    ok "default block prefers eth0 then usb0"
else
    bad "default block prefers eth0 then usb0" "$(tunnel_block | tr '\n' ';')"
fi
if tunnel_block | grep -q 'bind_address'; then
    bad "default block has no bind_address" "$(tunnel_block | tr '\n' ';')"
else
    ok "default block has no bind_address"
fi
if has 'url = "wss://r/api/tunnel/register"' && has 'tunnel_key = "kk"'; then
    ok "url and key come from the environment"
else
    bad "url and key come from the environment" "$(tunnel_block | tr '\n' ';')"
fi
if summary | grep -qF 'relay_route prefer ["eth0", "usb0"]'; then
    ok "the summary line says what was written"
else
    bad "the summary line says what was written" "$(summary)"
fi

# --- 2. a pin still works, and turns relay_route off (the two conflict) --------
run TUNNEL_URL=wss://r/api/tunnel/register TUNNEL_KEY=kk TUNNEL_BIND_ADDRESS=usb0
if has 'bind_address = "usb0"' && has 'relay_route = "off"' && ! tunnel_block | grep -q relay_route_prefer; then
    ok "TUNNEL_BIND_ADDRESS pins and turns relay_route off"
else
    bad "TUNNEL_BIND_ADDRESS pins and turns relay_route off" "$(tunnel_block | tr '\n' ';')"
fi
if summary | grep -qF 'pinned to usb0, relay_route off'; then
    ok "the summary line names the pin"
else
    bad "the summary line names the pin" "$(summary)"
fi

# --- 3. a custom order -------------------------------------------------------
run TUNNEL_URL=wss://r/api/tunnel/register TUNNEL_KEY=kk RELAY_ROUTE_PREFER=eth1,eth0,usb0
if has 'relay_route_prefer = ["eth1", "eth0", "usb0"]'; then
    ok "RELAY_ROUTE_PREFER sets the order"
else
    bad "RELAY_ROUTE_PREFER sets the order" "$(tunnel_block | tr '\n' ';')"
fi

# --- 4. an empty list: off, the tunnel follows the default route ---------------
run TUNNEL_URL=wss://r/api/tunnel/register TUNNEL_KEY=kk RELAY_ROUTE_PREFER=
if has 'relay_route = "off"' && ! tunnel_block | grep -q 'relay_route_prefer\|bind_address'; then
    ok "an empty RELAY_ROUTE_PREFER writes relay_route off"
else
    bad "an empty RELAY_ROUTE_PREFER writes relay_route off" "$(tunnel_block | tr '\n' ';')"
fi

# --- 5. a bad list stops the install before anything is copied ----------------
for bad_list in 'eth0,,usb0' 'eth 0' 'an-interface-name-too-long'; do
    run TUNNEL_URL=wss://r/api/tunnel/register TUNNEL_KEY=kk RELAY_ROUTE_PREFER="$bad_list"
    status=$?
    if [ "$status" -ne 0 ] && [ ! -e "$WORK/cap/sctl.toml" ] && grep -q 'is not an interface name' "$WORK/err"; then
        ok "RELAY_ROUTE_PREFER='$bad_list' is refused and nothing is copied"
    else
        bad "RELAY_ROUTE_PREFER='$bad_list' is refused and nothing is copied" "status=$status err: $(tr '\n' ';' < "$WORK/err")"
    fi
done

# --- 6. without TUNNEL_URL/TUNNEL_KEY the device's block is carried verbatim ---
existing='[tunnel]
tunnel_key = "old-key"
url = "wss://old/api/tunnel/register"
# a comment the operator left
bind_address = "usb0"
heartbeat_interval_secs = 7'
run FAKE_EXISTING_TUNNEL="$existing"
shipped=$(sed -n '/^\[tunnel\]/,$p' "$WORK/cap/sctl.toml")
if [ "$shipped" = "$existing" ]; then
    ok "preserved_block_is_verbatim (comments and pin included)"
else
    bad "preserved_block_is_verbatim" "shipped: $(printf '%s' "$shipped" | tr '\n' ';')"
fi
if summary | grep -q 'PRESERVED from device (6 lines)'; then
    ok "the summary line says the block was preserved"
else
    bad "the summary line says the block was preserved" "$(summary)"
fi

# --- 7. no block anywhere: none written, none invented ------------------------
run
if ! grep -q '^\[tunnel\]' "$WORK/cap/sctl.toml" && summary | grep -q 'none (device has no \[tunnel\] block)'; then
    ok "no TUNNEL_URL and no existing block writes no [tunnel] at all"
else
    bad "no TUNNEL_URL and no existing block writes no [tunnel] at all" "$(summary)"
fi

# --- 8. one without the other is an error --------------------------------------
run TUNNEL_URL=wss://r/api/tunnel/register
if [ $? -ne 0 ] && grep -q 'must be set together' "$WORK/err"; then
    ok "TUNNEL_URL without TUNNEL_KEY is refused"
else
    bad "TUNNEL_URL without TUNNEL_KEY is refused" "$(cat "$WORK/err")"
fi

printf '\n  %d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
