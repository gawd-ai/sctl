# TRD-1: Release integrity guard

Implements [ADR-001](../adr/001-an-artifact-carries-the-version-its-manifest-names.md). Status: Implemented (2026-10-01); slice 2, the first release run with the proof, is recorded at the next tag.

## 1. Objective

The `Release` workflow refuses to sign and publish a bundle in which any server artifact does not embed the manifest's version, and the docs say how a hand build matches a release. The pin fix itself (`rundev.sh`, `devices/common/build-lib.sh`) is on `main` since `7ed5bcc` and `1c2b943`.

## 2. What the artifacts embed (measured on the 0.6.8.179 bundle)

The four-part version is a compile-time constant in every **server** binary, inside a longer merged string (`strings -n 5 | grep -q "0.6.8.179"` matches on `sctl-x86_64` and on the gunzipped `sctl-server-mipsel_24kc.gz`; `grep -x` does not, the line is not the version alone). The comms plugins, `libc` and `libgcc` embed no version. The 0.6.7.172 bundle's OpenWrt servers embed `0.6.7.1` and fail the same test.

## 3. Deliverables

- `.github/workflows/release.yml`, job `bundle`, step `Assemble`, after `scripts/release-bundle.sh` and before signing: for every entry of `release/release.json` with role `server` (`jq -r '.targets[].files[] | select(.role == "server") | "\(.name)|\(.gzip)"'`), read the file (`gzip -dc` when `gzip` is true) and require `strings -n 5 | grep -qE "(^|[^0-9.])$VERSION([^0-9]|$)"`; print `proof ok <name>` or `proof FAILED <name>` and exit 1 at the end if any failed. The step runs before `release-sign.sh verify` so an unproven bundle is never signed.
- `scripts/release-bundle.sh`: no change (it names files and hashes; the proof is CI's).
- `docs/releasing.md`: under "Managed upgrades (0.6.7+)", the proof step; under "Version scheme", that a preset `SCTL_BUILD_NUMBER` wins in `rundev.sh` and the device builds and that a hand build meant to match a release pins it (`SCTL_BUILD_NUMBER=<n>`), with the 0.6.7.172 incident as the reason.
- `devices/rut241/README.md`: the probe env files under `/root/probes/<serial>.env` on the relay are not authoritative for a unit's API key (D4C5's was stale on 2026-10-01); the live key is netage-server's `[[fleet.device_keys]]` entry for that serial.
- `CHANGELOG.md` `[Unreleased]` (CRLF, edited byte-safe): the proof, the pin.

## 4. Verification

- Locally, the step's shell runs over `scratchpad/release-0.6.8` (passes) and `scratchpad/release-0.6.7` (fails on both OpenWrt servers), with `VERSION` read from each `release.json`.
- The next tag's `Release` run shows `proof ok` for the six server artifacts in the `Assemble` step's log, and the bundle verifies as before.
- Negative control: a branch run with `SCTL_BUILD_NUMBER` deliberately wrong in one job must fail the step (one-off, on a `v*` tag of a throwaway branch is not possible; use `workflow_dispatch` on the branch or accept the local negative control above).

## 5. Slices and status

1. Workflow step and docs: Implemented (2026-10-01). Verified locally with the step's shell as written in `release.yml` (`grep` without `-q`: under `pipefail` an early exit made every artifact read as a miss, found and fixed in the rehearsal): over the 0.6.8.179 bundle all six server artifacts print `proof ok`; over the 0.6.7.172 bundle the four cross-built servers pass and `sctl-server-mips_24kc.gz` and `sctl-server-mipsel_24kc.gz` fail, the release is refused with exit 1. The negative control is that second run.
2. First release run with the proof: Planned (the next tag).
