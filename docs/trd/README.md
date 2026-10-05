# Technical requirements documents

A TRD says what to build, in which files, and how it is accepted: the implementation counterpart of an ADR. `TRD-N-slug.md`, numbered without padding. Status `Planned` → `Implemented (<date>)`, flipped in the same change that ships it, with the verification it actually ran. A TRD names the ADR it implements; an ADR names the TRD that realizes it.

| TRD | Title | Implements | Status |
|---|---|---|---|
| [TRD-1](TRD-1-release-integrity-guard.md) | Release integrity guard | [ADR-001](../adr/001-an-artifact-carries-the-version-its-manifest-names.md) | Implemented (2026-10-01) |
| [TRD-2](TRD-2-ramboot-on-loader-run-units-and-cache-staging.md) | Ramboot on loader-run units and cache staging | [ADR-002](../adr/002-flash-constrained-units-boot-from-the-relays-mirror.md) | Planned |
| [TRD-3](TRD-3-managed-relay-upgrade.md) | Managed relay upgrade | [ADR-003](../adr/003-the-relay-upgrades-itself-through-the-managed-path-last.md) | Implemented (2026-10-01) |
| [TRD-4](TRD-4-relay-route-ownership-bench-and-rollout.md) | Relay route ownership bench and rollout | [ADR-004](../adr/004-relay-route-ownership-on-units-with-a-wireguard-pin.md) | Planned |
| [TRD-5](TRD-5-mwan3-verdict-in-net-state.md) | mwan3's verdict in net.state | [ADR-005](../adr/005-the-agent-reports-the-units-own-uplink-verdict.md) | Planned |
