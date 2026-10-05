# TRD-6: sctl net tools

Implements [ADR-006](../adr/006-the-agent-carries-its-own-network-tools.md). Status: Planned.

## 1. Objective

`sctl net snmp|http|tcp` work the same on every unit, take secrets from the environment, and answer in JSON, so a runbook or recovery action written once runs anywhere.

## 2. Deliverables

- `server/src/net_tools/` (new): `snmp.rs` (v1/v2c get, getnext walk, set for integer and string, reusing the encoder of the Infra `snmp` check), `http.rs` (one request with a cookie jar file, redirects followed up to 5, form or raw body, basic auth from `SCTL_CRED_USER_n`/`SCTL_CRED_PASS_n`), `tcp.rs` (connect with a timeout); a `net` subcommand in the CLI.
- Infra HTTP checks: an optional `expect_status`; without it any HTTP response is up (fleet TRD-3).
- `docs/cli.md` or the README's command list; CHANGELOG (CRLF).

## 3. Verification

Unit tests against a loopback SNMP responder and HTTP server; the bench rehearsal of fleet TRD-3; fmt and clippy as CI; release through the proof and the fleet's rollout.

## 4. Slices and status

1. snmp, http, tcp subcommands: Planned. 2. HTTP check meaning: Planned. 3. Released and rolled out: Planned.
