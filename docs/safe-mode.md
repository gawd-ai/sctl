# Safe mode — operator runbook

What to do when a device has engaged safe mode, and how to get it back to
full service — including remotely, over the relay, on a CGNAT unit you
cannot SSH into. Covers sctl 0.6.0.

## What safe mode is

When the supervisor (`sctl supervise`) detects a crash-loop — the child
`sctl serve` exiting abnormally **3 times within 180 s** — it writes
`<data_dir>/safe_mode.flag` (default `/var/lib/sctl/safe_mode.flag`,
written atomically via tmp+rename). The flag is JSON:

```json
{ "since_unix": 1755300000, "reason": "exit code 101", "consecutive_crashes": 3 }
```

The **next** `sctl serve` startup sees the flag and skips every optional
subsystem, keeping only the management plane alive so you can reach the
box, read logs, and clear the flag. The startup log banner is unmissable:

```text
==== SAFE MODE ACTIVE ==== modem/GPS/LTE/watchdog/infra subsystems will be skipped. ...
```

While the flag exists, the supervisor also stretches its restart backoff to
a fixed 300 s (instead of the normal exponential ramp capped at
`[supervisor] max_backoff`) so it does not spin on a flag nobody has
cleared.

The point: the crashing subsystem is almost always an optional one (a modem
driver wedging the comms plugin, an infra probe, the watchdog acting on bad
hardware). Safe mode breaks the loop while keeping the device reachable —
a crash-looping CGNAT unit that stayed fully down would need a site visit.

## What is skipped, what still runs

**Skipped while the flag exists:**

- The comms provider plugin — and with it everything modem-shaped: AT
  access, GPS polling, LTE signal polling, and the LTE recovery watchdog.
  `/api/gps` and `/api/lte/*` answer `503 MODEM_UNAVAILABLE`.
- The infra monitor (site target probing).

**Still live:**

- The HTTP server — exec, files, sessions, playbooks, STP transfers,
  activity, events, diagnostics.
- The tunnel client — the device still registers with its relay, so it
  stays remotely reachable.
- The safe-mode endpoints themselves.

## Inspect

Local or direct:

```sh
curl -H "Authorization: Bearer $API_KEY" http://DEVICE:1337/api/safe_mode/flag
```

- Not in safe mode: `{ "active": false }`
- In safe mode: `{ "active": true, "flag": { "since_unix": ..., "reason": ..., "consecutive_crashes": ... } }`

Remotely, through the relay's generic passthrough (new in 0.6.0 — this is
the designed recovery path for crash-looping CGNAT devices):

```sh
curl -H "Authorization: Bearer $DEVICE_API_KEY" \
  https://RELAY/d/SERIAL/api/safe_mode/flag
```

The key is the **target device's** API key, not the relay's.

## Investigate before clearing

The flag tells you the loop happened; the why is on the device:

- `<data_dir>/last_panic.log` — the panic hook persists the last panic with
  timestamp, thread, and (because the supervisor runs the child with
  `RUST_BACKTRACE=full`) a full backtrace.
- The system log / logread around the crash timestamps.
- `flag.reason` — the exit description the supervisor recorded.

Clearing the flag without fixing the cause just schedules the next
crash-loop: three more crashes and the supervisor re-engages safe mode
(the flag write is idempotent).

## Clear

```sh
# direct
curl -X DELETE -H "Authorization: Bearer $API_KEY" \
  http://DEVICE:1337/api/safe_mode/flag

# via relay
curl -X DELETE -H "Authorization: Bearer $DEVICE_API_KEY" \
  https://RELAY/d/SERIAL/api/safe_mode/flag
```

Responses: `{ "cleared": true }`, or `{ "cleared": false, "reason": "flag
not present" }` — the endpoint is idempotent, deleting a non-existent flag
is OK (200).

**Clearing the flag does not restart the daemon.** The running process
already skipped its subsystems at startup; they come back only on the next
start. So after `DELETE`:

- With an init system: `systemctl restart sctl` / `/etc/init.d/sctl
  restart`.
- Remotely with no SSH: restart through sctl itself (e.g. `POST /api/exec`
  with a detached restart — see the operational notes on restarting sctl
  over its own tunnel), or wait for a natural power cycle; RAM-boot devices
  come up fresh on every boot.

A single deliberate restart does not re-trigger safe mode — the supervisor
needs 3 crashes inside 180 s.

## The flag is the whole interface

There is no safe-mode config section and no API to *enter* safe mode; the
only writer is the supervisor's crash-loop detector, and the only state is
the flag file. Deleting the file by hand over SSH is exactly equivalent to
the `DELETE` endpoint.
