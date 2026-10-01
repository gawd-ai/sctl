# Teltonika RUT241

Device-specific deployment assets for running `sctl` on a Teltonika RUT241/RUT2M class router.

The RUT241 has a very small writable overlay, so this deployment stores compressed payloads in persistent flash and expands them into `/tmp` at service start:

```text
/usr/local/lib/sctl/*.gz      persistent compressed payloads
/etc/init.d/sctl              procd service
/etc/sctl/sctl.toml           small config only
/tmp/sctl/sctl-server         runtime expanded binary
/tmp/sctl/lib/*.so            runtime expanded comms plugin
```

Known target:

```text
OpenWrt target: ramips/mt76x8
Rust target:    mipsel-unknown-linux-musl
CPU:            MediaTek MT7628AN / mipsel_24kc
Modem:          Quectel EC25-AF(D)
AT port:        /dev/ttyUSB2
LTE interface:  wwan0
```

Build artifacts:

```sh
devices/build.sh rut241
```

This creates the payloads expected by `install.sh`:

```text
.artifacts/rut241/sctl-server-mipsel_24kc.gz
.artifacts/rut241/sctl-comms-quectel-mipsel_24kc.so.gz
```

Install:

```sh
API_KEY="$(openssl rand -hex 32)" \
devices/rut241/install.sh root@ROUTER_IP
```

Optional overrides:

```sh
SERIAL=RUT241-001 LTE_INTERFACE=qmimux0 \
API_KEY=... devices/rut241/install.sh root@ROUTER_IP
```

The unit's API key has one owner once it is in the fleet: netage-server's
`[[fleet.device_keys]]` entry for its serial (on the relay droplet,
`/etc/netage-server/netage-server.toml`). The probe env files under
`/root/probes/<serial>.env` there are what an onboarding wrote at the time
and go stale when a key is rotated (D4C5's was, found 2026-10-01); do not
read a key from them. Read it inside the shell into a variable and never
print it.

The AT port is auto-detected (USB interface :1.2) — never hardcode `/dev/ttyUSBn`,
it re-enumerates and a stale path takes the modem down. `LTE_INTERFACE=qmimux0` is
the QMI data bearer (holds the IPv4); using `wwan0` is the long-standing NOCONN bug.

The installer refuses to proceed unless the overlay can keep a safety margin after writing the compressed payloads.

## Ramboot layout (from 0.6.7)

The overlay is 4 MB. A 0.6.7 payload set (3.8 MB) does not fit next to the one
the unit runs, so an in-service RUT241 moves to the ramboot layout
(`docs/upgrade.md`): the overlay keeps only `/etc/init.d/sctl` (the shared
`devices/common/sctl-ramboot.init`), `/etc/sctl/ramboot.sh`,
`/etc/sctl/ramboot.conf` (the mirror URLs and SHA-256s of one version) and
`/etc/sctl/install.json` (`{"layout":"ramboot","target":"mipsel_24kc"}`); the
payloads are fetched into `/tmp/sctl/cache/` at boot from the relay's
plain-HTTP mirror and verified by SHA-256. From then on the agent upgrades
itself by rewriting `ramboot.conf` (the relay's `mirror_base`), and a reboot
with the mirror unreachable retries until it answers.

The move, through the relay and with no SSH:

```sh
./rundev.sh relay artifacts root@RELAY mirror 0.6.7.172 http://RELAY:8081/artifacts/0.6.7.172
./rundev.sh device upgrade-remote rut241-relay 0.6.7.172 root@RELAY
```

`rundev.sh` reads the version's manifest and mirror from the relay, checks the
mirror serves the manifest's bytes, writes `ramboot.conf`, stages the files
through the file API and hands the swap to `ramboot-migrate.sh` on the device.
That script fetches both payloads into the cache and verifies them before
anything on flash moves, keeps the current init and payloads in `/tmp`, writes
the ramboot files, removes the gz payloads (that is what frees the overlay),
restarts the agent once and puts the old layout back unless `/api/health`
answers with the version and a connected tunnel twice within 240 s. Progress is
in `/tmp/sctl-rut241-ramboot/state` and `/tmp/sctl-rut241-ramboot.log` on the
device.

`install.sh` above still installs the gz-tmp layout for a unit with room; a
`[tunnel]` block is not written by it and must be re-added.
