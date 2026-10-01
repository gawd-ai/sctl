# ADR-001: An artifact carries the version its manifest names, and CI proves it

Status: Accepted (2026-10-01). Realized by [TRD-1](../trd/TRD-1-release-integrity-guard.md). Cites `docs/upgrade.md` (the release bundle and the version proof) and `docs/releasing.md` (the version scheme).

## 1. Trigger

The first bundle published by the `Release` workflow, 0.6.7.172, carried OpenWrt payloads (`mips_24kc`, `mipsel_24kc`) whose binaries answer `sctl 0.6.7.1`. The workflow's `meta` job computed the build number from a full checkout and passed it to every build job as `SCTL_BUILD_NUMBER`, but the OpenWrt jobs check out shallowly and `devices/common/build-lib.sh` recomputed the number from `git rev-list --count HEAD` of that checkout (one commit) over the preset. The cross-built targets were right because `Cross.toml` passes the variable through unchanged. Nothing in the pipeline compared what a binary says with what `release.json` names, so the bundle was signed, uploaded and listed as complete.

The defect was found by the RUT241's migration script, which runs the staged server's `--version` before anything on flash moves, and refused. An XE300 asked to upgrade from that bundle would have staged, proved the binary, found `0.6.7.1`, and refused with `binary_does_not_run` (`docs/upgrade.md`), so no unit was harmed; but the relay held a release that could not upgrade any OpenWrt unit, and the fleet offered it as one.

The same week the hand path (`rundev.sh device upgrade-remote`) stamped three units `0.6.7.173` because a docs commit landed on the checkout between two builds: the number came from the checkout there too (`rundev.sh:61`).

## 2. What existed

- `server/build.rs`: a preset `SCTL_BUILD_NUMBER` wins, else `git rev-list --count HEAD`.
- `rundev.sh:61`: `export SCTL_BUILD_NUMBER="$(git rev-list --count HEAD)"`, unconditionally.
- `devices/common/build-lib.sh` `build_number()`: `git rev-list --count HEAD`, unconditionally, exported over the environment.
- `.github/workflows/release.yml`: `meta` computes the number from a full checkout; the build jobs set it in their environment; the `cross` job prints `--version` with `|| true`; the `openwrt` job prints nothing; `scripts/release-bundle.sh` checks sizes and hashes only.

## 3. Decision

1. **One source for the build number.** A preset `SCTL_BUILD_NUMBER` wins everywhere a binary is built: `server/build.rs` (already), `rundev.sh` (`7ed5bcc`) and `devices/common/build-lib.sh` (`1c2b943`). CI sets it once, from the full checkout's count, and every job inherits it. A hand build meant to match a release pins it (`SCTL_BUILD_NUMBER=<n> ./rundev.sh device upgrade-remote <name>`); a hand build that does not pin it is a bench build and says so by its number.
2. **The release job proves every server artifact.** After `scripts/release-bundle.sh` assembles the bundle, the `Assemble` step fails unless every artifact of role `server` embeds the manifest's `version` (the four-part string is a compile-time constant in the binary; the OpenWrt payloads are gunzipped first). Plugins, `libc` and `libgcc` carry no version and are not checked. A bundle whose proof fails is never signed, published or uploaded.
3. **The proof is the manifest's.** `release.json`'s `version` is the only name of a release; a binary that answers another string is not that release, whatever its hashes say.

## 4. Consequences

- A bundle on a relay is trusted to upgrade every target it lists; the fleet's "available" means that.
- The 0.6.7.172 bundle stays on the relay only while a ramboot unit may still roll back to it (ADR-002); it is never offered to an OpenWrt unit again (the fleet offers newer stable releases only, and 0.6.8.179 supersedes it).
- The pin fix is already on `main`; the proof is built by TRD-1 and exercised by the next tag.

## 5. Rejected alternatives

- **A full checkout in the OpenWrt jobs only.** It hides the next override; the number must have one owner wherever a build runs.
- **Comparing `--version` output in CI.** Only the native binary can run there; `strings` on the embedded constant covers every target the same way, which is what the agent itself checks after a swap.
