# GL.iNet GL-XE300 (Puli)

Fixed-site / bench sctl target. Structurally this device is a **RUT241, not a WE826**:
real OpenWrt with procd, musl, and a large writable overlay, installed over SSH.

## Target

| | |
|---|---|
| SoC | Qualcomm Atheros QCA9533 ver 2, MIPS 24Kc |
| Arch | `mips_24kc` **big-endian**, soft-float musl 1.1.24 |
| Rust target | `mips-unknown-linux-musl` |
| OS | OpenWrt 19.07.8 r11364, `ath79/nand`, GL.iNet firmware 3.x |
| Overlay | UBI/ubifs, ~100 MB (~95 MB free) |
| RAM | 128 MB (60 MB tmpfs) |
| Modem | Quectel **EC25-AFXD** (`2c7c:0125`), QMI composition: `cdc-wdm0` + `wwan0` |
| Ethernet | 2 external ports: `eth0` (LAN, in `br-lan`), `eth1` (WAN) |
| GPS | **None.** The EC25 GNSS die powers on but no antenna is routed to it, and the GL firmware has no GPS plumbing at all. Do not add a `[gps]` section: it only buys pointless AT traffic on the shared port |

## Build

```sh
devices/build.sh xe300
```

Artifacts land in `.artifacts/xe300/`:

- `sctl-server-mips_24kc.gz`
- `sctl-comms-quectel-mips_24kc.so.gz`

**Only two payloads.** The WE826 additionally ships musl + libgcc because its stock
firmware is uClibc with no OpenWrt loader. The XE300 *is* OpenWrt 19.07 with musl
1.1.24 and runs these binaries under its own `/lib/libc.so`, verified on hardware.

**The build deliberately reuses the ath79-`generic` target script on an ath79-`nand`
device.** That is not an oversight. The OpenWrt generic/nand split is a kernel and
flash-image distinction; both subtargets ship gcc 7.5.0 + musl and unpack the same
`toolchain-mips_24kc_gcc-7.5.0_musl`, so the userland binaries are interchangeable.
The 19.07.10 SDK against a 19.07.8 device is likewise fine inside the series.

## Install

```sh
API_KEY=... SERIAL=XE300-<MAC> devices/xe300/install.sh root@192.168.8.1
```

Payloads go to `/usr/local/lib/sctl/*.gz`; the procd init expands them into
`/tmp/sctl` at start and supervises with `respawn 5 10 0`.

`data_dir` is `/usr/local/lib/sctl/data` on the **overlay**, not tmpfs, so
`connection_history.jsonl` and `safe_mode.flag` survive a reboot. On the RAM-booted
WE826 they do not, which is why a field failure there leaves no forensic trail.

`touch /etc/sctl/disabled` keeps the agent down across reboots for field triage.

### The relay route

`install.sh` writes `relay_route = "follow_default"` into `[tunnel]` (`RELAY_ROUTE`,
default `follow_default`). The agent then keeps the host route to the relay on the
best uplink that answers and moves its tunnel without a restart (`docs/config.md`,
`relay_route`). A tunnel pinned with `bind_address` keeps its pin and gets no
`relay_route`: the agent refuses both together.

That replaces two shell hotplugs, which `install.sh` removes together with the pin
they left (`relay-route.sh`), keeping them in `/tmp/sctl-relay-route.backup.<stamp>`:

- `/etc/hotplug.d/iface/96-wg-repin` pinned the WireGuard endpoint, which is the
  relay, at metric 0 through the lowest-metric default route. The agent never
  replaces a route it did not install, so that pin would keep it out.
- `/etc/hotplug.d/iface/97-sctl-rehome` restarted sctl when the wan came up.

Failing over, and coming back, needs `rp_filter` at 0 or 2 on the uplinks: the
agent's probe of an uplink the route does not use is answered on that uplink, and
strict filtering (1) drops the answer, so that uplink would stay suspect for good.
With the agent keeping the route, `install.sh` (and the remote upgrade) runs
`relay-route.sh rp-filter`, which makes it 2 (loose) at once for `all`, `default`
and every interface, and keeps `net.ipv4.conf.all.rp_filter=2` and
`net.ipv4.conf.default.rp_filter=2` in `/etc/sysctl.conf` (the file the fleet's
onboarding already uses for `ignore_routes_with_linkdown`) for every boot. Run
again, it changes nothing. The agent names any uplink still strict in
`/api/health` under `tunnel.relay_route.rp_filter_strict`.

### Remote upgrade: `rundev.sh device upgrade-remote`

From 0.6.7 a unit upgrades itself (`docs/upgrade.md`): the fleet asks it for a
version its relay serves, and the agent fetches the signed bundle, verifies it,
stages it, swaps the payloads through `sctl upgrade-apply` (the `gz-tmp` layout
named in `/etc/sctl/install.json`, which `install.sh` and `upgrade.sh` both
write), restarts once, and rolls back unless healthy. The path below is the
hand path: the last one a unit needs (it writes `install.json`), and bring-up on
a bench unit afterwards.

`rundev.sh device upgrade-remote <name>` upgrades a unit through the relay, with no
SSH. It asks the device how sctl is installed, and when it finds this layout
(`/usr/local/lib/sctl/sctl-server-mips_24kc.gz` under a procd init) it:

1. builds the payloads with `devices/build.sh xe300`: the OpenWrt SDK build, not the
   generic `cross` binary, which also does not fit `/usr/bin/sctl`;
2. stages them in `/tmp/sctl-xe300-upgrade` over STP, with `upgrade.sh` and
   `relay-route.sh` through the file API, then checks their hashes and that the
   server runs (`--version`);
3. refuses a `[tunnel]` pinned with `bind_address` before shipping anything, and
   says where `rp_filter` is strict today;
