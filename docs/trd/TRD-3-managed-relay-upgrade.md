# TRD-3: Managed relay upgrade

Implements [ADR-003](../adr/003-the-relay-upgrades-itself-through-the-managed-path-last.md). Status: Planned.

## 1. Objective

A relay upgrades itself through `POST /api/upgrade` with the same proof, helper, health and rollback as a device, asked by one `rundev.sh` command, after its devices; the scp path is retired.

## 2. Deliverables

- `server/src/upgrade/stage.rs` `manifest_url()`: when the request names no `manifest_url` and there is no `[tunnel] url` but the server runs in relay mode (`tc.relay`), the URL is `http://127.0.0.1:<[server] listen port>/api/tunnel/artifacts/<version>/release.json`; the bearer for the manifest, the signature and the artifact is the relay's own operator key (it reaches its own reader, which accepts it). A unit test covers the derivation and the precedence of an explicit `manifest_url`.
- `server/src/upgrade/routes.rs`: no change (the route exists); `docs/http-api.md` and `docs/upgrade.md` §relay say the default.
- `rundev.sh`: `relay upgrade <user@host> <version>` replaced by the managed request: over SSH, `POST http://127.0.0.1:8443/api/upgrade` with the operator key read where it lives (`$RELAY_REMOTE_CONFIG`), body `{version, request_id: "relay:<version>:<n>"}`; then poll `/api/health` `upgrade` every 5 s up to 5 min, print each phase change, the outcome and the log tail; exit non-zero on `rolled_back` or `needs_hands`. The old `do_relay_upgrade` (scp, stop, start) is removed; `relay deploy` stays for a first install.
- `docs/releasing.md`: the relay step of the release checklist becomes this command, after the fleet's rollout of the version is done on that relay's devices.
- `CHANGELOG.md` `[Unreleased]` (CRLF).

## 3. Verification

- `cargo test`, clippy pedantic, fmt.
- The local bench relay (`scratchpad/upg`, systemd layout under a user `systemd-run` or the bench's equivalent): a good bundle ends `done`, a bad binary ends `rolled_back` with the previous binary back and the relay serving again.
- do-toronto, after LiveBarn's rollout of the current release completes and with the owner's OK: `rundev.sh relay upgrade root@174.138.114.209 <version>`; `/api/health` answers the version with `upgrade.phase = idle`; every device reconnects once (netage-server's listing stays complete); the previous binary stays in `/var/lib/sctl/rollback`.

## 4. Slices and status

1. Relay-mode manifest URL: Planned.
2. `rundev.sh relay upgrade` as the managed request: Planned.
3. Bench, then do-toronto: Planned.
