# ZBTLink WE826-Q-WD

Deployment assets for running `sctl` on the ZBTLink WE826-Q-WD / QCA9531 4G router without flashing a custom firmware image.

This device has only about 832 KB of writable overlay, so the install writes only a small persistent bootstrap:

```text
/etc/init.d/sctl              procd service
/etc/sctl/ramboot.sh          fetch/verify/expand wrapper
/etc/sctl/ramboot.conf        payload URLs and hashes
/etc/sctl/sctl.toml           runtime server config
/etc/sctl/install.json        layout, target and loader prefix (docs/upgrade.md)
/tmp/sctl/sctl-server         runtime expanded server binary
/tmp/sctl/lib/*.so            runtime expanded comms plugin
```

The server binary and comms plugin are downloaded into `/tmp` at boot, verified by SHA-256, expanded there, and executed from RAM. This avoids storing multi-megabyte payloads in flash and keeps all changes reversible from SSH or the web UI.

The stock firmware is uClibc-based, while the sctl payloads are built against OpenWrt musl. The RAM boot bundle includes musl `libc.so` and `libgcc_s.so.1`; the bootstrap invokes the musl loader directly from `/tmp`, so no files need to be added under `/lib`.

## Device Notes

Observed target:

```text
OS:        QSDK / OpenWrt-derived ar71xx
Kernel:    Linux 3.3.8
CPU:       Qualcomm Atheros QCA9531, MIPS 24Kc, big-endian
RAM:       128 MB
Flash:     16 MB SPI NOR
Overlay:   /dev/mtdblock3, about 832 KB total
Runtime:   /tmp tmpfs, about 61 MB total
Modem:     Quectel EC25-AFXD, AT on /dev/ttyUSB2
Network:   LTE interface usb0
```

Do not flash custom firmware on this device unless you have a verified recovery path. The RAM boot path is the intended deployment model.

## Install

The unit boots its payloads from a relay's plain-HTTP mirror of a release
(docs/upgrade.md, "Bundle retention"): name the mirror and the script reads
every URL and SHA-256 from its `release.json`, writes `ramboot.conf` and an
`install.json` with the `helper_prefix` the upgrade helper needs under this
unit's musl loader. From then on the unit upgrades itself when the fleet asks
(the relay keeps the previous bundle until the unit is past it).

```sh
API_KEY=... \
MIRROR_BASE=http://174.138.114.209:8081/artifacts/0.6.9.200 \
TUNNEL_URL=wss://relay-001.netage.ai/api/tunnel/register TUNNEL_KEY=... \
devices/we826-qwd/install.sh root@ROUTER_IP
```

For a bench without a relay, provide the URLs for the compressed server and plugin payloads yourself. Hashes are for the downloaded payload files, before gzip expansion.

Build local payload artifacts:

```sh
devices/build.sh we826-qwd
```

This creates:

```text
.artifacts/we826-qwd/sctl-server-mips_24kc.gz
.artifacts/we826-qwd/sctl-comms-quectel-mips_24kc.so.gz
.artifacts/we826-qwd/libc-mips_24kc.so.gz
.artifacts/we826-qwd/libgcc_s-mips_24kc.so.1.gz
```

```sh
API_KEY=... \
SERVER_URL=https://example.invalid/sctl-server-mips.gz \
SERVER_SHA256=... \
PLUGIN_URL=https://example.invalid/sctl-comms-quectel-mips.so.gz \
PLUGIN_SHA256=... \
MUSL_LIBC_URL=https://example.invalid/libc.so.gz \
MUSL_LIBC_SHA256=... \
LIBGCC_URL=https://example.invalid/libgcc_s.so.1.gz \
LIBGCC_SHA256=... \
devices/we826-qwd/install.sh root@ROUTER_IP
```

For the stock WE826-Q-WD SSH server, you may need legacy SSH options:

```sh
SSH_OPTS='-oKexAlgorithms=+diffie-hellman-group14-sha1,diffie-hellman-group1-sha1 -oHostKeyAlgorithms=+ssh-rsa -oPubkeyAcceptedAlgorithms=+ssh-rsa' \
API_KEY=... SERVER_URL=... SERVER_SHA256=... PLUGIN_URL=... PLUGIN_SHA256=... \
devices/we826-qwd/install.sh root@ROUTER_IP
```

Set `INSTALL_DISABLED=1` to install the files without starting the service. Remove `/etc/sctl/disabled` and restart the service when the payload server is ready:

```sh
rm -f /etc/sctl/disabled
/etc/init.d/sctl restart
```

## A unit installed before 0.6.9

A unit whose agent is older than 0.6.7 has no `install.json` and no upgrade
route, so the fleet cannot ask it (Bus 01 runs 0.6.2); one installed by 0.6.7
or 0.6.8 has an `install.json` without `helper_prefix`, so its upgrade helper
cannot start under the loader. It moves once, through the relay and with no
SSH, to the current layout and a current release
([TRD-8](../../docs/trd/TRD-8-we826-guarded-move-to-the-current-ramboot-layout.md)):

