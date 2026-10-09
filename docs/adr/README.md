# Architecture decision records

A decision that shapes the agent, the relay or the way they are built and released gets an ADR here: `NNN-slug.md`, three digits. Status `Proposed` → `Accepted (<date>)`; an accepted ADR is frozen and is only ever `Superseded by ADR-NNN`. The build spec that realizes a decision is a TRD in `../trd/`; the ADR names it (`Realized by`), the TRD names the ADR (`Implements`). `docs/upgrade.md`, `docs/config.md` and `docs/http-api.md` stay the wire and configuration contracts; ADRs cite them.

| ADR | Title | Status | Realized by |
|---|---|---|---|
| [001](001-an-artifact-carries-the-version-its-manifest-names.md) | An artifact carries the version its manifest names, and CI proves it | Accepted (2026-10-01) | [TRD-1](../trd/TRD-1-release-integrity-guard.md) |
| [002](002-flash-constrained-units-boot-from-the-relays-mirror.md) | Flash-constrained units boot from the relay's mirror | Accepted (2026-10-01) | [TRD-2](../trd/TRD-2-ramboot-on-loader-run-units-and-cache-staging.md), [TRD-8](../trd/TRD-8-we826-guarded-move-to-the-current-ramboot-layout.md) (a WE826 installed before 0.6.9) |
| [003](003-the-relay-upgrades-itself-through-the-managed-path-last.md) | The relay upgrades itself through the managed path, last | Accepted (2026-10-01) | [TRD-3](../trd/TRD-3-managed-relay-upgrade.md) |
| [004](004-relay-route-ownership-on-units-with-a-wireguard-pin.md) | Relay route ownership on units with a WireGuard pin | Proposed (2026-10-01) | [TRD-4](../trd/TRD-4-relay-route-ownership-bench-and-rollout.md) |
| [005](005-the-agent-reports-the-units-own-uplink-verdict.md) | The agent reports the uplink verdict of the unit's own failover engine (mwan3) beside the kernel's routes | Accepted (2026-10-05) | [TRD-5](../trd/TRD-5-mwan3-verdict-in-net-state.md) |
| [006](006-the-agent-carries-its-own-network-tools.md) | The agent carries its own network tools | Accepted (2026-10-05) | [TRD-6](../trd/TRD-6-sctl-net-tools.md) |
| [007](007-the-agent-reports-power-only-from-a-source-the-unit-publishes.md) | The agent reports a unit's power only from a source the unit already publishes; the XE300 publishes none | Accepted (2026-10-08), option A | None: no agent code; TRD-7 only if option B is ever taken |
