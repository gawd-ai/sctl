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

### `rundev.sh device upgrade` does NOT work for this device

`uname -m` returns `mips` (big-endian), which has **no entry** in `rundev.sh`'s
`ARCH_TARGET` map, so `device upgrade` and `device upgrade-remote` both fail at
`arch_to_bin`. Do not "fix" that by adding a map entry: those paths build a generic
`cross` binary rather than the proven OpenWrt-SDK `-Z build-std` one, and install to
`/usr/bin/sctl`, which is not where this device keeps it. Use `install.sh` over SSH,
or push the `.gz` files via the file API / STP and restart the init script.

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
