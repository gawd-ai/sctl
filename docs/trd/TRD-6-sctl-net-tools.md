# TRD-6: sctl net tools

Implements [ADR-006](../adr/006-the-agent-carries-its-own-network-tools.md). Status: Implemented (2026-10-08): slices 1 and 2 shipped in 0.6.11; slice 3 is recorded below once the fleet's rollout reaches the units.

## 1. Objective

`sctl net snmp|http|tcp` work the same on every unit, take secrets from the environment, and answer in JSON, so a runbook or recovery action written once runs anywhere.

## 2. Deliverables

The agent had no SNMP encoder to share: its Infra `snmp` check ran `snmpget`, which the units do not ship. The plan was corrected on 2026-10-06 before any code.

- `server/src/net_tools/` (new):
  - `snmp.rs`: v1/v2c `get`, `walk` (GETNEXT bounded to the subtree, stopping on an agent that does not advance) and `set` (integer or string) on the `snmp2` crate (default features off: no v3 crypto; `heap_buffers` keeps its two 64 KiB buffers off the task stack). Values as JSON, an agent refusal named by its RFC 3416 status.
  - `http.rs`: one request through `routes::fetch::execute` (the agent's own TLS stack and its pin, CA, pin-store and opt-in TOFU ladder), a cookie jar file (0600) kept between steps, redirects followed (303, and 301/302 after a POST, continue as GET), form bodies, Basic auth.
  - `tcp.rs`: does a port answer, and how fast.
  - `mod.rs`: `sctl net snmp|http|tcp` and `sctl help net`; one JSON object on stdout, exit 0, 1 on failure, 2 on a usage error. The community comes from `-c` or `SCTL_SNMP_COMMUNITY` and is never defaulted for a set.
- `routes/fetch.rs`: `execute` is public; the response lists every `Set-Cookie` (`set_cookies`).
- Infra (`infra/checks.rs`): the SNMP check is a GET of sysDescr through `net_tools::snmp`; an HTTP(S) check without `expected_status` counts any answer as up.
- Recovery (`infra/mod.rs`, `infra/monitor.rs`): `RecoveryConfig.credential_ids` puts entry n in the command's environment as `SCTL_CRED_USER_n`/`SCTL_CRED_PASS_n` from the unit's credential store; a missing credential fails the run; passwords are masked in the recovery log. The fleet sends `credential_ids` and lets the agent fire such a recovery by itself for agents from the release that carries this TRD.
- `docs/http-api.md` (`set_cookies`), CHANGELOG (CRLF).

## 3. Verification

Unit tests: a loopback SNMP agent (GET round trip with the community on the wire, SET with the value read back, a refusal named, a walk on an agent that does not advance, silence as a timeout), a loopback HTTP login that sets two cookies and redirects after a POST, the jar, form encoding, redirect resolution, a TCP port open and closed, the recovery environment and its mask, the HTTP check without an expected status. `cargo fmt --all --check` and `cargo clippy --workspace --all-targets -- -D warnings` as CI. The mips payload measured against 0.6.10: `sctl-server-mips_24kc.gz` 3,642,535 to 3,694,561 bytes (+52 KB, 1.4 %), built with `devices/xe300/build.sh` on 2026-10-06. Then the bench rehearsal of fleet TRD-3, the release proof and the fleet's rollout.

## 4. Slices and status

1. snmp, http, tcp subcommands: Implemented (2026-10-06, `5b2931e`). 2. HTTP check meaning: Implemented (2026-10-06, `5b2931e`). 3. Released (0.6.11, 2026-10-08) and rolled out: Planned until the fleet's rollout.
