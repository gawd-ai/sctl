# ADR-007: The agent reports a unit's power only from a source the unit already publishes; the XE300 publishes none

Status: Accepted (2026-10-08), option A of section 7. The live check (section 9) confirmed sections 3 and 4 on a deployed unit, and the owner's rule for this round was option A unless the check showed a read-only source; it showed none. Realized by: none; no agent code is written (TRD-7 only if option B is ever taken). Cites `docs/http-api.md` (`GET /api/info`).

## 1. Trigger

On 2026-10-06 LiveBarn's Mountain Arena unit (XE300-9483C446D52E, a GL.iNet GL-XE300C4 "Puli" with an internal battery) lost its wire at 17:34 UTC and went silent at 20:38. Nobody could tell a power cut that the battery carried for three hours from a line loss followed by an LTE loss, and the fleet's assistant guessed about the battery. The fleet already has a battery field and a battery bar, but no agent fills them, so the guess had nothing to stand on.

## 2. What was studied

No XE300 is on the bench (every known unit is deployed), so the study is offline, from GL.iNet's public firmware.

- `openwrt-xe300-3.215-0921-1663732402.tar`, sha256 `de5b2d39d27b0e8ef8530119cf9a11a88dbcdfec43766c36c26a3ef5d2d6b1f3` as GL's firmware API publishes it. 3.212, which the units run, is no longer offered: the API lists 3.215, 3.216 and 3.217 for the 3.x line. 3.215 is the closest and has the units' base: OpenWrt 19.07.8 r11364, ath79/nand, kernel 4.14.241. <!-- secret-scan: allow -->
- The battery package, `gl-xe300-mcu 3.0.4-1`, was built on 2021-02-15 and is byte-identical in 3.215 and 3.217 (`openwrt-xe300-3.217-0508-1683513909.tar`, sha256 `d5cfa70d2f3cfa4718009fad29188fe04163b3fdc579a803ecc76737ac3b2f8b`). It predates 3.212 and did not change across the later releases, so 3.212 very likely carries the same files; the live check compares their hashes (section 8). <!-- secret-scan: allow -->
- The root squashfs was unpacked with `unsquashfs`. The vendor binaries (MIPS16e, section headers stripped) were read with the OpenWrt 19.07 SDK's objdump and `strings`; the shell scripts, the web UI and the device tree were read directly.

| File (3.215 and 3.217) | sha256 |
|---|---|
| `/usr/bin/xe300-mcu` | `00fc123d5b1689182d34a9ed06b1fbab2e37c025ed3efbd4c80ddbb277c3601d` | <!-- secret-scan: allow -->
| `/usr/bin/mcu_update` | `503f644b5f5ad4a1c051d63fce3e0948a6f6b5aff7537f84fce1a18fadd74116` | <!-- secret-scan: allow -->
| `/usr/lib/gl/libglmcu.so` | `0384c4ed93c38664431f460418b73aa0a3cbaee20a4a06b7ad0fa38ee6e8e9a2` | <!-- secret-scan: allow -->

## 3. What the unit exposes

1. **The battery and charger sit behind a microcontroller on a USB serial bridge:** USB port `1-1.4`, interface `1-1.4:1.0`, bound by the kernel's usb-serial drivers (`ch341` and `cp210x` are shipped) as `/dev/ttyUSB0`. The modem sits on `1-1.2`; with the MCU present its ports move to `ttyUSB1` to `ttyUSB4`, AT on `ttyUSB3`. Evidence: `/etc/init.d/init_gpio` takes that sysfs directory as the mark of a battery unit; `/etc/init.d/gl_init` (XE300 branch) moves smsd from `ttyUSB2` to `ttyUSB3` when it exists; the 2026-09-04 bring-up saw five ttyUSB ports, AT on `ttyUSB3` and PPP on `ttyUSB4`, which is this layout.
2. **A unit without the battery has no MCU:** `init_gpio` then exports GPIO 16 and drives it high to power the USB bus.
3. **The kernel has no supply driver.** The `power_supply` class is built in, but nothing registers a supply: the device tree's only i2c device is an RTC (`rtc-sd2068` at 0x32), and no GPIO reports a charger or an input. `/sys/class/power_supply` is expected empty.
4. **No ubus object, no rpcd plugin, no uci key, no daemon.** `/usr/share/rpcd/acl.d` and `/usr/libexec/rpcd` hold nothing about power. No init script, cron entry or hotplug script runs the MCU reader; `gl_monitor` runs `e750-mcu` only for the `mifi` model.
5. **The vendor's reader, `/usr/bin/xe300-mcu`, is one-shot, and it writes to the MCU.** In order, it:
   - opens `/dev/ttyUSB0` at 9600 baud, raw, and takes it exclusively (`TIOCEXCL`);
   - closes and exits at once if `/tmp/mcu.lock` exists;
   - reads and parses whatever the MCU already sent (`FIONREAD`), then flushes the port;
   - writes the 22-byte query `{ "mcu_status": "1" }` with its trailing NUL;
   - polls up to four times, 300 ms apart, for the reply `{OK},<P>,<T>,<C>,<N>` (at least three fields; `T` in tenths of a degree);
   - truncates and rewrites `/tmp/mcu_data`, then closes the port.
