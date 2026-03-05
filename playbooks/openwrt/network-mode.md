---
name: network-mode
description: Configure BPI ethernet port roles — router (stock), switch (L2 bridge), or hybrid (passthrough + routed) with LTE backup
params:
  apn:
    type: string
    description: Carrier APN (e.g. ltemobile.apn, fast.t-mobile.com)
  action:
    type: string
    description: Action to perform
    default: status
    enum: [status, apply, revert]
  mode:
    type: string
    description: Network mode to apply
    default: router
    enum: [router, switch, hybrid]
  wan_port:
    type: string
    description: Uplink port (WAN)
    default: eth4
  bridge_proto:
    type: string
    description: "How BPI gets its management IP on the bridge: dhcp (upstream DHCP), static (fixed IP), none (LTE only)"
    default: dhcp
    enum: [dhcp, static, none]
  bridge_ip:
    type: string
    description: "Static IP/mask for BPI on bridge (e.g. 10.0.0.50/24). Required when bridge_proto=static"
    default: ""
  bridge_gateway:
    type: string
    description: "Default gateway for static bridge (e.g. 10.0.0.1). Required when bridge_proto=static"
    default: ""
  passthrough_ports:
    type: string
    description: "Comma-separated ports bridged with WAN (hybrid only)"
    default: eth0
  routed_ports:
    type: string
    description: "Comma-separated ports where BPI serves DHCP (hybrid only)"
    default: eth1,eth2,eth3,eth5
  routed_subnet:
    type: string
    description: "BPI IP on routed segment (hybrid only)"
    default: 192.168.2.1/24
  lte_backup:
    type: string
    description: Keep wwan0 as fallback route
    default: "yes"
    enum: [yes, no]
  verbosity:
    type: string
    description: Output detail level
    default: normal
    enum: [brief, normal, verbose]
---