4. starts `upgrade.sh` on the device, detached. It copies the current payloads,
   `sctl.toml` and hotplugs to `/usr/local/lib/sctl/rollback` (kept afterwards),
   installs the payloads, sets `[tunnel] relay_route` (every other line of
   `sctl.toml` stays as it is), removes the two hotplugs and their pin, makes
   `rp_filter` loose as above, and restarts the agent once;
5. puts all of it back and restarts again unless, within 180 s, `/api/health` on
   the device reports the new version and a connected tunnel twice in a row. The
   loose `rp_filter` stays: it is harmless to any agent, and `bind_address` needs
   it too.

Progress is in `/tmp/sctl-xe300-upgrade/state` and `/tmp/sctl-xe300-upgrade.log` on
the device, and `rundev.sh` prints that log when the upgrade rolls back.
`RELAY_ROUTE=off` upgrades the payloads only, leaving `sctl.toml`, the hotplugs and
`rp_filter` alone.

`rundev.sh device upgrade` (over SSH) still does not handle this layout: `uname -m`
is `mips`, which has no `ARCH_TARGET` entry. Over SSH, use `install.sh`, which
preserves the `[tunnel]` block.

### Bench check: can netifd leave the relay alone?

netifd's WireGuard handler pins a `/32` to each peer endpoint, which is the relay,
through whichever uplink was up when `wg0` came up. The agent's metric 0 route
outranks that pin, so the pin does no harm, but newer handlers read
`option nohostroute '1'` on the interface and then add no pin at all. Before
deciding whether the upgrade should set it, check on a bench unit which handler
the firmware ships:

```sh
grep -n nohostroute /lib/netifd/proto/wireguard.sh
```

Over SSH (see below for the options this dropbear needs):

```sh
ssh -o 'HostKeyAlgorithms=+ssh-rsa' -o 'PubkeyAcceptedAlgorithms=+ssh-rsa' \
    root@192.168.8.1 'grep -n nohostroute /lib/netifd/proto/wireguard.sh'
```

Or through the relay, with the unit's API key and no SSH:

```sh
curl -sf -X POST "https://<relay>/d/<serial>/api/exec" \
    -H "Authorization: Bearer $API_KEY" -H 'Content-Type: application/json' \
    -d '{"command": "grep -n nohostroute /lib/netifd/proto/wireguard.sh"}' | jq .
```

- **Lines printed** (exit code 0): the handler supports it. The upgrade could then
  set it on `wg0` (`uci set network.wg0.nohostroute='1'`, `uci commit network`,
  `ifup wg0`) so that netifd never pins the endpoint and the agent's route is the
  only one to the relay.
- **Nothing printed** (exit code 1): this firmware's handler always pins the
  endpoint, and the agent's route outranking the pin is the whole answer.
- **No such file** (exit code 2): WireGuard is not set up through netifd here;
  look at how `wg0` is brought up before deciding anything.

Neither `install.sh` nor the remote upgrade sets `nohostroute` today: it waits
for this check.

## SSH access

GL.iNet firmware 3.x ships **dropbear 2019.78**, which:

- offers an **`ssh-rsa` host key only**, and
- **predates ed25519 client-key support** (added in dropbear 2020.79).

So the client key must be **RSA**, and modern OpenSSH needs both algorithms
re-enabled:

```sh
ssh -o 'HostKeyAlgorithms=+ssh-rsa' -o 'PubkeyAcceptedAlgorithms=+ssh-rsa' root@192.168.8.1
```

Quote each `-o` separately; building them in a shell variable and expanding it
unquoted fails with `keyword hostkeyalgorithms extra arguments at end of line`.
Legacy KEX is **not** needed (2019.78 does curve25519), and plain `scp` works (no
`-O`).

**A blank root password makes dropbear accept the `none` auth method**, so an
`ssh -o PreferredAuthentications=publickey` test against an unconfigured unit
succeeds *without ever validating the key*. Always set the root password first, then
verify key auth with a negative control (a key that is not in `authorized_keys` must
be refused).

## LTE

`[lte] interface` must name the netdev that actually holds the bearer's IPv4 and
default route, because the plugin derives `data_active` from
`interface_has_ipv4(interface)`. Naming a control-only netdev is the long-standing
NOCONN bug.

On this device that is **`wwan0`**, not the RUT241's `qmimux0`: the EC25-AFXD comes up
in the QMI composition with no `usb0`/`qmimux*`, and `qmi_wwan` is in 802.3 mode
(`/sys/class/net/wwan0/qmi/raw_ip` = `N`), so `wwan0` carries the address itself.
**Re-confirm on the first SIM**: the data call is dialled by GL's `gl_modem connect`,
not netifd (there is no modem interface in uci `network` at all), and it picks the
ifname at dial time.

## Vendor firmware notes

GL.iNet's cloud stack ships enabled in `rc.d`. On a fresh unit, disable it before the
device is given an uplink:

```sh
for s in gl_mqtt gl_bigdata gl_tertf gl_s2s siderouter; do
    /etc/init.d/$s disable; /etc/init.d/$s stop
done
```

`gl_bigdata` uploads to `https://telemetry.goodcloud.xyz` (and has an
`upload_scan_aps` flag). `gl_mqtt` is gated on `glconfig.cloud.enable`. These are UCI
flags a factory reset, a firmware upgrade, or anyone completing the setup wizard can
flip back, so disabling the init scripts is the durable half. Leave `gl_health`,
`gl_monitor` and `gl_led` alone: they are local-only.

`/etc/init.d/modem-init` issues `AT+QNVFW` + **`AT+CFUN=1,1`** once at boot to force
data-centric mode. That is a deliberate modem reset and re-enumerates the USB bus,
which is another reason the AT port must never be hardcoded.