6. **The cache, `/tmp/mcu_data`,** is one JSON object, `{"T":%.1f,"P":%d,"C":%d,"N":%d}`, followed by a NUL byte and no newline. `T` is the temperature in degrees C, `P` the battery percent, `C` is 1 while charging, and `N` is what the vendor calls `Charge_cnt` (meaning unconfirmed). It carries no timestamp; its age is the file's mtime. It lives in tmpfs, so it exists only after a run since boot.
7. **Only the GL web API runs the reader, and only when asked.**
   - `/cgi-bin/api/mcu/get` (`/usr/lib/gl/libglmcu.so`, `mcu_get_data`) runs `xe300-mcu`, reads the cache and answers `Temperature`, `Percent` (capped at 100), `Charging` and `Charge_cnt`. When the cache does not parse, it answers zeros, so a 0 from this API is not a reading.
   - `/cgi-bin/api/client/percent/get` (`/usr/lib/gl/libglsdk.so`, `router_percent_get`) does the same.
   - The admin page polls `mcu/get` every 26 s while a browser has it open (`/www/src/temple/internet/index.js`). Both calls need a GL admin session; lighttpd serves them.
8. **`/usr/bin/mcu_update` flashes the MCU's firmware over the same port:** it stops lighttpd, creates `/tmp/mcu.lock` and streams the image. It is never run.
9. **Mains is not reported as such.** `C = 1` means external power is in and the battery is charging. `C = 0` means either on battery or on mains with a full battery; the vendor's UI paints 100 % green either way. Only a mains-unplug test on a unit separates them.
10. **Onboarding does not touch this path.** `ops/onboard-xe300.sh` (fleet) disables `gl_mqtt`, `gl_bigdata`, `gl_tertf`, `gl_s2s` and `siderouter`. None of them reads the MCU, and there is no MCU daemon to disable. lighttpd stays enabled.
11. **The agent never opens the MCU's port today.** The Quectel plugin finds its AT port by USB vendor `2c7c` and interface `:1.2` (`drivers/sctl-comms-quectel/src/lib.rs`, `detect_quectel_at_port`), never by probing ttyUSB devices.

## 4. What is readable without a write

Nothing current. The only reading on the unit is `/tmp/mcu_data`, and it exists only after something writes the status query to the MCU. A probe's LAN gets no DHCP from the unit and nobody administers it through GL's page, so on a deployed probe the file is absent or as old as the last browser session. Reading the cache alone would report a value whose age depends on whether someone happened to browse the unit. A query to a power-controlling microcontroller is a write, not a read, even when it is the vendor's own query.

## 5. Decision