```sh
#!/bin/sh
# Network Mode Playbook
# Configures BPI ethernet ports as router (stock), switch (L2 bridge),
# or hybrid (passthrough + routed) with optional LTE backup.
#
# Modes:
#   router — eth4=WAN, eth0-3+eth5=LAN bridge, standard NAT routing
#   switch — all 6 ports in one bridge, pure L2, no DHCP server
#   hybrid — WAN+passthrough=br-wan, routed=br-lan with BPI DHCP server

ACTION="{{action}}"
MODE="{{mode}}"
WAN_PORT="{{wan_port}}"
BRIDGE_PROTO="{{bridge_proto}}"
BRIDGE_IP="{{bridge_ip}}"
BRIDGE_GATEWAY="{{bridge_gateway}}"
PASSTHROUGH_PORTS="{{passthrough_ports}}"
ROUTED_PORTS="{{routed_ports}}"
ROUTED_SUBNET="{{routed_subnet}}"
LTE_BACKUP="{{lte_backup}}"
VERBOSITY="{{verbosity}}"

BACKUP_DIR="/etc/config/backup"

# ── Helpers ────────────────────────────────────────────────────────────────

sep()  { printf '\n══════ %s ══════\n' "$1"; }
ok()   { printf '  ✓ %s\n' "$1"; }
err()  { printf '  ✗ %s\n' "$1"; }
inf()  { printf '  · %s\n' "$1"; }
warn() { printf '  ⚠ %s\n' "$1"; }

port_exists() { [ -d "/sys/class/net/$1" ]; }

port_status() {
    carrier=$(cat /sys/class/net/$1/carrier 2>/dev/null || echo "0")
    speed=$(cat /sys/class/net/$1/speed 2>/dev/null || echo "?")
    if [ "$carrier" = "1" ]; then
        echo "UP (${speed}Mbps)"
    else
        echo "down"
    fi
}

in_list() { echo ",$2," | grep -q ",$1,"; }

# ── Validation ─────────────────────────────────────────────────────────────

validate_params() {
    errors=0

    if ! command -v uci >/dev/null 2>&1; then
        err "uci not available — not an OpenWrt device?"
        return 1
    fi

    if ! port_exists "$WAN_PORT"; then
        err "WAN port $WAN_PORT does not exist"
        errors=$((errors + 1))
    fi

    # LTE gate
    if [ "$LTE_BACKUP" = "yes" ]; then
        wwan_ip=$(ip -4 addr show wwan0 2>/dev/null | grep 'inet ')
        if [ -z "$wwan_ip" ]; then
            err "wwan0 has no IP — LTE backup unavailable"
            err "Aborting: would lose remote access if config misconfigures"
            errors=$((errors + 1))
        fi
    fi

    # bridge_proto checks
    if [ "$BRIDGE_PROTO" = "static" ]; then
        if [ -z "$BRIDGE_IP" ]; then
            err "bridge_proto=static requires bridge_ip (e.g. 10.0.0.50/24)"
            errors=$((errors + 1))
        else
            echo "$BRIDGE_IP" | grep -qE '^[0-9]+[.][0-9]+[.][0-9]+[.][0-9]+/[0-9]+$' || {
                err "Invalid bridge_ip format: $BRIDGE_IP (expected e.g. 10.0.0.50/24)"
                errors=$((errors + 1))
            }
        fi
        if [ -z "$BRIDGE_GATEWAY" ]; then
            err "bridge_proto=static requires bridge_gateway (e.g. 10.0.0.1)"
            errors=$((errors + 1))
        fi
    fi

    if [ "$BRIDGE_PROTO" = "none" ]; then
        warn "bridge_proto=none: BPI only reachable via LTE tunnel"
    fi

    # Unreachable check
    if [ "$MODE" = "switch" ] && [ "$LTE_BACKUP" = "no" ] && [ "$BRIDGE_PROTO" = "none" ]; then
        err "switch + bridge_proto=none + lte_backup=no = BPI completely unreachable"
        errors=$((errors + 1))
    fi

    # Hybrid-specific checks
    if [ "$MODE" = "hybrid" ]; then
        for p in $(echo "$PASSTHROUGH_PORTS" | tr ',' ' '); do
            p=$(echo "$p" | tr -d ' ')
            [ -z "$p" ] && continue
            if ! port_exists "$p"; then
                err "Passthrough port $p does not exist"
                errors=$((errors + 1))
            fi
            if [ "$p" = "$WAN_PORT" ]; then
                err "WAN port $WAN_PORT also listed in passthrough_ports"
                errors=$((errors + 1))
            fi
        done

        for p in $(echo "$ROUTED_PORTS" | tr ',' ' '); do
            p=$(echo "$p" | tr -d ' ')
            [ -z "$p" ] && continue
            if ! port_exists "$p"; then
                err "Routed port $p does not exist"
                errors=$((errors + 1))
            fi
            if [ "$p" = "$WAN_PORT" ]; then
                err "WAN port $WAN_PORT also listed in routed_ports"
                errors=$((errors + 1))
            fi
            if in_list "$p" "$PASSTHROUGH_PORTS"; then
                err "Port $p in both passthrough and routed lists"
                errors=$((errors + 1))
            fi
        done

        echo "$ROUTED_SUBNET" | grep -qE '^[0-9]+[.][0-9]+[.][0-9]+[.][0-9]+/[0-9]+$' || {
            err "Invalid routed_subnet format: $ROUTED_SUBNET"
            errors=$((errors + 1))
        }

        [ -z "$PASSTHROUGH_PORTS" ] && warn "No passthrough ports — basically router mode"
        [ -z "$ROUTED_PORTS" ] && warn "No routed ports — basically switch mode"
    fi

    return $errors
}

# ── Detection & Status ─────────────────────────────────────────────────────

detect_mode() {
    # br-wan exists → hybrid
    if [ -d /sys/class/net/br-wan ]; then
        echo "hybrid"
        return
    fi
    # WAN port inside br-lan → switch
    if [ -d "/sys/class/net/br-lan/brif/$WAN_PORT" ]; then
        echo "switch"
        return
    fi
    # Separate wan interface → router
    if ubus call network.interface.wan status >/dev/null 2>&1; then
        echo "router"
        return
    fi
    echo "unknown"
}

show_status() {
    sep "NETWORK MODE STATUS"

    current=$(detect_mode)
    echo "Current mode: $(echo "$current" | tr a-z A-Z)"

    echo ""
    echo "── Port roles ──"
    for port in eth0 eth1 eth2 eth3 eth4 eth5; do
        if port_exists "$port"; then
            status=$(port_status "$port")
            role="unassigned"
            if [ -d "/sys/class/net/br-lan/brif/$port" ]; then
                role="br-lan"
            elif [ -d "/sys/class/net/br-wan/brif/$port" ]; then
                role="br-wan"
            else
                wan_dev=$(uci get network.wan.device 2>/dev/null)
                [ "$port" = "$wan_dev" ] && role="wan"
            fi
            printf "  %-6s %-16s %s\n" "$port" "$status" "($role)"
        fi
    done

    echo ""
    echo "── Interfaces ──"
    ip -br addr 2>/dev/null

    echo ""
    echo "── DHCP server ──"
    dhcp_ignore=$(uci get dhcp.lan.ignore 2>/dev/null)
    if [ "$dhcp_ignore" = "1" ]; then
        inf "DHCP server disabled on lan"
    else
        dhcp_start=$(uci get dhcp.lan.start 2>/dev/null)
        dhcp_limit=$(uci get dhcp.lan.limit 2>/dev/null)
        ok "DHCP server active on lan (start=$dhcp_start, limit=$dhcp_limit)"
    fi

    echo ""
    echo "── LTE backup ──"
    wwan_ip=$(ip -4 addr show wwan0 2>/dev/null | grep 'inet ' | awk '{print $2}')
    if [ -n "$wwan_ip" ]; then
        ok "wwan0: $wwan_ip"
    else
        err "wwan0: no IP"
    fi

    echo ""
    echo "── Default routes ──"
    ip route show default 2>/dev/null | while read line; do
        inf "$line"
    done

    if [ "$VERBOSITY" != "brief" ]; then
        echo ""
        echo "── Firewall zones ──"
        uci show firewall 2>/dev/null | grep -E '([.]name=|[.]network=|[.]masq=)' | while read line; do
            inf "$line"
        done
    fi

    if [ "$VERBOSITY" = "verbose" ]; then
        echo ""
        echo "── Full network config ──"
        uci show network 2>/dev/null
    fi

    echo ""
    echo "── Backups ──"
    if ls "$BACKUP_DIR"/*.pre-netmode >/dev/null 2>&1; then
        ok "Backups available at $BACKUP_DIR/*.pre-netmode"
        ls -la "$BACKUP_DIR"/*.pre-netmode 2>/dev/null | while read line; do
            inf "$line"
        done
    else
        inf "No network-mode backups found"
    fi

    sep "STATUS COMPLETE"
}

# ── Backup & Restore ──────────────────────────────────────────────────────

do_backup() {
    echo "── Backing up configs ──"
    mkdir -p "$BACKUP_DIR"
    cp /etc/config/network "$BACKUP_DIR/network.pre-netmode"
    cp /etc/config/dhcp "$BACKUP_DIR/dhcp.pre-netmode"
    cp /etc/config/firewall "$BACKUP_DIR/firewall.pre-netmode"
    ok "Backed up to $BACKUP_DIR/*.pre-netmode"
}

do_revert() {
    sep "REVERTING TO PREVIOUS CONFIG"

    if ! ls "$BACKUP_DIR"/*.pre-netmode >/dev/null 2>&1; then
        err "No network-mode backups found in $BACKUP_DIR"
        exit 1
    fi

    echo "── Restoring configs ──"
    cp "$BACKUP_DIR/network.pre-netmode" /etc/config/network
    cp "$BACKUP_DIR/dhcp.pre-netmode" /etc/config/dhcp
    cp "$BACKUP_DIR/firewall.pre-netmode" /etc/config/firewall
    ok "Configs restored from *.pre-netmode"

    restart_services

    echo ""
    echo "── Verification ──"
    current=$(detect_mode)
    ok "Detected mode: $current"

    wwan_check=$(ip -4 addr show wwan0 2>/dev/null | grep 'inet ')
    [ -n "$wwan_check" ] && ok "LTE management path active" || warn "LTE not active"

    sep "REVERT COMPLETE"
}

# ── Config Writers ─────────────────────────────────────────────────────────

write_network_head() {
    # Loopback + globals (preserves DUID/ULA)
    DUID=$(uci get network.globals.dhcp_default_duid 2>/dev/null)
    ULA=$(uci get network.globals.ula_prefix 2>/dev/null)

    cat > /etc/config/network << 'HEAD'

config interface 'loopback'
	option device 'lo'
	option proto 'static'
	list ipaddr '127.0.0.1/8'

config globals 'globals'
HEAD
    [ -n "$DUID" ] && echo "\toption dhcp_default_duid '$DUID'" >> /etc/config/network
    [ -n "$ULA" ] && echo "\toption ula_prefix '$ULA'" >> /etc/config/network
}

append_wwan() {
    cat >> /etc/config/network << 'WWAN'

config interface 'wwan'
	option proto 'qmi'
	option device '/dev/cdc-wdm0'
	option pdptype 'ipv4v6'
	option apn '{{apn}}'
	option auth 'none'
	option metric '20'
WWAN
}

write_dhcp_base() {
    cat > /etc/config/dhcp << 'DHCP'

config dnsmasq
	option domainneeded '1'
	option boguspriv '1'
	option filterwin2k '0'
	option localise_queries '1'
	option rebind_protection '1'
	option rebind_localhost '1'
	option local '/lan/'
	option domain 'lan'
	option expandhosts '1'
	option nonegcache '0'
	option cachesize '1000'
	option authoritative '1'
	option readethers '1'
	option leasefile '/tmp/dhcp.leases'
	option resolvfile '/tmp/resolv.conf.d/resolv.conf.auto'
	option nonwildcard '1'
	option localservice '1'
	option ednspacket_max '1232'
	option filter_aaaa '0'
	option filter_a '0'

config odhcpd 'odhcpd'
	option maindhcp '0'
	option leasefile '/tmp/odhcpd.leases'
	option leasetrigger '/usr/sbin/odhcpd-update'
	option loglevel '4'
	option piodir '/tmp/odhcpd-piodir'
	option hostsdir '/tmp/hosts'
DHCP
}

write_firewall_base() {
    cat > /etc/config/firewall << 'FWBASE'

config defaults
	option syn_flood '1'
	option input 'REJECT'
	option output 'ACCEPT'
	option forward 'REJECT'

FWBASE
}

write_firewall_rules() {
    cat >> /etc/config/firewall << 'FWRULES'
config rule
	option name 'Allow-DHCP-Renew'
	option src 'wan'
	option proto 'udp'
	option dest_port '68'
	option target 'ACCEPT'
	option family 'ipv4'

config rule
	option name 'Allow-Ping'
	option src 'wan'
	option proto 'icmp'
	option icmp_type 'echo-request'
	option family 'ipv4'
	option target 'ACCEPT'

config rule
	option name 'Allow-IGMP'
	option src 'wan'
	option proto 'igmp'
	option family 'ipv4'
	option target 'ACCEPT'

config rule
	option name 'Allow-DHCPv6'
	option src 'wan'
	option proto 'udp'
	option dest_port '546'
	option family 'ipv6'
	option target 'ACCEPT'

config rule
	option name 'Allow-MLD'
	option src 'wan'
	option proto 'icmp'
	option src_ip 'fe80::/10'
	list icmp_type '130/0'
	list icmp_type '131/0'
	list icmp_type '132/0'
	list icmp_type '143/0'
	option family 'ipv6'
	option target 'ACCEPT'

config rule
	option name 'Allow-ICMPv6-Input'
	option src 'wan'
	option proto 'icmp'
	list icmp_type 'echo-request'
	list icmp_type 'echo-reply'
	list icmp_type 'destination-unreachable'
	list icmp_type 'packet-too-big'
	list icmp_type 'time-exceeded'
	list icmp_type 'bad-header'
	list icmp_type 'unknown-header-type'
	list icmp_type 'router-solicitation'
	list icmp_type 'neighbour-solicitation'
	list icmp_type 'router-advertisement'
	list icmp_type 'neighbour-advertisement'
	option limit '1000/sec'
	option family 'ipv6'
	option target 'ACCEPT'

config rule
	option name 'Allow-ICMPv6-Forward'
	option src 'wan'
	option dest '*'
	option proto 'icmp'
	list icmp_type 'echo-request'
	list icmp_type 'echo-reply'
	list icmp_type 'destination-unreachable'
	list icmp_type 'packet-too-big'
	list icmp_type 'time-exceeded'
	list icmp_type 'bad-header'
	list icmp_type 'unknown-header-type'
	option limit '1000/sec'
	option family 'ipv6'
	option target 'ACCEPT'

config rule
	option name 'Allow-IPSec-ESP'
	option src 'wan'
	option dest 'lan'
	option proto 'esp'
	option target 'ACCEPT'

config rule
	option name 'Allow-ISAKMP'
	option src 'wan'
	option dest 'lan'
	option dest_port '500'
	option proto 'udp'
	option target 'ACCEPT'

config rule
	option name 'Allow-SSH-WAN'
	option src 'wan'
	option proto 'tcp'
	option dest_port '22'
	option target 'ACCEPT'

config rule
	option name 'Allow-SCTL-WAN'
	option src 'wan'
	option proto 'tcp'
	option dest_port '1337'
	option target 'ACCEPT'
FWRULES
}

restart_services() {
    echo ""
    echo "── Restarting services ──"
    /etc/init.d/network restart
    sleep 3
    /etc/init.d/dnsmasq restart
    /etc/init.d/firewall restart
    sleep 1
    ok "Services restarted"
}

# ── Verify Functions ───────────────────────────────────────────────────────

verify_router() {
    echo ""
    echo "── Verification ──"

    if ubus call network.interface.wan status >/dev/null 2>&1; then
        wan_ip=$(ubus call network.interface.wan status 2>/dev/null | grep -o '"address":"[^"]*"' | head -1 | cut -d'"' -f4)
        ok "WAN interface up: ${wan_ip:-waiting for DHCP}"
    else
        err "WAN interface not found"
    fi

    br_ports=$(ls /sys/class/net/br-lan/brif/ 2>/dev/null | tr '\n' ' ')
    [ -n "$br_ports" ] && ok "br-lan ports: $br_ports" || err "br-lan has no ports"

    if [ -d "/sys/class/net/br-lan/brif/$WAN_PORT" ]; then
        err "$WAN_PORT still in br-lan"
    else
        ok "$WAN_PORT separate from bridge (WAN)"
    fi

    pidof dnsmasq >/dev/null 2>&1 && ok "dnsmasq running" || err "dnsmasq not running"

    wwan_check=$(ip -4 addr show wwan0 2>/dev/null | grep 'inet ')
    [ -n "$wwan_check" ] && ok "LTE active" || warn "LTE not active"

    sep "ROUTER MODE ACTIVE"
}

verify_switch() {
    echo ""
    echo "── Verification ──"

    br_ports=$(ls /sys/class/net/br-lan/brif/ 2>/dev/null | tr '\n' ' ')
    ok "br-lan ports: $br_ports"

    if [ -d "/sys/class/net/br-lan/brif/$WAN_PORT" ]; then
        ok "$WAN_PORT in bridge"
    else
        err "$WAN_PORT NOT in bridge"
    fi

    br_ip=$(ip -4 addr show br-lan 2>/dev/null | grep 'inet ' | awk '{print $2}')
    if [ -n "$br_ip" ]; then
        ok "Bridge IP: $br_ip"
    elif [ "$BRIDGE_PROTO" = "none" ]; then
        ok "No bridge IP (as configured)"
    else
        inf "Bridge IP: waiting for DHCP"
    fi

    if ! ubus call network.interface.wan status >/dev/null 2>&1; then
        ok "No separate WAN interface"
    else
        err "WAN interface still exists"
    fi

    dhcp_ignore=$(uci get dhcp.lan.ignore 2>/dev/null)
    [ "$dhcp_ignore" = "1" ] && ok "DHCP server disabled" || err "DHCP server still active"

    if [ "$LTE_BACKUP" = "yes" ]; then
        wwan_check=$(ip -4 addr show wwan0 2>/dev/null | grep 'inet ')
        [ -n "$wwan_check" ] && ok "LTE active" || warn "LTE not active"
    fi

    sep "SWITCH MODE ACTIVE"
    echo ""
    echo "All ports bridged — pure L2 forwarding."
    echo "BPI management: $BRIDGE_PROTO on bridge + LTE tunnel"
}

verify_hybrid() {
    echo ""
    echo "── Verification ──"

    if [ -d /sys/class/net/br-wan ]; then
        brwan_ports=$(ls /sys/class/net/br-wan/brif/ 2>/dev/null | tr '\n' ' ')
        ok "br-wan ports: $brwan_ports"
    else
        err "br-wan not found"
    fi

    if [ -d /sys/class/net/br-lan ]; then
        brlan_ports=$(ls /sys/class/net/br-lan/brif/ 2>/dev/null | tr '\n' ' ')
        ok "br-lan ports: $brlan_ports"
    else
        err "br-lan not found"
    fi

    wan_dev=$(uci get network.wan.device 2>/dev/null)
    [ "$wan_dev" = "br-wan" ] && ok "WAN on br-wan ($BRIDGE_PROTO)" || err "WAN device is $wan_dev, expected br-wan"

    lan_ip=$(ip -4 addr show br-lan 2>/dev/null | grep 'inet ' | awk '{print $2}')
    [ -n "$lan_ip" ] && ok "br-lan IP: $lan_ip" || err "br-lan has no IP"

    pidof dnsmasq >/dev/null 2>&1 && ok "dnsmasq running (DHCP on routed ports)" || err "dnsmasq not running"

    if [ "$LTE_BACKUP" = "yes" ]; then
        wwan_check=$(ip -4 addr show wwan0 2>/dev/null | grep 'inet ')
        [ -n "$wwan_check" ] && ok "LTE active" || warn "LTE not active"
    fi

    sep "HYBRID MODE ACTIVE"
    echo ""
    echo "Passthrough: $WAN_PORT,$PASSTHROUGH_PORTS → br-wan (upstream DHCP to devices)"
    echo "Routed: $ROUTED_PORTS → br-lan ($ROUTED_SUBNET, BPI DHCP server)"
}

# ── Apply: Router ──────────────────────────────────────────────────────────

apply_router() {
    sep "APPLYING ROUTER MODE"
    do_backup

    # ── Network ──
    echo ""
    echo "── Writing network config ──"
    write_network_head

    # br-lan: all ports except wan_port
    printf '\nconfig device\n' >> /etc/config/network
    echo "\toption name 'br-lan'" >> /etc/config/network
    echo "\toption type 'bridge'" >> /etc/config/network
    for port in eth0 eth1 eth2 eth3 eth4 eth5; do
        [ "$port" = "$WAN_PORT" ] && continue
        echo "\tlist ports '$port'" >> /etc/config/network
    done

    cat >> /etc/config/network << EOF

config interface 'lan'
	option device 'br-lan'
	option proto 'static'
	list ipaddr '192.168.1.1/24'
	option ip6assign '60'

config interface 'wan'
	option device '$WAN_PORT'
	option proto 'dhcp'
	option metric '10'

config interface 'wan6'
	option device '$WAN_PORT'
	option proto 'dhcpv6'
EOF

    [ "$LTE_BACKUP" = "yes" ] && append_wwan
    ok "Network config written"

    # ── DHCP ──
    echo ""
    echo "── Writing DHCP config ──"
    write_dhcp_base
    cat >> /etc/config/dhcp << 'EOF'

config dhcp 'lan'
	option interface 'lan'
	option start '100'
	option limit '150'
	option leasetime '12h'
	option dhcpv4 'server'
	option dhcpv6 'server'
	option ra 'server'
	option ra_slaac '1'
	list ra_flags 'managed-config'
	list ra_flags 'other-config'

config dhcp 'wan'
	option interface 'wan'
	option ignore '1'
EOF
    ok "DHCP config written"

    # ── Firewall ──
    echo ""
    echo "── Writing firewall config ──"
    write_firewall_base

    cat >> /etc/config/firewall << 'EOF'
config zone
	option name 'lan'
	list network 'lan'
	option input 'ACCEPT'
	option output 'ACCEPT'
	option forward 'ACCEPT'

config zone
	option name 'wan'
	list network 'wan'
	list network 'wan6'
	list network 'wwan'
	option input 'REJECT'
	option output 'ACCEPT'
	option forward 'DROP'
	option masq '1'
	option mtu_fix '1'

config forwarding
	option src 'lan'
	option dest 'wan'

EOF
    write_firewall_rules
    ok "Firewall config written"

    restart_services
    verify_router
}

# ── Apply: Switch ──────────────────────────────────────────────────────────

apply_switch() {
    sep "APPLYING SWITCH MODE"
    [ "$BRIDGE_PROTO" = "dhcp" ] && warn "BPI management IP will change to DHCP-assigned address"
    [ "$BRIDGE_PROTO" = "none" ] && warn "BPI will only be reachable via LTE tunnel"
    do_backup

    # ── Network ──
    echo ""
    echo "── Writing network config ──"
    write_network_head

    # br-lan: all 6 ports
    printf '\nconfig device\n' >> /etc/config/network
    echo "\toption name 'br-lan'" >> /etc/config/network
    echo "\toption type 'bridge'" >> /etc/config/network
    for port in eth0 eth1 eth2 eth3 eth4 eth5; do
        echo "\tlist ports '$port'" >> /etc/config/network
    done

    printf '\nconfig interface '\''lan'\''\n' >> /etc/config/network
    echo "\toption device 'br-lan'" >> /etc/config/network
    echo "\toption proto '$BRIDGE_PROTO'" >> /etc/config/network

    case "$BRIDGE_PROTO" in
        dhcp)   echo "\toption metric '10'" >> /etc/config/network ;;
        static)
            echo "\tlist ipaddr '$BRIDGE_IP'" >> /etc/config/network
            echo "\toption gateway '$BRIDGE_GATEWAY'" >> /etc/config/network
            ;;
    esac

    [ "$LTE_BACKUP" = "yes" ] && append_wwan
    ok "Network config written (all ports bridged, proto=$BRIDGE_PROTO)"

    # ── DHCP ──
    echo ""
    echo "── Writing DHCP config ──"
    write_dhcp_base
    cat >> /etc/config/dhcp << 'EOF'

config dhcp 'lan'
	option interface 'lan'
	option ignore '1'
EOF
    ok "DHCP config written (server disabled)"

    # ── Firewall ──
    echo ""
    echo "── Writing firewall config ──"
    write_firewall_base

    cat >> /etc/config/firewall << 'EOF'
config zone
	option name 'lan'
	list network 'lan'
	option input 'ACCEPT'
	option output 'ACCEPT'
	option forward 'ACCEPT'

EOF

    if [ "$LTE_BACKUP" = "yes" ]; then
        cat >> /etc/config/firewall << 'EOF'
config zone
	option name 'wan'
	list network 'wwan'
	option input 'REJECT'
	option output 'ACCEPT'
	option forward 'DROP'
	option masq '1'
	option mtu_fix '1'

config forwarding
	option src 'lan'
	option dest 'wan'

EOF
    fi

    write_firewall_rules
    ok "Firewall config written"

    restart_services
    verify_switch
}

# ── Apply: Hybrid ──────────────────────────────────────────────────────────

apply_hybrid() {
    sep "APPLYING HYBRID MODE"
    inf "WAN + passthrough: $WAN_PORT,$PASSTHROUGH_PORTS → br-wan ($BRIDGE_PROTO)"
    inf "Routed: $ROUTED_PORTS → br-lan ($ROUTED_SUBNET, DHCP server)"
    [ "$LTE_BACKUP" = "yes" ] && inf "LTE: wwan0 (metric 20, backup)"
    do_backup

    # ── Network ──
    echo ""
    echo "── Writing network config ──"
    write_network_head

    # br-wan: wan_port + passthrough_ports
    printf '\nconfig device\n' >> /etc/config/network
    echo "\toption name 'br-wan'" >> /etc/config/network
    echo "\toption type 'bridge'" >> /etc/config/network
    echo "\tlist ports '$WAN_PORT'" >> /etc/config/network
    for port in $(echo "$PASSTHROUGH_PORTS" | tr ',' ' '); do
        port=$(echo "$port" | tr -d ' ')
        [ -n "$port" ] && echo "\tlist ports '$port'" >> /etc/config/network
    done

    # br-lan: routed_ports
    printf '\nconfig device\n' >> /etc/config/network
    echo "\toption name 'br-lan'" >> /etc/config/network
    echo "\toption type 'bridge'" >> /etc/config/network
    for port in $(echo "$ROUTED_PORTS" | tr ',' ' '); do
        port=$(echo "$port" | tr -d ' ')
        [ -n "$port" ] && echo "\tlist ports '$port'" >> /etc/config/network
    done

    # wan interface on br-wan
    printf '\nconfig interface '\''wan'\''\n' >> /etc/config/network
    echo "\toption device 'br-wan'" >> /etc/config/network
    echo "\toption proto '$BRIDGE_PROTO'" >> /etc/config/network

    case "$BRIDGE_PROTO" in
        dhcp)   echo "\toption metric '10'" >> /etc/config/network ;;
        static)
            echo "\tlist ipaddr '$BRIDGE_IP'" >> /etc/config/network
            echo "\toption gateway '$BRIDGE_GATEWAY'" >> /etc/config/network
            ;;
    esac

    # lan interface on br-lan
    cat >> /etc/config/network << EOF

config interface 'lan'
	option device 'br-lan'
	option proto 'static'
	list ipaddr '$ROUTED_SUBNET'
EOF

    [ "$LTE_BACKUP" = "yes" ] && append_wwan
    ok "Network config written (br-wan + br-lan)"

    # ── DHCP ──
    echo ""
    echo "── Writing DHCP config ──"
    write_dhcp_base
    cat >> /etc/config/dhcp << 'EOF'

config dhcp 'lan'
	option interface 'lan'
	option start '100'
	option limit '150'
	option leasetime '12h'
	option dhcpv4 'server'

config dhcp 'wan'
	option interface 'wan'
	option ignore '1'
EOF
    ok "DHCP config written (server on lan/routed)"

    # ── Firewall ──
    echo ""
    echo "── Writing firewall config ──"
    write_firewall_base

    cat >> /etc/config/firewall << 'EOF'
config zone
	option name 'lan'
	list network 'lan'
	option input 'ACCEPT'
	option output 'ACCEPT'
	option forward 'ACCEPT'

config zone
	option name 'wan'
	list network 'wan'
EOF
    [ "$LTE_BACKUP" = "yes" ] && echo "\tlist network 'wwan'" >> /etc/config/firewall
    cat >> /etc/config/firewall << 'EOF'
	option input 'REJECT'
	option output 'ACCEPT'
	option forward 'DROP'
	option masq '1'
	option mtu_fix '1'

config forwarding
	option src 'lan'
	option dest 'wan'

EOF
    write_firewall_rules
    ok "Firewall config written"

    restart_services
    verify_hybrid
}

# ── Main ───────────────────────────────────────────────────────────────────

case "$ACTION" in
    status)
        show_status
        ;;
    apply)
        echo "── Validating parameters ──"
        if ! validate_params; then
            err "Validation failed — aborting"
            exit 1
        fi
        ok "Validation passed"

        current=$(detect_mode)
        if [ "$current" = "$MODE" ]; then
            warn "Device appears to already be in $MODE mode"
            inf "Proceeding anyway (will refresh config)"
        fi

        case "$MODE" in
            router)  apply_router ;;
            switch)  apply_switch ;;
            hybrid)  apply_hybrid ;;
            *)       err "Unknown mode: $MODE"; exit 1 ;;
        esac
        ;;
    revert)
        do_revert
        ;;
    *)
        err "Unknown action: $ACTION"
        echo "Valid actions: status, apply, revert"
        exit 1
        ;;
esac
```
