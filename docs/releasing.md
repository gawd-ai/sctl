# Building, deploying, and releasing sctl

The tooling map — what `rundev.sh` can do, how versions are derived, how
device payloads are built and rolled out, and the checklist for cutting a
release. Covers sctl 0.6.0.

## Contents

- [Version scheme](#version-scheme)
- [rundev.sh subcommands](#rundevsh-subcommands)
- [Device payload builds](#device-payload-builds)
- [Publish vs. activate](#publish-vs-activate)
- [Release checklist](#release-checklist)

## Version scheme

The full version is `<cargo-version>.<build-number>` — e.g. `0.6.0.113` —
assembled in `server/src/lib.rs`:

```rust
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), ".", env!("SCTL_BUILD_NUMBER"));
```

- **`<cargo-version>`** is `version` in `server/Cargo.toml`. A release bumps
  it in all five version carriers: `server/`, `mcp/`,
  `crates/sctl-comms-abi/`, `drivers/sctl-comms-quectel/` (Cargo.toml each)
  and `web/package.json`.
- **`<build-number>`** is the commit count, `git rev-list --count HEAD`,
  resolved by `server/build.rs` in this order:
  1. the `SCTL_BUILD_NUMBER` env var — **required for cross builds**, where
     the build runs in a Docker container that cannot see the host's
     `.git` (`devices/common/build-lib.sh` exports it automatically);
  2. `git rev-list --count HEAD` from the source tree;
  3. `"0"` as a last resort (tarball builds).

The build number bumps on every commit, so a fresh binary is never mistaken
for an older one — `sctl v0.6.0.113` in the startup log identifies the
exact commit count it was cut from.

A preset `SCTL_BUILD_NUMBER` wins everywhere: `server/build.rs`, `rundev.sh`
and `devices/common/build-lib.sh` all keep one that is already exported.
That is how CI's `Release` workflow gives every job the number its `meta`
job counted on a full checkout, and how a hand build is made to match a
release: `SCTL_BUILD_NUMBER=179 ./rundev.sh device upgrade-remote <name>`
produces the same `0.6.8.179` the bundle carries. Before 2026-10-01 the
device build counted its own checkout over the preset, so CI's shallow
OpenWrt jobs stamped 0.6.7.172's `mips_24kc` and `mipsel_24kc` servers as
`0.6.7.1` and a managed upgrade from that bundle failed its version proof on
every XE300 and RUT241 (ADR-001). The `Release` workflow now refuses such a
bundle (see "Managed upgrades").

## rundev.sh subcommands

`./rundev.sh <command>` is the whole dev/deploy toolbox. From the dispatch
at the bottom of the script:

**Dev stack**

| Command | Does |
|---------|------|
| `setup` | Build everything (server, mcp, web) + start all services + register MCP clients (the default). |
| `build` | Build only — no start/stop. |
| `start` | Restart all services without rebuilding. |
| `stop` | Stop all services + deregister MCP clients when safe. |
| `status` | Show what's running. |
| `tunnel` | Build + start the tunnel dev environment — a local relay with clients connecting through it. `--cloudflared` rides a Cloudflare Quick Tunnel (double-CGNAT rehearsal); `--relay-url <url>` targets an external relay. |

**MCP registration** (no build/start; each writes the sctl MCP server into
one agent client's config)

| Command | Does |
|---------|------|
| `agents` | Register MCP in all supported agent clients. |
| `claude` | Register in Claude Code. |
| `codex` | Register in Codex CLI. |
| `hermes` | Register in Hermes. |
| `opencode` | Register in OpenCode. |
| `openclaw` | Register in OpenClaw. |
| `grok` | Print Grok Build / generic MCP config. |
| `nanoclaw` | Print NanoClaw / generic MCP config. |

**Environment profiles** (named copies of `devices.dev.json`)

| Command | Does |
|---------|------|
| `env show` | Show the active profile and its devices (default). |
| `env ls` | List saved profiles. |
| `env use <name>` | Switch the active device config to a named profile. |
| `env save [name]` | Save the current config as a named profile. |
| `env edit [name]` | Open a profile in `$EDITOR` (syncs if it is the active one). |

**Device management**

| Command | Does |
|---------|------|
| `device ls` | List registered devices with live health checks (default). |
| `device add <name> <host>` | Discover + register a device — probes arch, serial, api_key via SSH. |
| `device rm <name>` | Remove a device from the config. |
| `device deploy <name>` | Full deploy: cross-compile + upload binary + config + init script over SSH. |
| `device upgrade <name>` | Binary-only upgrade via SSH (stop → upload → start). |
| `device deploy-watchdog <name>` | Deploy the watchdog script + cron entry (SSH or API). |
| `device upgrade-remote <name>` | Binary upgrade **via the relay** (STP upload + swap) — no SSH path needed. |
| `device upgrade-remote <name> <version> <user@relay>` | RUT241: the move to the ramboot layout, fed by the relay's mirror (`devices/rut241/ramboot-migrate.sh`). |
| `device upgrade-remote <name> <version> <user@relay \| mirror URL>` | WE826 installed before 0.6.9: one guarded move to the current ramboot layout, rolled back by itself unless healthy (`devices/we826-qwd/ramboot-refresh.sh`, TRD-8). The mirror by IP for a unit that fetches by IP. |

**Relay VPS deployment**

| Command | Does |
|---------|------|
| `relay setup <user@host>` | Full VPS provisioning: Caddy + sctl + firewall. |
| `relay deploy [user@host]` | Deploy binary + service, preserving config. |
| `relay upgrade [user@host]` | Binary-only upgrade (stop → upload → start). |
| `relay status [user@host]` | Health check + connected devices (default). |
| `relay sctlin [user@host]` | Deploy the sctlin web UI to the relay. |

**Playbook library**

| Command | Does |
|---------|------|
| `playbook ls` | List playbooks in the local library (default). |
| `playbook deploy <device\|all> [category]` | Push playbooks to device(s) via the API. |

## Device payload builds

Embedded targets are built by device, not by triple:

```sh
devices/build.sh we826-qwd     # dispatches to devices/we826-qwd/build.sh
devices/build.sh rut241
devices/build.sh xe300
```

Each device's `build.sh` picks its toolchain from `devices/targets/`
(OpenWrt SDK definitions), exports `SCTL_BUILD_NUMBER`, builds with
`-Z build-std` where the target needs it, and drops gzipped artifacts under
the ignored `.artifacts/<device>/` tree — server binary, comms plugin, and
(for the RAM-boot target) the musl loader libraries. See
`devices/README.md` and the per-device READMEs for install flows.

## Publish vs. activate

RAM-boot devices (the WE826 class — ~832 KB writable overlay) never store
the multi-megabyte payload in flash. The persistent part is only a
bootstrap: `ramboot.sh` plus `ramboot.conf` carrying **payload URLs and
SHA-256 hashes**. At every boot the device downloads the payloads into
tmpfs, verifies the hashes, and executes from RAM.

That splits a rollout into two distinct steps:

1. **Publish** — build the payload, host it at a URL, and write the new
   URL + SHA-256 into the device's `/etc/sctl/ramboot.conf` (a small
   overlay write, doable remotely through the file API). Nothing changes
   for the running instance; the in-RAM binary keeps running and the tmpfs
   payload cache keeps service restarts cheap.
2. **Activate** — the device picks up the published payload on its next
   fetch: a service restart, or naturally on the next **power cycle**
   (tmpfs is cleared, so a reboot always re-fetches and re-verifies).

On a fielded unit still running the pre-supervisor init, **prefer natural
power-cycle activation over a remote service restart**: the old init
backgrounds the wrapper exactly once with no respawn, and after a conf
publish the cache is invalid, so a restart forces a fresh fetch over
whatever link the vehicle has right now — if that fetch exhausts its
attempts, sctl stays down for the rest of the power-on and the tunnel was
the only way in. A power cycle risks the same fetch but happens when the
vehicle would re-fetch anyway. Units running the supervised init retry
every 10 s and may activate either way.

Never re-point a payload URL at different bytes without updating the hash
on the device — the boot-time verification would fail and the unit would
retry-loop instead of starting sctl. Publish new payloads under new names
or update URL and hash together. **Keep the previous payload hosted at its
exact URL until every device's `ramboot.conf` is confirmed repointed**: a
not-yet-repointed device that power-cycles must still be able to fetch the
bytes its flash pins, or it goes dark until the next cycle.

The point of no return for a device is the `ramboot.conf` write, not the
publish. Before pinning a new SHA on a remote CGNAT unit: (a) boot the
exact artifact on a bench unit of the same class first — a payload that
cannot reach registration leaves no tunnel to write a corrected conf
through; (b) read and record the device's current `ramboot.conf` URL+SHA
as the rollback reference; (c) verify the out-of-band path (WireGuard)
actually works on that specific unit if one is claimed.

SSH-deployed devices (RUT241 class) and relays have no publish step:
`rundev.sh device upgrade` / `device upgrade-remote` / `relay upgrade`
stop, swap the binary, and start.

`rundev.sh device upgrade*` is not universal, though. It resolves a build
from `uname -m` through `ARCH_TARGET`, so a device reporting an arch that
is absent from that map (the GL-XE300 reports big-endian `mips`) fails at
`arch_to_bin`. Adding a map entry would be worse than the failure: those
paths build a generic `cross` binary rather than the OpenWrt-SDK
`-Z build-std` one the device was qualified on, and install to
`/usr/bin/sctl` regardless of where that device keeps its payloads. Use
the device's own `install.sh` over SSH instead, or push the `.gz` payloads
via the file API / STP and restart its init script.

## Release checklist

For a 0.6.0-style release (a version bump with device payloads and a relay
in production):

1. **Gates green.** CI on the release commit: fmt, clippy `--workspace`,
   `cargo test --workspace`, the http-api docs gate, the config docs gate,
   the ts-rs bindings diff, web check/test/package, and both build jobs.
   Locally: `./scripts/check-http-api-docs.py` and
   `./scripts/check-config-docs.py` run in seconds.
2. **Bump versions.** All five carriers (see [Version
   scheme](#version-scheme)); update the root `CHANGELOG.md`.
3. **Build payloads** with `devices/build.sh <target>` from the tagged-to-be
   commit; record artifact SHA-256s.
4. **Payload before relay, activation confirmed.** Publish + activate on a
   bench device of the same class first (a payload that cannot reach
   registration leaves a CGNAT unit with no remote recovery), then the
   fleet. Device payloads talk to the *old* relay in this window, which is
   exactly the compatibility that matters: devices upgrade before relays,
   so new-device-old-relay must hold. How long to soak is a judgment call,
   but the relay gate is not: the relay deploys only once every fleet
   device is **confirmed activated** (check versions on `/api/health`), not
   merely published-to — a device still on the old payload answers no
   HTTP proxying at all under the new relay (`DEVICE_PAYLOAD_OUTDATED`)
   and its health polling reads as down.
5. **Deploy the relay** (`rundev.sh relay upgrade`) after that gate;
   verify `relay status` shows every expected device re-registered. Rotate
   relay keys only after the whole fleet runs 0.6.0+ (the new client
   slow-retries a rotated key forever; the old client exits).
6. **Tag at deploy.** Tag (`v0.6.0`) the exact commit the deployed
   artifacts were built from, when they are live — not when the branch
   merges. The tag is the statement "this is what production runs".

## Managed upgrades (0.6.7+)

From 0.6.7 a release is a **signed bundle** built by CI on the tag
(`.github/workflows/release.yml`): one artifact set per target, one
`release.json` naming every file's SHA-256, signed with the key in the
`SCTL_RELEASE_SIGNING_KEY` secret (`scripts/release-sign.sh`). The bundle is
uploaded to each relay (`./rundev.sh relay artifacts <user@host> <version>
<dir>`) and devices upgrade themselves when the fleet asks
(`POST /api/upgrade`), rolling back on their own when the new agent does not
answer health. `docs/upgrade.md` is the contract.

The paths above (`device upgrade`, `device upgrade-remote`) remain for a
unit that runs an agent older than 0.6.7 or has no `/etc/sctl/install.json`
(and a WE826 whose `install.json` lacks `helper_prefix`, installed by 0.6.7
or 0.6.8); each of them now writes that file, so the last hand upgrade leaves a unit
that upgrades itself from then on. `relay deploy` remains for a first
install; `relay upgrade` is the managed request above, no longer an scp. `sctl upgrade
<version>` from a shell on the box does what the fleet does, for a bench.

Release checklist additions:
1. The tag must equal `server/Cargo.toml`'s version (`v0.6.7` for `0.6.7`);
   the workflow refuses otherwise. Fetch before tagging.
2. Wait for the `Release` workflow; download the bundle from the GitHub
   release, or `scripts/release-bundle.sh` a bench bundle from local
   artifacts (unsigned unless `SIGNING_KEY_FILE` is set; a bench signs with
   its own key and lists it in `[upgrade] trust_keys`).
3. `./rundev.sh relay artifacts <user@host> <version> <dir>` on every relay,
   then `./rundev.sh relay artifacts <user@host> mirror <version> <url>` for
   the plain-HTTP copy a `ramboot` unit boots from, then the fleet's rollout.
4. The relay itself, last: `./rundev.sh relay upgrade <user@host> <version>`
   once every device ring of that version is done on it. The relay fetches
   the bundle from its own store, swaps under a transient `systemd-run`
   unit, checks its own health and rolls back by itself; its devices
   reconnect once (ADR-003). The previous binary stays in
   `/var/lib/sctl/rollback/`.

Before the bundle is signed, the `Assemble` step proves every `server`
artifact (`sctl-<target>`, the gunzipped `sctl-server-<target>.gz`) embeds
the manifest's four-part version (`strings -n 5`, the version bounded by
non-version characters: it sits inside a longer merged string in the
binary). The plugins, `libc` and `libgcc` embed no version and are not
checked. One failure refuses the release, so a bundle on a relay is trusted
to upgrade every target it names (ADR-001). The step's log reads
`proof ok <name>` per artifact.