1. **The agent reports a unit's power only from a source the unit already publishes, without a write by the agent.** The XE300 has none, so sctl reports no `power` key on it this round, and no agent code is written.
2. **Nothing infers it.** A missing `power` key means "the unit reports no power state", never mains and never battery. The fleet reads the key by presence (fleet TRD-10, unreadable branch: the assistant's projection drops the always-null battery field, and the "Built-in battery backup" claim goes to the owner).
3. **The live check runs once, read-only (section 8),** on one XE300 the owner names, in phase S. It confirms section 3 on a deployed unit: the MCU's presence, the hashes, the cache's presence and age, the empty supply class, and which process holds which serial port. If it finds a source the image did not show (a supply driver, a ubus object, a daemon that keeps the cache fresh), this ADR is revised before acceptance and TRD-7 reads that source.
4. **The carrier, if power is ever reported, is `GET /api/info` only:** `power: {source: "mains" | "battery", battery_pct, since}`, the key omitted when unread. There is no pushed message: the relay forwards only `net.state`, `infra.state` and `upgrade.state` and upgrades last, while netage-server already reads `/api/info` on every health poll.

## 6. Consequences

- The 2026-10-06 question stays unanswerable on today's units, and the fleet says so instead of guessing.
- Nothing on the units changes. sctl 0.6.12 is not cut for power unless option B is taken.
- The fleet's power display and notifications (fleet TRD-10, ADR-0022) have no XE300 producer until then; they render nothing when the key is absent.

## 7. For the owner, after the live check

- **A. Keep reporting nothing.** No risk, no answer.
- **B. The agent runs the vendor's reader, never its own serial code.** On a `GET /api/info`, at most once per 30 s (the vendor's admin page asks every 26 s), the agent runs `/usr/bin/xe300-mcu` only when the MCU's USB function exists and `/tmp/mcu.lock` does not, then reads `/tmp/mcu_data` if the run rewrote it. A run that fails (the vendor UI holds the port, no reply) omits the key. `source` is reported only after the unplug test settles what `C` means at 100 %. This writes the vendor's fixed status query to the power MCU about 2,900 times a day: the exchange GL's own UI makes while open, but unproven at that cadence for months. TRD-7 (sctl 0.6.12) is written only if this is taken; it is proven on one unit as a separate tunnel-less daemon, and the mains-unplug test is scheduled with the owner.

Rejected:

- **Reading `/tmp/mcu_data` alone by its mtime:** nothing refreshes it on a probe.
- **Serial code in the agent:** it would own a power MCU's port, duplicate a vendor protocol and race the vendor's exclusive open.
- **Inferring power from the wire and LTE pattern:** from the agent, an ISP outage and a mains loss look alike.

## 8. The live check (read-only)

Rules:

- One owner-approved exec batch on one named XE300, through the agent already on it. Nothing else runs.
- Never open, read, write or `stty` any `/dev/tty*` device. Sysfs entries and `/proc/*/fd` links are read; the devices never are.
- Never run `xe300-mcu`, `mcu_update`, `e750-mcu` or any `gl_*` command, and never call `/cgi-bin/api`: each of them talks to the MCU or the modem. The list below runs none of them.
- Never create, delete or touch `/tmp/mcu.lock` or `/tmp/mcu_data`.
- Never start, stop, enable or disable a service. `/etc/init.d/<name> enabled` only tests the rc.d link.
- `uci` is read by key name only, never by value, so no secret is printed.
- Never replace or restart the flashed agent. A TRD-7 build, if option B is taken, runs beside it as a separate tunnel-less daemon.
- The mains-unplug test is separate and scheduled with the owner.

