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

Never re-point a payload URL at different bytes without updating the hash
on the device — the boot-time verification would fail and the unit would
retry-loop instead of starting sctl. Publish new payloads under new names
or update URL and hash together.

SSH-deployed devices (RUT241 class) and relays have no publish step:
`rundev.sh device upgrade` / `device upgrade-remote` / `relay upgrade`
stop, swap the binary, and start.

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
4. **Payload soak before relay deploy.** Publish + activate on a
   representative device first and let it soak — tunnel stable through
   heartbeat cycles, exec/files/sessions exercised, watchdog quiet, no
   supervisor restarts. Device payloads talk to the *old* relay during the
   soak, which is exactly the compatibility that matters: devices upgrade
   before relays, so new-device-old-relay must hold.
5. **Deploy the relay** (`rundev.sh relay upgrade`) only after the soak;
   verify `relay status` shows every expected device re-registered.
6. **Tag at deploy.** Tag (`v0.6.0`) the exact commit the deployed
   artifacts were built from, when they are live — not when the branch
   merges. The tag is the statement "this is what production runs".