```sh
./rundev.sh device upgrade-remote <name> <version> http://174.138.114.209:8081/artifacts/<version>
# or name the relay and let its listing give the mirror (it must be by IP for
# a unit that fetches by IP, as Bus 01 does):
./rundev.sh device upgrade-remote <name> <version> root@174.138.114.209
```

`rundev.sh` checks the release's signature and the mirror's bytes, refuses a
mirror by name for a unit whose `ramboot.conf` names its mirror by IP, stages
`ramboot-refresh.sh` (with `refresh-stage.sh`) and follows its state. On the
unit the script refuses a layout it does not recognise and anything the move
would change beyond the agent (`relay_route` the old agent does not run, a
`netage-wanpref` that is installed but not running and enabled), fetches and
proves the new payloads (the server runs through the new loader and reads
this unit's `sctl.toml`), fetches the old payloads from their URLs into RAM,
then stops the old agent, writes the current init and `ramboot.sh`, and runs
the new version from a RAM copy of its `ramboot.conf`: flash still boots the
old version. Only when the new agent answers with the version and a tunnel
that holds for 60 s does it write `ramboot.conf` and `install.json` and restart
once more from flash. Otherwise it puts everything back byte for byte (the
state dir included) and the old agent starts from its cache, with no network;
if the old agent does not come back with its tunnel, the unit reboots into
the old layout. `sctl.toml` is never written; the unit's fetch settings
(`FETCH_ATTEMPTS` and friends) are kept. A power cut at any step boots one
version whole.

`WE826_WAIT_SECS` (300) is how long the new agent has to become healthy, and
the old one to come back; `WE826_PREMOVE_SECS` (1800) bounds the fetches,
after which nothing moves. The states are in `/tmp/sctl-we826-refresh/state`
while it runs (`rundev.sh` removes that directory after `done`); the log
stays in `/tmp/sctl-we826-refresh.log` and the old files in
`/tmp/sctl-we826-refresh-rollback` until the next boot. A unit whose
`install.json` names the loader is refused: it takes managed upgrades.

Run it with the unit parked and powered (it is ignition-fed on a bus), on its
wire when there is one. The tests (`tests/ramboot-refresh-test.sh`,
`tests/upgrade-remote-test.sh`) need `busybox`, `dash`, `jq`, `openssl`,
`xxd`, `git` (they run the released init files and `ramboot.sh` from the
history) and a coreutils `sleep`.

## Safety

- Keep `ALLOW_UNSIGNED=0` for real deployments.
- Keep payload URLs and hashes out of tracked source files.
- Use `/etc/sctl/disabled` as the fast local kill switch.
- Use `sysupgrade --test` only for firmware research; this deployment does not require firmware flashing.
- **`install.sh` overwrites `/etc/sctl/sctl.toml` wholesale.** The `[tunnel]` block is
  per-deployment and is therefore *not* in the template. To stop a re-install silently
  changing how a unit that is only reachable over that tunnel routes it, `install.sh` either
  writes a fresh block from `TUNNEL_URL`/`TUNNEL_KEY` (with `RELAY_ROUTE_PREFER` or
  `TUNNEL_BIND_ADDRESS`), or, when those are not supplied, reads the existing block off the
  device and carries it forward verbatim. It prints which path it took. Check that line on
  every install against an onboarded unit.
- **The default block is unpinned: sctl keeps the relay's route itself.** It writes
  `relay_route = "prefer"` and `relay_route_prefer = ["eth0", "usb0"]` (`RELAY_ROUTE_PREFER`,
  comma-separated). The vendor dial script installs LTE's default route at metric 0 on every
  dial, so `"follow_default"` would keep the relay on the SIM; `"prefer"` names the wire
  first, and sctl's own probes still move the relay's `/32` to LTE when the wire cannot
  reach the relay and back when it can (`docs/config.md`, `relay_route`). `netage-wanpref`
  keeps owning the default route for passenger traffic; the two never touch the same route.
- **`TUNNEL_BIND_ADDRESS` still pins a unit that must stay on one uplink**, and turns
  `relay_route` off, since the two conflict. It must be an interface *name*, not an IP: sctl
  only applies `SO_BINDTODEVICE` when the value fails to parse as an IP address
  (`server/src/tunnel/client.rs`). An IP literal sets only the source address, which leaves
  tunnel egress following the default route, exactly what pinning is meant to prevent.
- **A pin on the cellular interface is a metering decision, not a routing one.** With
  `bind_address` on the LTE bearer, every byte of the management plane (tunnel heartbeats,
  the fleet's health, uplink and infra polls, sessions) rides the SIM even while a wired WAN
  is up. The RUT241 at Canphone did that for three months at ~50 MB/day. Pin the wired side
  when a pin is wanted, or leave the default so the relay rides the wire and fails over to
  cellular on sctl's own evidence.