```sh
# firmware and the MCU package (compare with section 2)
cat /etc/glversion; grep -E 'DISTRIB_(RELEASE|REVISION)' /etc/openwrt_release
opkg list-installed | grep -E '^gl-xe300-mcu '
sha256sum /usr/bin/xe300-mcu /usr/bin/mcu_update /usr/lib/gl/libglmcu.so
cat /proc/uptime; date -u

# the kernel's supply class
ls -la /sys/class/power_supply/ 2>&1
for d in /sys/class/power_supply/*; do [ -r "$d/uevent" ] && { echo "== $d"; cat "$d/uevent"; }; done

# the MCU's USB function (sysfs only)
ls /sys/bus/usb/devices/1-1.4/1-1.4:1.0/ 2>&1
for f in idVendor idProduct manufacturer product; do printf '%s=' "$f"; cat "/sys/bus/usb/devices/1-1.4/$f" 2>&1; done
readlink /sys/bus/usb/devices/1-1.4/1-1.4:1.0/driver
for t in /sys/class/tty/ttyUSB*; do echo "${t##*/} $(readlink -f "$t/device")"; done
ls /sys/class/gpio/

# ubus, uci (key names only), scripts and services
ubus list | grep -iE 'mcu|batt|power|charg'
uci show 2>/dev/null | cut -d= -f1 | grep -iE 'mcu|batt|charg|power'
grep -lE 'mcu|batt|charg' /etc/init.d/* /usr/bin/gl_* /etc/hotplug.d/*/* /etc/crontabs/* 2>/dev/null
for s in gl_mqtt gl_bigdata gl_tertf gl_s2s siderouter gl_monitor gl_init init_gpio lighttpd; do [ -x "/etc/init.d/$s" ] && { "/etc/init.d/$s" enabled && echo "$s enabled" || echo "$s disabled"; }; done

# processes, and which process holds which serial port
ps w | grep -E '[x]e300-mcu|[m]cu_update|[l]ighttpd|[g]l_monitor|[s]msd'
for p in /proc/[0-9]*; do for f in "$p"/fd/*; do l=$(readlink "$f" 2>/dev/null); case "$l" in /dev/ttyS*|/dev/ttyUSB*|/dev/ttyACM*) echo "$(cat "$p/comm" 2>/dev/null) ${p#/proc/} $l";; esac; done; done

# the vendor's cache: read, never refreshed
ls -l /tmp/mcu_data /tmp/mcu.lock 2>&1
date -u -r /tmp/mcu_data 2>&1
cat /tmp/mcu_data 2>&1 | tr '\0' '\n'

# logs
logread | grep -iE 'mcu|batt|charg|power|ttyUSB0|ch341|cp210' | tail -50
dmesg | grep -iE 'ttyUSB|ch341|cp210|usb 1-1\.4|batt|charg' | tail -50
```

Expected, from the image: the three hashes of section 2; an empty `/sys/class/power_supply`; `1-1.4:1.0/ttyUSB0` present on a battery unit; no ubus object or uci key about power; no process holding `ttyUSB0`; `/tmp/mcu_data` absent, or its mtime far older than `date -u`.

## 9. The live check, run (2026-10-08)

One read-only batch, the list of section 8 exactly, on one XE300 of the production fleet (XE300-9483C446D3EA), through its own agent, at 2026-10-08 15:40 UTC, uptime 14.9 days. Then two more reads: `/etc/init.d/init_gpio` and GPIO 16's sysfs entries. No `/dev/tty*` was opened, no vendor tool run, no service touched.

- Firmware 3.212, OpenWrt 19.07.8 r11364; `gl-xe300-mcu 3.0.4-1`. The three hashes equal section 2's table, so 3.212 carries the same battery package as the studied images.
- `/sys/class/power_supply` is empty: no supply driver.
- USB `1-1.4` is the MCU's bridge: `1a86:7523` ("USB Serial"), bound to `ch341`, `ttyUSB0`; the modem's ports are `ttyUSB1` to `ttyUSB4`.
- No ubus object about power, battery or charge; the only matching uci keys are the radio's `txpower` and `txpower_max`.
- The scripts naming the MCU or the battery are `init_gpio` and `gl_monitor` (enabled), as the image showed; `gl_mqtt`, `gl_bigdata`, `gl_tertf`, `gl_s2s`, `siderouter` and `gl_init` are disabled (the onboarding's work).
- No process holds `ttyUSB0`. `smsd` and the agent hold `ttyUSB3` (the modem's AT port), `askfirst` the console `ttyS0`.
- `/tmp/mcu_data` and `/tmp/mcu.lock` do not exist: nothing has read the MCU since boot.
- The logs name no battery or charge event; `dmesg` shows the `ch341` bridge attaching at 33 s.
- **Correction to section 8's expectation:** GPIO 16 IS exported, direction out, value 1, on this battery unit. `init_gpio` runs at `START=01`, before USB enumerates `1-1.4` (14 s in `dmesg`), so its battery-unit test (the `ttyUSB0` directory) is false at that moment and it powers the USB bus on every unit. GPIO 16 is an output the vendor drives, not an input about power, so it is no source either.

**Result:** no source a unit publishes without a write. Option A stands: the agent reports no `power` key on the XE300, and the fleet says the unit reports no power state (fleet TRD-10). The mains-unplug test is not needed for option A and stays with the owner for a later phase, should option B ever be taken.
