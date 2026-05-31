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
SERIAL=RUT241-001 AT_DEVICE=/dev/ttyUSB2 LTE_INTERFACE=wwan0 \
API_KEY=... devices/rut241/install.sh root@ROUTER_IP
```

The installer refuses to proceed unless the overlay can keep a safety margin after writing the compressed payloads.
