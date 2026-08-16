# sctl documentation

Start with the doc that matches what you're doing:

| You are… | Read |
|----------|------|
| **Operating devices** — a unit crash-looped, or you're rolling out a release | [safe-mode.md](safe-mode.md) — the safe-mode runbook: inspect, investigate, clear (incl. remotely over the relay) · [releasing.md](releasing.md) — rundev.sh toolbox, version scheme, payload publish-vs-activate, release checklist |
| **Writing an API client** | [http-api.md](http-api.md) — every route, request/response shapes, auth modes, relay access (CI-gated against the registered routes) · [errors.md](errors.md) — the error shape, `retryable` semantics, complete code catalog |
| **Configuring a device or relay** | [config.md](config.md) — every TOML key with its true default, validation rules, env precedence (CI-gated against `config.rs`) · plus the fully-keyed [`sctl.toml.example`](../server/sctl.toml.example) and [`relay.toml.example`](../server/relay.toml.example) |
| **Contributing / understanding the system** | [guide.md](guide.md) — how sctl works, deployment, tunneling, multi-device operations · [design-streaming-jobs.md](design-streaming-jobs.md) — archived design doc for the streaming-jobs primitive (deployed 2026-05) |

Component docs elsewhere in the tree: [`../README.md`](../README.md)
(project overview), [`../devices/README.md`](../devices/README.md) (device
payload targets and per-device install notes), [`../web/`](../web/)
(sctlin), [`../CHANGELOG.md`](../CHANGELOG.md) (the one changelog, per
component).

Two of these docs are CI-gated so they cannot silently rot:
`scripts/check-http-api-docs.py` diffs http-api.md's route headings against
the registered routes, and `scripts/check-config-docs.py` diffs config.md
and sctl.toml.example against the config structs.
