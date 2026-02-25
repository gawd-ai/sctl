---
name: openwrt-network-setup
description: OpenWrt network configuration — WiFi check/reset, DNS/DHCP status, VLAN review, interface diagnostics, firewall zones
params:
  action:
    type: string
    description: Action to perform
    default: check
    enum: [check, reset-wifi, restart-network]
  interface:
    type: string
    description: Interface to check (or 'all' for all interfaces)
    default: all
  verbosity:
    type: string
    description: Output detail level
    default: normal
    enum: [brief, normal, verbose]
---

```sh
#!/bin/sh
# OpenWrt Network Setup Playbook
# WiFi check/reset, DNS/DHCP, VLANs, interface diagnostics, firewall zones

ACTION="{{action}}"
INTERFACE="{{interface}}"
VERBOSITY="{{verbosity}}"

sep() { printf '\n══════ %s ══════\n' "$1"; }

# ── WiFi Reset ─────────────────────────────────────────────────────────────
# Only runs when action=reset-wifi; resets wireless config and restarts radios

if [ "$ACTION" = "reset-wifi" ]; then
    sep "WIFI RESET"
    if ! command -v uci >/dev/null 2>&1; then
        echo "ERROR: uci not available — cannot reset wifi"
        exit 1
    fi

    echo "── Current wireless config (before reset) ──"
    uci show wireless 2>/dev/null | grep -v '\.key=' || echo "(no wireless config)"
    echo ""

    echo "── Disabling all radios ──"
    for radio in $(uci show wireless 2>/dev/null | grep '=wifi-device$' | cut -d. -f2 | cut -d= -f1); do
        echo "  Disabling $radio..."
        uci set "wireless.${radio}.disabled=1"
    done
    uci commit wireless
    wifi down 2>/dev/null || echo "(wifi down failed)"
    sleep 2

    echo "── Re-enabling all radios ──"
    for radio in $(uci show wireless 2>/dev/null | grep '=wifi-device$' | cut -d. -f2 | cut -d= -f1); do
        echo "  Enabling $radio..."
        uci set "wireless.${radio}.disabled=0"
    done
    uci commit wireless
    echo ""

    echo "── Restarting wifi ──"
    wifi up 2>/dev/null || /etc/init.d/network restart
    sleep 3

    echo ""
    echo "── Wireless config (after reset) ──"
    uci show wireless 2>/dev/null | grep -v '\.key=' || echo "(no wireless config)"
    echo ""

    if command -v iwinfo >/dev/null 2>&1; then
        echo "── Radio status ──"
        iwinfo 2>/dev/null || echo "(iwinfo failed)"
    fi

    sep "WIFI RESET COMPLETE"
    echo "Reset completed at $(date)"
    exit 0
fi

# ── Network Restart ────────────────────────────────────────────────────────
# Only runs when action=restart-network; restarts the network subsystem

if [ "$ACTION" = "restart-network" ]; then
    sep "NETWORK RESTART"

    echo "── Interfaces before restart ──"
    ip -br addr 2>/dev/null || ifconfig
    echo ""

    echo "── Restarting network service ──"
    if [ -x /etc/init.d/network ]; then
        /etc/init.d/network restart
        sleep 5
    else
        echo "ERROR: /etc/init.d/network not found"
        exit 1
    fi

    echo ""
    echo "── Interfaces after restart ──"
    ip -br addr 2>/dev/null || ifconfig
    echo ""

    echo "── Default routes ──"
    ip route show default 2>/dev/null || route -n | grep "^0.0.0.0"

    sep "NETWORK RESTART COMPLETE"
    echo "Restart completed at $(date)"
    exit 0
fi

# ── Check Mode (default) ──────────────────────────────────────────────────
# Shows information only, makes no changes

# ── WiFi Config ────────────────────────────────────────────────────────────

sep "WIFI CONFIG"
if command -v uci >/dev/null 2>&1; then
    uci show wireless 2>/dev/null | grep -v '\.key=' || echo "(no wireless config found)"
else
    echo "(uci not available)"
fi

if [ "$VERBOSITY" != "brief" ]; then
    echo ""
    echo "── Radio status ──"
    if command -v iwinfo >/dev/null 2>&1; then
        for dev in $(iwinfo 2>/dev/null | grep "ESSID" | awk '{print $1}'); do
            echo ""
            echo "── $dev ──"
            iwinfo "$dev" info 2>/dev/null
            if [ "$VERBOSITY" = "verbose" ]; then
                echo ""
                echo "  ── Associated stations ──"
                iwinfo "$dev" assoclist 2>/dev/null || echo "  (no stations)"
                echo ""
                echo "  ── Scan results ──"
                iwinfo "$dev" scan 2>/dev/null | head -40 || echo "  (scan not available)"
            fi
        done
    else
        echo "(iwinfo not available)"
        iw dev 2>/dev/null || echo "(iw not available either)"
    fi
fi

# ── DNS / DHCP Status ─────────────────────────────────────────────────────

sep "DNS / DHCP STATUS"
echo "── DHCP config ──"
if command -v uci >/dev/null 2>&1; then
    uci show dhcp 2>/dev/null || echo "(no dhcp config)"
else
    echo "(uci not available)"
fi

echo ""
echo "── dnsmasq process ──"
pidof dnsmasq >/dev/null 2>&1 && echo "dnsmasq: running (PID $(pidof dnsmasq))" || echo "dnsmasq: NOT running"

echo ""
echo "── DHCP leases ──"
if [ -f /tmp/dhcp.leases ]; then
    cat /tmp/dhcp.leases
    echo ""
    echo "  ($(wc -l < /tmp/dhcp.leases) active leases)"
else
    echo "(no lease file at /tmp/dhcp.leases)"
fi

echo ""
echo "── DNS resolv config ──"
cat /etc/resolv.conf 2>/dev/null || echo "(no /etc/resolv.conf)"
if [ "$VERBOSITY" != "brief" ]; then
    echo ""
    echo "── Auto-generated resolv ──"
    cat /tmp/resolv.conf.d/resolv.conf.auto 2>/dev/null || echo "(not available)"
fi

if [ "$VERBOSITY" = "verbose" ]; then
    echo ""
    echo "── dnsmasq log (last 20 lines) ──"
    logread 2>/dev/null | grep -i dnsmasq | tail -20 || echo "(logread not available)"
fi

# ── VLAN Review ────────────────────────────────────────────────────────────

sep "VLAN REVIEW"
if command -v uci >/dev/null 2>&1; then
    echo "── VLAN devices ──"
    uci show network 2>/dev/null | grep -E '(bridge-vlan|vlan|vid|ports)' || echo "(no VLAN config found)"

    echo ""
    echo "── Bridge config ──"
    uci show network 2>/dev/null | grep -E '(=bridge|\.type=|\.ports=|\.ifname=)' || echo "(no bridge config found)"

    if [ "$VERBOSITY" = "verbose" ]; then
        echo ""
        echo "── Switch config (DSA/swconfig) ──"
        uci show network 2>/dev/null | grep -E '(=switch|switch_vlan)' || echo "(no switch config — may be using DSA)"
        echo ""
        echo "── Bridge details ──"
        for br in $(ls /sys/class/net/*/bridge/bridge_id 2>/dev/null | cut -d/ -f5); do
            echo ""
            echo "  ── $br ──"
            echo "  STP: $(cat /sys/class/net/$br/bridge/stp_state 2>/dev/null || echo 'unknown')"
            ls /sys/class/net/$br/brif/ 2>/dev/null | while read port; do
                echo "    port: $port"
            done
        done
    fi
else
    echo "(uci not available)"
fi

# ── Interface Diagnostics ─────────────────────────────────────────────────

sep "INTERFACE DIAGNOSTICS"
if [ "$INTERFACE" = "all" ]; then
    echo "── All interfaces ──"
    if command -v ip >/dev/null 2>&1; then
        ip -br addr
        if [ "$VERBOSITY" != "brief" ]; then
            echo ""
            echo "── Link state ──"
            ip -br link
        fi
        if [ "$VERBOSITY" = "verbose" ]; then
            echo ""
            echo "── Full ip addr ──"
            ip addr
        fi
    else
        ifconfig
    fi

    echo ""
    echo "── ubus network status ──"
    if command -v ubus >/dev/null 2>&1; then
        for iface in $(ubus list 2>/dev/null | grep '^network\.interface\.' | sed 's/network\.interface\.//'); do
            echo ""
            echo "── $iface ──"
            if [ "$VERBOSITY" = "brief" ]; then
                ubus call "network.interface.${iface}" status 2>/dev/null | grep -E '("up"|"l3_device"|"proto"|"ipv4-address"|"address")' || echo "  (status unavailable)"
            else
                ubus call "network.interface.${iface}" status 2>/dev/null || echo "  (status unavailable)"
            fi
        done
    else
        echo "(ubus not available)"
    fi
else
    echo "── Interface: $INTERFACE ──"
    if command -v ip >/dev/null 2>&1; then
        ip addr show "$INTERFACE" 2>/dev/null || echo "  (interface $INTERFACE not found)"
        echo ""
        ip -s link show "$INTERFACE" 2>/dev/null || echo "  (no stats for $INTERFACE)"
    else
        ifconfig "$INTERFACE" 2>/dev/null || echo "  (interface $INTERFACE not found)"
    fi

    echo ""
    echo "── ubus status ──"
    if command -v ubus >/dev/null 2>&1; then
        ubus call "network.interface.${INTERFACE}" status 2>/dev/null || echo "  (no ubus status for $INTERFACE — try UCI interface name)"
    else
        echo "(ubus not available)"
    fi
fi

if [ "$VERBOSITY" != "brief" ]; then
    echo ""
    echo "── Default routes ──"
    ip route show default 2>/dev/null || route -n | grep "^0.0.0.0"

    echo ""
    echo "── All routes ──"
    ip route 2>/dev/null || route -n
fi

if [ "$VERBOSITY" = "verbose" ]; then
    echo ""
    echo "── Network UCI config ──"
    uci show network 2>/dev/null || echo "(uci not available)"
fi

# ── Firewall Zones ─────────────────────────────────────────────────────────

sep "FIREWALL ZONES"
if command -v fw4 >/dev/null 2>&1; then
    echo "── fw4 zones ──"
    fw4 zone 2>/dev/null || echo "(fw4 zone failed)"
elif command -v fw3 >/dev/null 2>&1; then
    echo "── fw3 zones ──"
    fw3 zone 2>/dev/null || echo "(fw3 zone failed)"
else
    echo "(no fw4/fw3 available)"
fi

echo ""
echo "── Zone-to-interface mapping ──"
if command -v uci >/dev/null 2>&1; then
    uci show firewall 2>/dev/null | grep -E '(=zone$|\.name=|\.network=|\.input=|\.output=|\.forward=)' || echo "(no firewall zone config)"
else
    echo "(uci not available)"
fi

if [ "$VERBOSITY" != "brief" ]; then
    echo ""
    echo "── Firewall rules ──"
    if command -v uci >/dev/null 2>&1; then
        uci show firewall 2>/dev/null | grep -E '(=rule$|\.name=|\.src=|\.dest=|\.proto=|\.dest_port=|\.target=)' || echo "(no firewall rules)"
    fi
fi

if [ "$VERBOSITY" = "verbose" ]; then
    echo ""
    echo "── NAT / masquerade ──"
    iptables -t nat -L -n 2>/dev/null || nft list table inet fw4 2>/dev/null | head -50 || echo "(neither iptables nor nft available)"

    echo ""
    echo "── Forwarding rules ──"
    uci show firewall 2>/dev/null | grep -E '(=forwarding$|\.src=|\.dest=)' || echo "(no forwarding rules)"

    echo ""
    echo "── Network log (last 20 lines) ──"
    logread 2>/dev/null | grep -iE '(netifd|network|interface|dhcp)' | tail -20 || echo "(logread not available)"
fi

sep "DONE"
echo "Network setup check completed at $(date)"
```
