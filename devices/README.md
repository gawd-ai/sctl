# Device Targets

Device-specific deployment assets live here. The goal is to keep target quirks close to the device while sharing build mechanics across devices.

```text
devices/
  common/       shared shell helpers for builds/installers
  targets/      reusable toolchain target definitions
  rut241/       Teltonika RUT241 install/config/build assets
  we826-qwd/    ZBTLink WE826-Q-WD RAM boot install/config assets
```

Build a device package:

```sh
devices/build.sh rut241
devices/build.sh we826-qwd
```

Install a device package:

```sh
API_KEY=... devices/rut241/install.sh root@ROUTER_IP
```

Install a RAM-boot device package:

```sh
API_KEY=... SERVER_URL=... SERVER_SHA256=... PLUGIN_URL=... PLUGIN_SHA256=... \
MUSL_LIBC_URL=... MUSL_LIBC_SHA256=... LIBGCC_URL=... LIBGCC_SHA256=... \
devices/we826-qwd/install.sh root@ROUTER_IP
```

Generated binaries, compressed payloads, SDKs, and local secrets do not belong in this tree. They are written under ignored paths:

```text
.artifacts/
.toolchains/
```

When adding a new device, prefer this split:

- Put reusable CPU/SDK/toolchain details in `devices/targets/`.
- Put device-specific config templates, init scripts, install scripts, and README notes under `devices/<device>/`.
- Put shared shell functions in `devices/common/` only when a second device needs the same behavior.
- Keep local IPs, API keys, IMEIs, SIM IDs, and customer identifiers out of tracked files.
