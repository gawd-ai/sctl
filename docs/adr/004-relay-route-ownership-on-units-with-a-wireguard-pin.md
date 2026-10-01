# ADR-004: Relay route ownership on units with a WireGuard pin

Status: Proposed (2026-10-01); accepted when [TRD-4](../trd/TRD-4-relay-route-ownership-bench-and-rollout.md)'s bench proves the failover on hardware. Cites `docs/config.md` (`relay_route`, `relay_route_prefer`) and `server/src/netwatch/owner.rs` (`LeftAlone`).

## 1. Trigger

After the 0.6.7 upgrades every XE300 and the BPI report `relay_route.dev = null`: the agent-owned relay route of 0.6.5 and 0.6.6 is inert in production. On the XE300s netifd's WireGuard handler pins a `/32` to the peer endpoint (the relay) through the wire at metric 0, and `owner.rs` leaves a foreign metric-0 route alone (`LeftAlone`). The BPI's tunnel is bound to `eth5` outright (`bind_address`) and its pins sit at metric 10 (WireGuard's) and 40 (a uci route on wwan). So the failover the route exists for, "the wire is up but cannot reach the relay", never happens: when the arena's ISP dies with the link light on, the unit goes dark until the ISP returns, SIM or not. When the cable is pulled the kernel drops the pin with the link and the tunnel falls to LTE anyway.

## 2. What existed

- `relay_route = "follow_default" | "prefer"` (0.6.5, 0.6.6): the agent keeps `relay/32 via <gw> dev <uplink> metric 0 proto 83`, probes the relay over the wire, and moves the route to LTE when the wire cannot reach it; refused together with `bind_address`.
- `owner.rs`: a foreign `/32` at metric 0 is left alone; a foreign route at any other metric is outranked by the agent's metric-0 route and left in place.
- The XE300 firmware's `/lib/netifd/proto/wireguard.sh` honours `option nohostroute '1'` (checked on D5C1 on 2026-10-01); the BPI has no netifd WireGuard handler and its metric-10 pin's origin is not yet found.
- The fallback was proven in a network namespace only (0.6.5).

## 3. Proposed decision

1. **On netifd units whose WireGuard handler honours it, `wg0` carries `option nohostroute '1'`,** so WireGuard's traffic to the relay follows the main table, which the agent's route owns; the pin disappears and `LeftAlone` no longer applies. The XE300 install and upgrade scripts set it when the handler honours it, and say so.
2. **On the BPI, `relay_route = "prefer"` with `relay_route_prefer = ["eth5", "wwan0"]` and no `bind_address`.** Its pins stay: both are outranked by the agent's metric-0 route, and the uci route on wwan is the fallback the agent would install itself.
3. **The agent keeps leaving a foreign metric-0 route alone.** Fighting netifd is not the agent's job; the unit's configuration is.

## 4. Consequences

- The "wire up, upstream dead" case fails the tunnel over to LTE within the suspect window, and WireGuard (LAN access) follows it.
- A unit's LAN-access WireGuard traffic may ride LTE during an ISP outage; the metered-link rules apply (`docs/config.md`, the agent's own route is the only management route).
- Decided per unit, at a quiet hour, with a dead-man that restores the setting; bench first.

## 5. Rejected alternatives

- **The agent overriding a foreign metric-0 route.** It would replace netifd's route and netifd would put it back on the next `ifup`.
- **A lower metric for the agent's route.** Metric 0 is the lowest; the pin is at 0 too, and the kernel's tie-break is not a contract.
