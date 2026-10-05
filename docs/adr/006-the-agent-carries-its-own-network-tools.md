# ADR-006: The agent carries its own network tools

Status: Accepted (2026-10-05). Realized by [TRD-6](../trd/TRD-6-sctl-net-tools.md).

## 1. Trigger

Restoring a LiveBarn arena's internet on 2026-10-05 needed one SNMP set on a power bar. The XE300's firmware has busybox `wget` (no `-S`), `curl`, `nc`, and no SNMP client; the operator built an SNMPv1 packet by hand in shell. Before that, the fleet's assistant spent hours on curl and busybox differences across firmware. The agent already speaks SNMP (its Infra `snmp` check) and HTTP.

## 2. Decision

1. **`sctl net` subcommands, the same on every unit:** `sctl net snmp get|walk|set <host> <oid> [type value]` (v1 and v2c, community from `SCTL_SNMP_COMMUNITY` or the stored-credential environment, never the command line), `sctl net http <method> <url>` (cookies, redirects followed, headers shown, form data, credentials from the environment), `sctl net tcp <host> <port>`. JSON on stdout, a non-zero exit with a one-line reason on failure.
2. **Usable wherever a command runs:** exec, runbooks, recovery actions. Nothing new listens; they are one-shot subcommands of the binary already on the unit.

## 3. Consequences

- A runbook written once works on an XE300, a RUT241, a BPI or a WE826.
- The binary grows by the SNMP encoder it already carries for checks (shared code), plus a small HTTP client path it already has for fetches.
