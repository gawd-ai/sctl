# TRD-5: mwan3's verdict in net.state

Implements [ADR-005](../adr/005-the-agent-reports-the-units-own-uplink-verdict.md). Status: Planned.

## 1. Objective

An XE300 whose wire has link and a lease but no internet reports, in `net.state`, `GET /api/net` and `GET /api/health`, that mwan3 holds the wire `offline` and the modem `online`, within the same second mwan3 decides it, without polling; every other unit reports exactly what it does today.

## 2. Deliverables

- `server/src/netwatch/mwan3.rs` (new): `Verdict` (`Online`, `Offline`, serialized lowercase); `read_states(dir)` reads `<dir>/<iface>` files (`online`, `offline`; anything else ignored); `resolve_devices()` runs `ubus call network.interface dump` (2 s cap) and maps each logical interface to its `l3_device`; `annotate(&mut NetState, dir)` sets `Interface.verdict` for the devices named; `Watcher` opens inotify on the directory (`IN_CLOSE_WRITE | IN_MOVED_TO | IN_CREATE`) when it exists, and when it does not, retries the watch after each dump (a `stat`, no timer); `changed().await` pends forever without a watch.
- `server/src/netwatch/mod.rs`: `Interface.verdict: Option<Verdict>` (part of `same_network` through `PartialEq`); `dump_with` annotates after `from_dump`; `watch_events` waits on the netlink socket or the mwan3 watcher, whichever fires first, then runs the same debounce and dump. `STATE_DIR` is `/var/run/mwan3/iface_state`, overridable for tests and benches by `SCTL_MWAN3_STATE_DIR`.
- `server/src/tunnel/net_state.rs`: `InterfaceEntry.verdict` (`skip_serializing_if` none); the doc comment's example gains it; caps unchanged.
- `server/src/routes/health.rs`: `network.verdicts` (`{"eth1":"offline","wwan0":"online"}`) from the current netwatch state, absent when no interface carries a verdict.
- `docs/http-api.md`: the field in `net.state`, `/api/net` and `/api/health`.
- Tests: `read_states` with a fake directory (online, offline, garbage, missing dir); `annotate` with an injected mapping; `watch_events` wakes on a file write under `SCTL_MWAN3_STATE_DIR` (tokio test with a temp dir); the `net_state::build` test gains a verdict; `same_network` differs on a verdict flip alone.
- `scripts/netns-test.sh` (the existing netns rehearsal): a case that writes `offline` then `online` into a temp state dir and reads the flip from `GET /api/net`.
- CHANGELOG `[Unreleased]` then `[0.6.10]` (CRLF, byte-safe).

## 3. Verification

- `cargo test -p sctl`, `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`.
- The netns rehearsal with the flip.
- On D52E after the rollout: `GET /api/health` shows `network.verdicts.eth1 = offline` while the arena line is down, and the relay's `/api/tunnel/events` carries the field in `net.state`.
- Release 0.6.10 passes the release proof; the bundle and its mirror are on do-toronto with the previous bundles kept.

## 4. Slices and status

1. mwan3 module, netwatch wiring, message and health fields, unit tests: Planned.
2. netns flip case: Planned.
3. Release 0.6.10: Planned.
4. Seen on an XE300 after the fleet's rollout: Planned.
