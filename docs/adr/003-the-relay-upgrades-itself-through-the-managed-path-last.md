# ADR-003: The relay upgrades itself through the managed path, last

Status: Accepted (2026-10-01). Realized by [TRD-3](../trd/TRD-3-managed-relay-upgrade.md). Cites `docs/upgrade.md` (the `systemd` layout and the relay section) and `docs/releasing.md` (`rundev.sh relay upgrade`).

## 1. Trigger

The relay on do-toronto runs 0.6.7.172 under the `systemd` layout with its `install.json` written, and serves `POST /api/upgrade` for itself (it is an authenticated route of every sctl). But a relay has no `[tunnel] url` (client mode only), so `manifest_url()` in `stage.rs` has nothing to derive the manifest's URL from and refuses the request with `internal` unless the caller passes `manifest_url`. The hand path, `rundev.sh relay upgrade`, is still a binary scp, a stop and a start with a 60 s health poll and no rollback.

## 2. What existed

- `stage.rs` derives the manifest URL from `[tunnel] url` (`ws(s)://<authority>` → `http(s)://<authority>/api/tunnel/artifacts/<v>/release.json`), fetches the manifest, its signature and the artifact with the tunnel key as bearer, and on `systemd` runs the helper through `systemd-run --collect`; health is version-only for a relay (`need_tunnel` false); the rollback set lives in `/var/lib/sctl/rollback`.
- The relay's artifact reader accepts the operator key and the tunnel key.
- `rundev.sh relay deploy` and `relay upgrade`: scp, restart, no manifest, no rollback.

## 3. Decision

1. **In relay mode the manifest URL defaults to the relay's own artifact route** (`http://127.0.0.1:<listen port>/api/tunnel/artifacts/<version>/release.json`), with the key the relay already holds as bearer. A request that names `manifest_url` still wins.
2. **The relay is upgraded by the managed request and nothing else:** `rundev.sh relay upgrade <user@host> <version>` becomes a `POST /api/upgrade` with the operator key, a poll of `/api/health`'s `upgrade` until `done`, `rolled_back` or `needs_hands`, and the log tail. The scp path is retired.
3. **The relay goes last.** A release reaches a relay's devices first (the fleet's rollout); the relay itself is asked once every ring of that release is done. Devices reconnect once.

## 4. Consequences

- The relay's upgrade has the same proof, dead-man and rollback as a device's; `/api/health` tells its outcome.
- Ops owns the relay's upgrade (one command per relay); the fleet does not ask the relay like a device.
- The 0.6.7.172 relay is upgraded to the current release after LiveBarn's first rollout completes.

## 5. Rejected alternatives

- **netage-server asking the relay like a device.** The relay is not a tunnel device; its operator API is ops territory, and a relay-side failure during a rollout should not be the fleet's to retry.
- **Keeping the scp path for speed.** It is the one unverified binary swap left in the system.
