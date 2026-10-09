# TRD-2: Ramboot on loader-run units and cache staging

Implements [ADR-002](../adr/002-flash-constrained-units-boot-from-the-relays-mirror.md). Status: Planned. Release: sctl 0.6.9.

## 1. Objective

A ramboot unit started under a loader (the WE826) can run the upgrade helper; staging never trusts a cached payload by its size; the WE826 boots from the relay's mirror and carries `install.json`; the relay keeps the previous bundle while a ramboot unit may roll back to it. The RUT241 already runs the layout (migrated 2026-10-01 by `devices/rut241/ramboot-migrate.sh`).

## 2. Deliverables

- `server/src/upgrade/install.rs`: `InstallInfo.helper_prefix: Vec<String>` from `install.json` (`"helper_prefix": ["/tmp/sctl/lib/libc.so", "--library-path", "/tmp/sctl/lib"]`), default empty; `report()` includes it; the parse test covers it.
- `server/src/upgrade/stage.rs` `spawn_helper`: when the prefix is set, the helper binary is the running server's own file named by `ramboot.conf`'s `BIN` (read once at stage time, `install.ramboot_conf`), copied to the stage as today, and started as `<prefix...> <helper copy> upgrade-apply --stage <dir>`; `current_exe()` is used only when the prefix is empty. A unit test with a fake prefix script asserts the argv.
- `server/src/upgrade/fetch.rs` / `stage.rs`: a destination that already exists is kept only when its SHA-256 equals the manifest's (hash it first, skip the download when equal); otherwise it is truncated before the download. Tests: equal (no download), larger-than-expected stale file (truncated and fetched), partial file (resumed with `If-Range` as today).
- `devices/we826-qwd/install.sh`, `sctl.toml.template`, `README.md`: `install.json` gains `helper_prefix`; the payload URLs default to the relay's mirror for the version being installed (`SERVER_URL` and friends derived from `MIRROR_BASE=<http://relay:8081/artifacts/<version>>` and the manifest's names, or given explicitly as today); the README's bring-up section names the relay mirror, not a laptop.
- `docs/upgrade.md`: `helper_prefix` in the `install.json` key table; the layout table's `ramboot` row names the RUT241; a "Bundle retention" paragraph: a relay keeps a version and its mirror until no ramboot unit may still roll back to it, and the fleet's operations doc owns the removal step.
- `devices/rut241/README.md`: the ramboot section stays as written on 2026-10-01; add that its rollback re-fetches the previous version from the previous mirror.
- `CHANGELOG.md` `[0.6.9]` (CRLF).

## 3. Verification

- `cargo test` in `server/` (new tests above), `cargo clippy --workspace --all-targets -- -D warnings` with pedantic as CI runs it, `cargo fmt --all --check`.
- The netns rehearsal (`scratchpad/upg` bench): a ramboot-layout fake agent started through a stub loader script upgrades from the bench relay (good bundle: `done`/`ok`), rolls back a bad binary (`rolled_back`, the previous `ramboot.conf` restored, the fetcher re-downloads the previous payload from the bench mirror), and a stale oversized cache file is replaced.
- The ZBT WE826 bench unit, installed with `helper_prefix`: an upgrade to a bench bundle and a rollback, end to end, over its real loader.
- Release 0.6.9 through the `Release` workflow with TRD-1's proof; upload to do-toronto with its mirror; the RUT241 takes 0.6.9 through the fleet's rollout (its first self-upgrade) and reports `done`.
- Bus 01 WE826, when online and with the owner's OK: `ramboot.conf` repointed at the relay's mirror (the version the bench proved) and `install.json` written, with the bus's dead-man switch (restore the previous `ramboot.conf` and restart unless the tunnel is back within 120 s); then it is a rollout citizen. Bus 01 runs 0.6.2, older than any agent that reads `install.json`, so this step is [TRD-8](TRD-8-we826-guarded-move-to-the-current-ramboot-layout.md)'s guarded move (2026-10-09).

## 4. Slices and status

1. `helper_prefix` (install.rs, stage.rs `spawn_helper` through the prefix, the helper copied from `ramboot.conf`'s `BIN`; tests: parse and report, the argv through a stub loader): Implemented (2026-10-01).
2. SHA-first cache staging (`stage.rs` `cache_disposition`: keep by hash, drop a stale file at least as large, resume a smaller one; test with the four cases): Implemented (2026-10-01).
3. WE826 install (`MIRROR_BASE`, `helper_prefix`) and docs: Implemented (2026-10-01).
4. Local rehearsal: done (2026-10-01). A ramboot-layout agent (x86_64, 0.6.8.201) started by the real boot fetcher (`devices/common/sctl-ramboot.sh`, `SCTL_RAMBOOT_CONFIG`) through a stub loader script named by `helper_prefix`, with the live payloads in its cache, a bench relay serving two bundles signed with the release key and a plain-HTTP mirror of them. The good bundle (0.6.8.9999) over a cache file padded past the new payload's size: the stale file was dropped and fetched afresh (the old rule would have refused `sha256_mismatch`), the helper ran through the loader words, `ramboot.conf` was rewritten to the bundle's mirror, the fetcher restarted the agent from cache, health ok at 15 s. The bad bundle (0.6.8.10000, a script as the server): swapped, the fetcher started it from cache, 180 s of no health, `ramboot.conf` restored, the fetcher logged "discarding cached server payload with invalid SHA-256" then "fetching server payload" from the previous mirror and started 0.6.8.9999, outcome `rolled_back`. Four fetcher starts in the journal, each accounted for. The stub loader `exec`s the binary, so unlike the musl loader it does not stay the process image; the `BIN`-from-`ramboot.conf` path is covered by the unit test and by the real loader on the ZBT. ZBT bench: Planned. The unit is not on the bench (a technician has it); the owner released the order on 2026-10-01: 0.6.9 ships first and the ZBT is benched when it is back.
5. Release 0.6.9: done (2026-10-01). `v0.6.9` built 0.6.9.188 through the `Release` workflow with TRD-1's proof; the bundle is on do-toronto, complete, with its mirror (`http://174.138.114.209:8081/artifacts/0.6.9.188`); 0.6.8.179 and 0.6.7.172 stay on the relay (the RUT241 runs 0.6.8.179 and would roll back to it). The fleet offered it once per tenant from the relay's live frame sequence and drafted LiveBarn's rollout with the RUT241 in ring 1: its first self-upgrade runs when that draft is approved, and is recorded here. Bus 01 and the ZBT: Planned, when each is reachable.
