# ADR-002: Flash-constrained units boot from the relay's mirror

Status: Accepted (2026-10-01) for the RUT241, which runs it since 2026-10-01 01:35 UTC at 0.6.8.179; the WE826 and the two hardening items are built by [TRD-2](../trd/TRD-2-ramboot-on-loader-run-units-and-cache-staging.md). Cites `docs/upgrade.md` (the `ramboot` layout, `mirror_base`, the rollback set) and `devices/rut241/README.md`.

## 1. Trigger

The RUT241's writable overlay is 4 MB (`rootfs_data`). It held the 0.6.0 payloads (3.3 MB) with 244 KB free. A 0.6.7 payload set is 3.8 MB: removing the old one frees 3.5 MB, so the new one does not fit beside it, and a `gz-tmp` upgrade (stage beside the target, keep a rollback set on the overlay) was impossible. The WE826 (832 KB overlay) never had room for any payload and has booted from a payload host over HTTP since its bring-up, with that host being a laptop on the bus's LAN.

## 2. What existed

- `docs/upgrade.md` already defines the `ramboot` layout: the overlay holds `/etc/init.d/sctl` (the shared `devices/common/sctl-ramboot.init`), `/etc/sctl/ramboot.sh`, `/etc/sctl/ramboot.conf` (URLs and SHA-256s) and `install.json`; at boot the payloads are fetched into `/tmp/sctl/cache/`, verified and expanded; an upgrade is a rewrite of `ramboot.conf` to a `mirror_base` the request names, with the previous `ramboot.conf` as the rollback set.
- The relay serves its artifact store to devices over its authenticated route and, when a mirror is set on a version (`PUT /api/tunnel/artifacts/{version}/mirror`), names where a boot fetcher gets the same files over plain HTTP. On do-toronto, Caddy already served `/var/lib/netage-payloads` on `:8081` for the WE826; a symlink `artifacts -> /var/lib/sctl/artifacts` makes every bundle a mirror at `http://<relay>:8081/artifacts/<version>/`.
- Two gaps, found while reading the agent for this decision: (a) `stage.rs` copies `current_exe()` as the upgrade helper, but on the WE826 the agent is started by the musl loader (`$LOADER --library-path ... $BIN serve`), so `current_exe()` is the loader and the helper cannot start: the request ends `helper_lost` with `ramboot.conf` untouched; (b) `fetch.rs` treats an existing cache file whose size is at least the expected size as complete, so an old payload no smaller than the new one makes the first request fail `sha256_mismatch` (the next one downloads fresh).
- A rollback restores `ramboot.conf` only; the cached payloads were overwritten in place while staging, so the boot fetcher re-downloads the previous version from the previous mirror.

## 3. Decision

1. **The `ramboot` layout is the layout of every unit whose overlay cannot hold two payload sets.** The RUT241 joins the WE826. The payloads come from the relay's plain-HTTP mirror, SHA-256 verified by the fetcher against `ramboot.conf`, which the agent rewrites on upgrade from the signed manifest.
2. **A relay keeps a bundle, and its mirror, until no ramboot unit may still roll back to it.** The fleet's operations doc says so; the previous release is removed from a relay only after every ramboot unit reports the newer one as `done`.
3. **The upgrade helper runs the way the unit runs its agent.** `install.json` gains `helper_prefix` (a command prefix, default empty); a loader-run unit's installer writes the loader and its library path there, and the agent runs the helper through it and names the real binary, never `current_exe()`, when the prefix is set.
4. **Staging never trusts a cached file by its size.** A file already in the cache is kept only when its SHA-256 is the manifest's; otherwise it is truncated before the download.
5. **The move of an in-service unit to this layout happens through the relay, with no SSH,** by a script that fetches and verifies both payloads into the cache before anything on flash moves, keeps the current init and payloads in RAM, writes the ramboot files, frees the overlay, restarts once, and puts the old layout back unless `/api/health` answers with the version and a connected tunnel twice within 240 s (`devices/rut241/ramboot-migrate.sh`, built and used on 2026-10-01).

## 4. Consequences

- A reboot while the mirror is unreachable leaves the unit without an agent until the mirror answers; the fetcher retries without end (20 attempts per run, respawned by procd). Accepted: it is the WE826's state today, and the alternative is a unit that cannot be upgraded at all.
- The relay's artifact store is public over plain HTTP on `:8081`. Accepted: the artifacts are the public GitHub release; integrity is the signature on the manifest and the SHA-256s in `ramboot.conf`.
- The WE826 cannot self-upgrade until TRD-2 ships `helper_prefix` (sctl 0.6.9); until then its `ramboot.conf` is repointed at the relay mirror by hand, with the bus's dead-man switch.
- A ramboot unit upgrades over whatever carries its default route at that moment; the fleet's busy guard (not on LTE) applies as to every unit.

## 5. Rejected alternatives

- **A smaller mipsel build.** Even 300 KB saved leaves a jffs2 overlay at 97 % with no room for the next release.
- **Payloads on removable storage.** The RUT241 has none.
- **Keeping the cached payloads of the previous version beside the new ones** so that a rollback needs no network. It doubles the tmpfs footprint on 122 MB units; the relay keeping the previous bundle is cheaper and the same guarantee.
