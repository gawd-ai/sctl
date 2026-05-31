# ZBTLink WE826-Q-WD

Deployment assets for running `sctl` on the ZBTLink WE826-Q-WD / QCA9531 4G router without flashing a custom firmware image.

This device has only about 832 KB of writable overlay, so the install writes only a small persistent bootstrap:

```text
/etc/init.d/sctl              procd service
/etc/sctl/ramboot.sh          fetch/verify/expand wrapper
/etc/sctl/ramboot.conf        payload URLs and hashes
/etc/sctl/sctl.toml           runtime server config
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

Provide URLs for the compressed server and plugin payloads. Hashes are for the downloaded payload files, before gzip expansion.

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

## Safety

- Keep `ALLOW_UNSIGNED=0` for real deployments.
- Keep payload URLs and hashes out of tracked source files.
- Use `/etc/sctl/disabled` as the fast local kill switch.
- Use `sysupgrade --test` only for firmware research; this deployment does not require firmware flashing.
