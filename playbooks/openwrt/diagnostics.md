---
name: openwrt-diagnostics
description: Comprehensive diagnostics for OpenWrt devices — network, WiFi, system health, firewall, and services
params:
  verbosity:
    type: string
    description: Output detail level
    default: normal
    enum: [brief, normal, verbose]
  include_wifi:
    type: string
    description: Include WiFi diagnostics (disable on non-WiFi devices)
    default: "true"
    enum: ["true", "false"]
  ping_target:
    type: string
    description: Host to ping for connectivity check
    default: 1.1.1.1
  dns_target:
    type: string
    description: Domain to resolve for DNS check
    default: cloudflare.com
---

```sh
#!/bin/sh
# OpenWrt Diagnostics Playbook
# Collects system, network, WiFi, firewall, and service information

VERBOSITY="{{verbosity}}"
INCLUDE_WIFI="{{include_wifi}}"
PING_TARGET="{{ping_target}}"
DNS_TARGET="{{dns_target}}"

sep() { printf '\n══════ %s ══════\n' "$1"; }

# ── System ──────────────────────────────────────────────────────────────────

sep "SYSTEM INFO"
echo "Hostname: $(cat /proc/sys/kernel/hostname)"
echo "Model: $([ -f /tmp/sysinfo/model ] && cat /tmp/sysinfo/model || echo 'unknown')"
echo "Board: $([ -f /tmp/sysinfo/board_name ] && cat /tmp/sysinfo/board_name || echo 'unknown')"
cat /etc/openwrt_release 2>/dev/null || echo "(no /etc/openwrt_release)"
echo ""
uname -a
echo ""
uptime

sep "MEMORY"
free 2>/dev/null || cat /proc/meminfo | head -5
if [ "$VERBOSITY" = "verbose" ]; then
    echo ""
    cat /proc/meminfo
fi

sep "STORAGE"
df -h
if [ "$VERBOSITY" = "verbose" ]; then
    echo ""
    echo "── Mount points ──"
    mount
    echo ""
    echo "── Block devices ──"
    block info 2>/dev/null || echo "(block command not available)"
fi

sep "CPU / LOAD"
echo "Load: $(cat /proc/loadavg)"
if [ "$VERBOSITY" != "brief" ]; then
    echo ""
    echo "── Top processes ──"
    top -b -n1 2>/dev/null | head -20 || ps -w | head -20
fi

# ── Network ─────────────────────────────────────────────────────────────────

sep "NETWORK INTERFACES"
if command -v ip >/dev/null 2>&1; then
    ip -br addr
    if [ "$VERBOSITY" = "verbose" ]; then
        echo ""
        ip addr
    fi
else
    ifconfig
fi

sep "ROUTES"
if [ "$VERBOSITY" = "brief" ]; then
    ip route show default 2>/dev/null || route -n | grep "^0.0.0.0"
else
    ip route 2>/dev/null || route -n
fi

sep "DNS CONFIG"
cat /etc/resolv.conf 2>/dev/null
if [ "$VERBOSITY" != "brief" ]; then
    echo ""
    cat /tmp/resolv.conf.d/resolv.conf.auto 2>/dev/null
fi

sep "CONNECTIVITY"
echo "── Ping $PING_TARGET ──"
ping -c 3 -W 3 "$PING_TARGET" 2>&1 || echo "FAILED: cannot reach $PING_TARGET"
echo ""
echo "── DNS resolve $DNS_TARGET ──"
nslookup "$DNS_TARGET" 2>&1 || echo "FAILED: cannot resolve $DNS_TARGET"

if [ "$VERBOSITY" = "verbose" ]; then
    echo ""
    echo "── Traceroute $PING_TARGET ──"
    traceroute -m 15 -w 2 "$PING_TARGET" 2>&1 || echo "(traceroute not available)"
fi

sep "NEIGHBORS"
ip neigh 2>/dev/null || cat /proc/net/arp

if [ "$VERBOSITY" != "brief" ]; then
    sep "LISTENING PORTS"
    netstat -tlnp 2>/dev/null || ss -tlnp 2>/dev/null || echo "(no netstat/ss)"
fi

# ── WiFi ────────────────────────────────────────────────────────────────────

if [ "$INCLUDE_WIFI" = "true" ]; then
    sep "WIFI STATUS"
    if command -v iwinfo >/dev/null 2>&1; then
        for dev in $(iwinfo | grep "ESSID" | awk '{print $1}'); do
            echo "── $dev ──"
            iwinfo "$dev" info
            if [ "$VERBOSITY" = "verbose" ]; then
                echo ""
                echo "── Associated stations ──"
                iwinfo "$dev" assoclist
            fi
            echo ""
        done
    else
        echo "(iwinfo not available)"
        iw dev 2>/dev/null || echo "(iw not available)"
    fi

    if [ "$VERBOSITY" != "brief" ]; then
        sep "WIFI CONFIG"
        uci show wireless 2>/dev/null | grep -v '\.key=' || echo "(uci not available)"
    fi
fi

# ── Firewall ────────────────────────────────────────────────────────────────

sep "FIREWALL"
if command -v fw4 >/dev/null 2>&1; then
    echo "── fw4 zones ──"
    fw4 zone 2>/dev/null || echo "(fw4 zone failed)"
    if [ "$VERBOSITY" != "brief" ]; then
        echo ""
        echo "── fw4 rules ──"
        fw4 print 2>/dev/null | head -50
    fi
elif command -v fw3 >/dev/null 2>&1; then
    echo "── fw3 zones ──"
    fw3 zone 2>/dev/null
else
    echo "── iptables ──"
    iptables -L -n --line-numbers 2>/dev/null | head -40 || echo "(iptables not available)"
fi

if [ "$VERBOSITY" = "verbose" ]; then
    echo ""
    echo "── NAT rules ──"
    iptables -t nat -L -n 2>/dev/null || nft list table inet fw4 2>/dev/null | head -40
fi

# ── Services ────────────────────────────────────────────────────────────────

sep "SERVICES"
if [ -d /etc/init.d ]; then
    for svc in /etc/init.d/*; do
        name=$(basename "$svc")
        enabled="disabled"
        "$svc" enabled 2>/dev/null && enabled="enabled"
        running="stopped"
        "$svc" running 2>/dev/null && running="running"
        printf "  %-20s %s / %s\n" "$name" "$enabled" "$running"
    done
fi

if [ "$VERBOSITY" = "verbose" ]; then
    sep "UCI NETWORK"
    uci show network 2>/dev/null || echo "(uci not available)"

    sep "DMESG (last 30 lines)"
    dmesg | tail -30

    sep "LOGREAD (last 30 lines)"
    logread 2>/dev/null | tail -30 || echo "(logread not available)"
fi

sep "DONE"
echo "Diagnostics completed at $(date)"
```
