# ADR-005: The agent reports the uplink verdict of the unit's own failover engine (mwan3) beside the kernel's routes

Status: Accepted (2026-10-05). Realized by [TRD-5](../trd/TRD-5-mwan3-verdict-in-net-state.md). Cites `docs/http-api.md` (`net.state`, `GET /api/net`, `GET /api/health`) and `docs/config.md` (`relay_route`).

## 1. Trigger

At Mountain Arena (XE300-9483C446D52E, 2026-10-05) the wired uplink had link, a DHCP lease and a gateway that answered ping, while that gateway returned ICMP net-unreachable for every internet destination: the line above the arena's router was down. The GL-XE300 firmware's failover engine, mwan3 2.8.4, had marked its `wan` interface offline and steered every flow through a policy table to the modem. The kernel's main table still held `default via 192.168.1.1 dev eth1` at metric 0, so `net.state` reported the wire as an up interface holding the lowest-metric default route. Every reader downstream (netage-server, the fleet's uplink record, its alerts and notifications) concluded the wire carried the site. It had not for three days, and when the lease came back on 2026-10-02 the fleet even recorded a switchback to it.

## 2. What existed

- `netwatch` dumps links, IPv4 addresses and main-table routes on every netlink event and publishes a `NetState`; `tunnel/net_state.rs` sends it to the relay as `net.state` v1; `GET /api/net` serves the same. The main table is the only table read (`netwatch/mod.rs`: "A policy table's default route (mwan3) is not the main table's").
- mwan3 keeps its verdict per logical interface in `/var/run/mwan3/iface_state/<iface>` (`online` or `offline`, rewritten on every change), exposes it over `ubus call mwan3 status '{"section":"interfaces"}'` (status, score, track_ip results), and runs `/etc/mwan3.user` with `connected` or `disconnected`. Its logical names (`wan`, `modem_1_1_2`) map to kernel devices through netifd (`ubus call network.interface dump`, `l3_device`).
- The relay-route owner (ADR-004 territory) already probes the relay over each uplink and had moved the management tunnel to LTE; that fact (`mgmt_diverges`) was the only hint.

## 3. Decision

1. **The agent reports the verdict of the unit's own failover engine, per kernel interface, beside the kernel's routes.** `NetState.interfaces[]` gains `verdict`: `online` or `offline` when mwan3 tracks the interface's logical owner, absent otherwise. It rides `net.state`, `GET /api/net` and `GET /api/health` (as `network.verdicts`).
2. **The agent does not interpret it.** The agent still owns only the relay's `/32`; which uplink carries the site's traffic is the failover engine's call, and the fleet's readers decide what to show and alert on (fleet ADR-0016).
3. **No poll.** The verdict directory is watched with inotify; its events wake the same dump-and-publish cycle netlink events do, and the directory is read on every dump. A unit without mwan3 pays nothing: no directory, no watch, no field.
4. **The mapping comes from netifd, once per change.** `ubus call network.interface dump` resolves logical names to devices when the verdicts change; a verdict whose logical interface maps to no device is dropped with a debug line, never guessed.

## 4. Consequences

- A wire with link and a lease but no internet reads as `verdict: offline` on the wire and `online` on the modem; netage-server and the fleet can show "wired down, on LTE backup" and tell the ISP (fleet ADR-0016).
- `net.state` stays v1: the field is additive and optional; older readers ignore it.
- Units without mwan3 (relay, BPI, bench VMs) are unchanged.
- Release 0.6.10 carries it to the XE300s through the fleet's rollout.

## 5. Rejected alternatives

- **Reading the policy tables and fwmark rules from netlink** to infer mwan3's choice: couples the agent to mwan3's table numbering and marks, and still needs the logical-to-device mapping.
- **Running `mwan3 status` or `ubus call mwan3 status` on a timer:** a poll, against the house rule; the state files already change exactly when the verdict does.
- **Hooking `/etc/mwan3.user` to call the agent's API:** a second path into the agent that an install script must write and keep; inotify needs nothing on the unit.
