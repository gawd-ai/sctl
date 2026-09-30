# Managed upgrades: the agent upgrades itself, told by the fleet

sctl 0.6.7 introduces the mechanism by which a device replaces its own agent
with a signed release its relay serves, restarts, proves the new agent healthy
and rolls back on its own when it is not. Nothing here needs a shell on the
device or a person watching. This document is the contract every side codes
against: the agent, the relay, the fleet backend (netage-server) and the fleet
application.

## Contents

- [Roles](#roles)
- [Targets and layouts](#targets-and-layouts)
- [The release bundle](#the-release-bundle)
- [Signing](#signing)
- [Artifacts on the relay](#artifacts-on-the-relay)
- [What the agent reports](#what-the-agent-reports)
- [Asking a device to upgrade](#asking-a-device-to-upgrade)
- [What the agent does](#what-the-agent-does)
- [The `upgrade.state` push](#the-upgradestate-push)
- [Holds](#holds)
- [The relay upgrades itself](#the-relay-upgrades-itself)
- [By hand](#by-hand)

## Roles

- **CI** builds one artifact per target on a tag, writes `release.json`, signs
  it, and publishes the bundle as a GitHub release.
- **The relay** holds a copy of the bundle under `<data_dir>/artifacts/<version>/`
  and serves it to its own devices. One copy per relay, however many devices.
- **The agent** reports what it is (version, target, layout), fetches its own
  artifact from its relay when asked, verifies the manifest's signature and the
  artifact's hash, stages the new files, hands off to a detached helper that
  swaps, restarts and watches, and reports the outcome on the tunnel.
- **netage-server** learns each device's version, target and layout from the
  relay, forwards "upgrade to X" to a device through the relay's REST proxy,
  and turns `upgrade.state` frames into fleet events.
- **The fleet application** owns releases, rollouts, rings, tenant windows and
  holds. It never touches a device directly.

## Targets and layouts

A **target** is the compile target, named after the OpenWrt convention because
that is what every install script already uses. It is a compile-time constant
in the agent (`sctl::upgrade::TARGET`):

| Target | Rust triple | Hardware |
|---|---|---|
| `x86_64` | `x86_64-unknown-linux-musl` | the relay droplet, dev boxes |
| `aarch64` | `aarch64-unknown-linux-musl` | |
| `armv7` | `armv7-unknown-linux-musleabihf` | BPI-R2 |
| `riscv64` | `riscv64gc-unknown-linux-musl` | BPI-RV2 |
| `mips_24kc` | `mips-unknown-linux-musl` (big-endian, dynamic musl) | GL-XE300, ZBT WE826 |
| `mipsel_24kc` | `mipsel-unknown-linux-musl` (dynamic musl) | RUT241 |

A **layout** is how the agent is installed on the box: where the files live,
how the service restarts, where a rollback set is kept. The install script
writes it to `/etc/sctl/install.json` and the agent reads it at start:

```json
{"v": 1, "layout": "gz-tmp", "target": "mips_24kc",
 "files": {"server": "/usr/local/lib/sctl/sctl-server-mips_24kc.gz",
           "plugin": "/usr/local/lib/sctl/sctl-comms-quectel-mips_24kc.so.gz"},
 "restart": "/etc/init.d/sctl restart",
 "rollback_dir": "/usr/local/lib/sctl/rollback",
 "health_url": "http://127.0.0.1:1337/api/health",
 "min_free_kb": 4096}
```

| Layout | Files | Restart | Rollback set | Used by |
|---|---|---|---|---|
| `usr-bin` | the raw binary at `/usr/bin/sctl`, the plugin `.so` beside procd | `/etc/init.d/sctl restart` | `/usr/lib/sctl/rollback/` | generic OpenWrt, the BPIs |
| `gz-tmp` | gzipped payloads under `/usr/local/lib/sctl/`; the init expands them to `/tmp/sctl/` at start | `/etc/init.d/sctl restart` | `/usr/local/lib/sctl/rollback/` | XE300, RUT241 |
| `ramboot` | nothing but `/etc/sctl/ramboot.conf` (URLs and SHA-256s); `ramboot.sh` fetches into `/tmp/sctl/cache/` at boot | `/etc/init.d/sctl restart` | the previous `ramboot.conf` | WE826 (832 KB overlay) |
| `systemd` | the raw binary at `/usr/local/bin/sctl` | `systemctl restart <unit>` | `/var/lib/sctl/rollback/` | the relay |

`files` names the persistent locations by **role**: `server`, `plugin`, and for
`ramboot` also `libc` and `libgcc`. A release's artifact list uses the same
roles, so the agent knows which artifact replaces which file without a
per-device table. Every key in `install.json` except `layout` and `target` has
a default per layout; a script writes only what differs.

An agent with no `install.json` reports `layout: null` and refuses to upgrade
("no install.json: this device was installed before 0.6.7"). The last hand
upgrade to 0.6.7 writes the file.

## The release bundle

CI builds it on a `v*` tag (`.github/workflows/release.yml`), and
`scripts/release-bundle.sh` assembles it from built artifacts, locally or in CI:

```
release/
  release.json
  release.json.sig
  sctl-x86_64                                 (role server, target x86_64)
  sctl-aarch64
  sctl-armv7
  sctl-riscv64
  libsctl_comms_quectel-armv7.so              (role plugin)
  libsctl_comms_quectel-riscv64.so
  sctl-server-mips_24kc.gz                    (role server, gzip)
  sctl-comms-quectel-mips_24kc.so.gz          (role plugin, gzip)
  libc-mips_24kc.so.gz                        (role libc, ramboot only)
  libgcc_s-mips_24kc.so.1.gz                  (role libgcc)
  sctl-server-mipsel_24kc.gz
  sctl-comms-quectel-mipsel_24kc.so.gz
```

`release.json`:

```json
{"v": 1,
 "version": "0.6.7.431",
 "channel": "stable",
 "min_from_version": "0.6.7.0",
 "published_at": "2026-10-01T02:14:00Z",
 "notes": "one paragraph for the release page",
 "targets": {
   "mips_24kc": {"files": [
      {"role": "server", "name": "sctl-server-mips_24kc.gz", "size": 3481233,
       "sha256": "…", "gzip": true},
      {"role": "plugin", "name": "sctl-comms-quectel-mips_24kc.so.gz", "size": 234511,
       "sha256": "…", "gzip": true},
      {"role": "libc", "name": "libc-mips_24kc.so.gz", "size": 1104233, "sha256": "…", "gzip": true},
      {"role": "libgcc", "name": "libgcc_s-mips_24kc.so.1.gz", "size": 41233, "sha256": "…", "gzip": true}]},
   "x86_64": {"files": [{"role": "server", "name": "sctl-x86_64", "size": 9123001, "sha256": "…", "gzip": false}]}
 }}
```

- `version` is the full four-part version the new binary prints; the build
  number is fixed by CI (`SCTL_BUILD_NUMBER` = the commit count of the tag,
  from a full checkout), so every artifact of a bundle prints the same string.
- `sha256` is the hash of the file as it sits in the bundle (the gzip for gz
  files). The agent checks it after the download and the helper checks it
  again on the staged file just before the swap.
- `min_from_version`: an agent below it refuses ("upgrade path not supported")
  and reports so. A request whose version is lower than the running one is a
  downgrade and is refused unless the request says `allow_downgrade`.
- A target absent from `targets` means "no artifact for this device": the
  agent reports `not_applied` with that reason; the fleet lists such devices
  and moves on.
- `channel` is `stable` or `candidate`. The fleet decides what to do with it;
  the agent does not care.

## Signing

`release.json.sig` is one line:

```
ed25519:<key id>:<base64 signature>
```

The signature is ed25519 over the exact bytes of `release.json` (no
canonicalization: the bytes CI wrote are the bytes that are signed and the
bytes that are served). The key id is the first 8 hex characters of the
SHA-256 of the raw 32-byte public key.

The private key is a GitHub Actions secret (`SCTL_RELEASE_SIGNING_KEY`, a PEM
ed25519 key); `scripts/release-sign.sh` signs with `openssl pkeyutl`. The agent
embeds the raw public keys in `server/src/upgrade/keys.rs`: the current key
and, when one is being rotated in, the next. A key rotates over two releases:
release N ships both keys, signed by the old; release N+1 is signed by the new.

`[upgrade] trust_keys = ["<hex public key>"]` in `sctl.toml` adds keys for a
bench or a lab (a release built and signed on a laptop). The fleet never sets
it. The relay applies the same check when a bundle is uploaded to it, so a
bundle that would be refused by every device is refused at the door.

## Artifacts on the relay

The relay keeps bundles under `<data_dir>/artifacts/<version>/` and serves them
on three routes:

| Route | Auth | Does |
|---|---|---|
| `PUT /api/tunnel/artifacts/{version}/{file}` | operator `api_key` | Stores one file of a bundle. `release.json` is verified against the embedded keys (and `trust_keys`) and its `version` must equal `{version}`; a bundle's files are checked against the manifest when it arrives, or when they arrive after it, and a mismatch removes the file and answers 422. |
| `GET /api/tunnel/artifacts` | operator `api_key` or `tunnel_key` | Lists versions: each with its manifest (parsed), which files are present and which the manifest still expects, and the ramboot mirror if one is set. |
| `GET /api/tunnel/artifacts/{version}/{file}` | `tunnel_key` (every device holds it) or operator `api_key` | Serves the file with `Content-Length`, `ETag` (the sha256), `Accept-Ranges: bytes` and `Range` support, so a download dropped on LTE resumes. |
| `DELETE /api/tunnel/artifacts/{version}` | operator `api_key` | Removes a bundle. |
| `PUT /api/tunnel/artifacts/{version}/mirror` (body `{"url": "http://host/path"}`) | operator `api_key` | Sets where a ramboot device's boot fetcher (busybox curl, no modern TLS) can fetch the same files over plain HTTP. Optional; without it a ramboot device reports `not_applied`. |

Every change publishes `artifacts.changed` on `/api/tunnel/events`, and the
replay carries one `artifacts` frame listing the versions after
`replay.done`'s predecessors (before the devices, so a collector knows what
exists before it hears who runs what):

```json
{"type": "artifacts", "versions": [{"version": "0.6.7.431", "channel": "stable",
  "complete": true, "targets": ["x86_64", "mips_24kc"], "published_at": "…",
  "min_from_version": "0.6.7.0", "notes": "…", "mirror": null}], "replay": true}
```

`rundev.sh relay artifacts <version> [bundle dir]` uploads a bundle (the
directory `release/` from `scripts/release-bundle.sh`, or a downloaded GitHub
release) to the relay with the routes above.

## What the agent reports

`tunnel.register` gains three fields, and the relay carries them on
`device.connected` frames and on `GET /api/tunnel/devices`:

```json
{"type": "tunnel.register", "serial": "…", "api_key": "…",
 "features": ["net.state", "infra.state", "upgrade.state"],
 "version": "0.6.7.431", "target": "mips_24kc", "layout": "gz-tmp"}
```

`GET /api/health` on the device gains `"target"` and `"layout"` beside
`"version"`, and `"upgrade"` with the current [`upgrade.state`](#the-upgradestate-push)
body.

## Asking a device to upgrade

`POST /api/upgrade` on the device (through the relay: `POST /d/{serial}/api/upgrade`
with the device's key, as any device route):

```json
{"version": "0.6.7.431",
 "request_id": "rollout-7f3a/ring-1",
 "manifest_url": "https://relay.example/api/tunnel/artifacts/0.6.7.431/release.json",
 "not_before": "2026-10-02T06:00:00Z",
 "not_after": "2026-10-02T09:00:00Z",
 "allow_downgrade": false,
 "mirror_base": "http://host/artifacts/0.6.7.431"}
```

- `version` is required. `manifest_url` defaults to the device's own relay:
  `https://<host of tunnel.url>/api/tunnel/artifacts/<version>/release.json`.
  Artifacts are fetched from the manifest's directory with the tunnel key.
- `not_before` / `not_after` are the window the requester means. An agent
  that reads the request outside it answers 422 with `window_passed` and does
  nothing: a device that was offline when the window was open must be asked
  again, never upgraded at a surprising hour. The check uses the device's
  clock; a device with no clock (before NTP) refuses with `no_clock`.
- `mirror_base` is for the ramboot layout only.

Answers:

| Status | Body | Meaning |
|---|---|---|
| 202 | `{"accepted": true, "state": {…}}` | Staging started; the outcome will arrive as `upgrade.state`. |
| 200 | `{"accepted": false, "state": {…}}` | Already at that version, nothing to do. |
| 409 | `{"error": "…", "code": "UPGRADE_IN_FLIGHT"}` | One is in progress; the state says which. |
| 423 | `{"error": "…", "code": "UPGRADE_HELD"}` | The device holds upgrades (config or file). |
| 422 | `{"error": "…", "code": "UPGRADE_REFUSED", "reason": "window_passed" \| "no_clock" \| "downgrade" \| "no_install_json" \| "no_artifact_for_target" \| "below_min_from_version"}` | Refused before anything was fetched. |

`GET /api/upgrade` returns the current state; `DELETE /api/upgrade` clears a
finished state (the fleet does not need it; it is for a person at the box).

## What the agent does

1. **Refuse early** (the 4xx above), then answer 202 and continue in a task.
2. **Fetch and verify the manifest**: signature against the embedded keys and
   `trust_keys`; `version` equals the requested one; `min_from_version`;
   the target's file list exists.
3. **Fetch the artifacts** into the stage directory (`<state_dir>/upgrade/stage/`,
   persistent storage, never `/tmp`, except on `ramboot` where the stage is the
   boot cache in `/tmp/sctl/cache/`), one at a time, resumable with `Range`,
   with a free-space check before each (the artifact's size plus
   `min_free_kb`). SHA-256 checked on every file. A failure here ends as
   `not_applied` with the reason; nothing on the device changed.
4. **Prove the new binary runs** where the layout allows it: the raw binary (or
   the gunzipped payload in `/tmp`) is executed with `--version` and must print
   the manifest's version. A binary that cannot start on this box is caught
   here, before anything moves.
5. **Write the rollback set**: the current files named by `install.json`, copied
   into `rollback_dir` (replacing an older set), plus `sctl.toml`. On
   `ramboot`, the current `ramboot.conf`.
6. **Hand off to the helper.** The running binary copies itself to the stage
   (`<stage>/helper`) and starts `helper upgrade-apply --stage <stage>` detached:
   its own session (`setsid`), stdio to the stage's `log`, and on the `systemd`
   layout through `systemd-run --collect --unit sctl-upgrade-<version>` so it
   lives outside the service's cgroup. The agent writes `phase: applying` to
   the state file, pushes it, and does nothing more: the helper owns the device
   from here.
7. **The helper** (state machine in `server/src/upgrade/apply.rs`, log in
   `<stage>/log`, state in `<state_dir>/upgrade/state.json`):
   1. waits 2 s so the 202 and the push leave;
   2. verifies every staged file's SHA-256 again;
   3. moves the staged files into place (`rename` beside the target, then
      `rename` over it, so a full filesystem fails before the swap), `sync`;
   4. runs the layout's restart command;
   5. waits for **health**: `GET health_url` answers with `version` equal to
      the target **and**, when the device has a `[tunnel]` client section,
      `tunnel.connected: true`, **twice, 5 s apart, within 180 s**;
   6. on success writes `phase: done, outcome: ok` and exits;
   7. otherwise restores the rollback set, restarts, waits for health once
      more (any version) and writes `rolled_back` (with the log tail), or
      `needs_hands` when even the restore did not come back.
8. **The new agent** reads `state.json` at start. A state of `applying` with
   its own version means the helper's health check is under way: it reports
   `applying` and, when the helper writes `done`, reports that (the state file
   is watched, not polled: the helper touches it, the agent's watcher wakes).
   An old agent that starts after a rollback finds `rolled_back` and reports
   it. Whatever it finds, it reports on every connection until `DELETE
   /api/upgrade` or the next request.

One upgrade at a time: the stage directory holds a `lock` (flock) taken by
both the agent and the helper. A crashed helper leaves the lock free and
`state.json` at `applying`; the next request clears it as `not_applied:
helper_lost` before it starts.

## The `upgrade.state` push

Feature `upgrade.state`, offered in `tunnel.register` and advertised by a relay
that keeps it. Sent on every connection right after the ack, and whenever the
state changes, at most one per second. Kept by the relay per connection,
published on `/api/tunnel/events` as `{"type": "upgrade.state", "serial", "connection_id", "received_at", "state", "replay"}`
and replayed after `net.state` and `infra.state`.

```json
{"type": "upgrade.state", "v": 1, "ts": "2026-10-02T06:12:00Z",
 "request_id": "rollout-7f3a/ring-1",
 "phase": "done",
 "outcome": "ok",
 "from_version": "0.6.7.431", "to_version": "0.6.8.440",
 "running_version": "0.6.8.440",
 "reason": null,
 "started_at": "2026-10-02T06:10:41Z", "ended_at": "2026-10-02T06:11:58Z",
 "log_tail": "…last 1 KiB of the helper's log…"}
```

| `phase` | `outcome` | Meaning |
|---|---|---|
| `idle` | `null` | Nothing in flight, nothing to report (cleared, or never asked). |
| `staging` | `null` | Fetching and verifying. |
| `applying` | `null` | The helper has the device. |
| `done` | `ok` | The new version answered health twice with the tunnel up. |
| `done` | `not_applied` | Refused or failed before anything moved; `reason` says why (`window_passed`, `no_clock`, `downgrade`, `no_install_json`, `no_artifact_for_target`, `below_min_from_version`, `manifest_unsigned`, `manifest_bad_signature`, `download_failed`, `sha256_mismatch`, `no_space`, `binary_does_not_run`, `helper_lost`, `held`). |
| `done` | `rolled_back` | The new version did not pass health; the previous files are back and healthy. |
| `done` | `needs_hands` | The restore did not come back healthy either; a person is needed. |

`running_version` is always the version of the agent that sent the message,
which is how a collector tells "rolled back and the old one is talking" from
"upgraded and the new one is talking" without trusting `outcome` alone.

## Holds

`[upgrade] hold = true` in `sctl.toml`, or the file `/etc/sctl/upgrade-hold`
(its first line is the reason), makes the agent answer 423 to every request
and report `outcome: not_applied, reason: held`. The hold is the box's own last
word; the fleet keeps its own holds (per device, location, vehicle, tenant)
and never sends a request to a device it holds.

## The relay upgrades itself

The relay is a `systemd` layout: `install.json` names the unit
(`sctl-relay`) and `/usr/local/bin/sctl`. Its service unit is hardened
(`ProtectSystem=strict`, `ReadWritePaths=/var/lib/sctl`), so the agent cannot
write its own binary from inside the service and any child dies with the
unit on restart. The helper therefore runs as a transient unit through
`systemd-run`, outside the hardened service, with the stage under
`/var/lib/sctl/upgrade/`. A relay is upgraded last, after its devices, by
the same `POST /api/upgrade` on its own operator API; its devices reconnect
once.

## By hand

- `sctl upgrade <version> [--manifest <url>] [--allow-downgrade]` does what the
  route does, from a shell on the box, reading `sctl.toml` for the relay and
  the keys. It is the bring-up path for a bench unit and the way a person
  finishes a `needs_hands` device.
- `sctl upgrade-apply --stage <dir>` is the helper. Never run by hand.
- `sctl target` prints the compile target, and `sctl install-info` prints what
  `install.json` resolves to with defaults applied.
- `rundev.sh device upgrade-remote` remains for a unit that runs an agent
  older than 0.6.7 or has no `install.json`. After the last hand upgrade every
  release is a rollout.
