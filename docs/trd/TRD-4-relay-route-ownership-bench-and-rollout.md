# TRD-4: Relay route ownership bench and rollout

Implements [ADR-004](../adr/004-relay-route-ownership-on-units-with-a-wireguard-pin.md), which it accepts when the bench passes. Status: Planned; scheduled by the owner.

## 1. Objective

Prove on hardware that a unit whose wire is up but cannot reach the relay moves its management tunnel to LTE and back, with WireGuard following; then apply the setting to every unit, one at a time.

## 2. Deliverables

- A bench recipe in `devices/xe300/README.md` ("Relay route ownership"): on one XE300 at a quiet hour, `uci set network.wg0.nohostroute='1'; uci commit network; ifup wg0`, guarded by a detached dead-man that restores the setting and `ifup wg0` unless the tunnel is connected within 120 s; the test: block the relay's address on the wire (`iptables -I OUTPUT -o eth1 -d <relay> -j DROP`, with a timed removal), expect `/api/health` `relay_route.dev` to become the LTE interface within the suspect window and the tunnel to reconnect; remove the block, expect the switchback; record both in this TRD.
- `devices/xe300/install.sh` and `devices/xe300/upgrade.sh` (via `relay-route.sh`): set `nohostroute` on `wg0` when `/lib/netifd/proto/wireguard.sh` honours it (the README's handler check becomes code), and say so in the device's log; a rollback restores the previous value.
- The BPI: a recipe in the migration-010 runbook replacing steps 8, 9 and 12: `sctl.toml` loses `bind_address` and gains `relay_route = "prefer"`, `relay_route_prefer = ["eth5", "wwan0"]`, through the file API, with a dead-man that restores the previous `sctl.toml` and restarts unless the tunnel is back within 120 s; the pins stay. Find the origin of the eth5 metric-10 pin first (not uci, not netifd's WireGuard handler, not rc.local) and record it.
- `docs/config.md`: the `relay_route` section names WireGuard's endpoint pin and `nohostroute`.

## 3. Verification

- The bench outcomes (failover time, switchback time, WireGuard handshake over LTE) recorded here with dates.
- Then one unit at a time with the owner's OK, at a quiet hour, each verified by `/api/health` (`relay_route.dev` set, `via` set, tunnel connected) and by the fleet's device page.

## 4. Slices and status

1. Bench on one XE300: Planned.
2. XE300 scripts: Planned.
3. BPI prefer: Planned.
