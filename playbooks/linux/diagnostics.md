---
name: linux-diagnostics
description: Comprehensive diagnostics for standard Linux systems — system health, network, storage, services, and security
params:
  verbosity:
    type: string
    description: Output detail level
    default: normal
    enum: [brief, normal, verbose]
  ping_target:
    type: string
    description: Host to ping for connectivity check
    default: 1.1.1.1
  dns_target:
    type: string
    description: Domain to resolve for DNS check
    default: cloudflare.com
  check_docker:
    type: string
    description: Include Docker/container diagnostics if present
    default: "true"
    enum: ["true", "false"]
---

```sh
#!/bin/bash
# Linux Diagnostics Playbook
# Compatible with Ubuntu, Debian, RHEL, Fedora, Arch, Alpine, etc.

VERBOSITY="{{verbosity}}"
PING_TARGET="{{ping_target}}"
DNS_TARGET="{{dns_target}}"
CHECK_DOCKER="{{check_docker}}"

sep() { printf '\n══════ %s ══════\n' "$1"; }

# ── System ──────────────────────────────────────────────────────────────────

sep "SYSTEM INFO"
echo "Hostname: $(hostname -f 2>/dev/null || hostname)"
uname -a
echo ""
if [ -f /etc/os-release ]; then
    . /etc/os-release
    echo "OS: $PRETTY_NAME"
    echo "ID: $ID (family: ${ID_LIKE:-$ID})"
elif [ -f /etc/redhat-release ]; then
    cat /etc/redhat-release
fi
echo ""
uptime

sep "MEMORY"
free -h 2>/dev/null || free
if [ "$VERBOSITY" = "verbose" ]; then
    echo ""
    echo "── Swap ──"
    swapon --show 2>/dev/null || cat /proc/swaps
fi

sep "CPU"
echo "Cores: $(nproc 2>/dev/null || grep -c ^processor /proc/cpuinfo)"
echo "Model: $(grep 'model name' /proc/cpuinfo 2>/dev/null | head -1 | cut -d: -f2 | xargs)"
echo "Load:  $(cat /proc/loadavg)"
if [ "$VERBOSITY" != "brief" ]; then
    echo ""
    echo "── Top processes by CPU ──"
    ps aux --sort=-%cpu 2>/dev/null | head -11 || ps aux | head -11
fi

sep "STORAGE"
df -hT 2>/dev/null || df -h
if [ "$VERBOSITY" = "verbose" ]; then
    echo ""
    echo "── Block devices ──"
    lsblk 2>/dev/null || echo "(lsblk not available)"
    echo ""
    echo "── Mount points ──"
    mount | grep -v "^cgroup\|^proc\|^sys\|^tmpfs\|^devpts"
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
    ifconfig -a 2>/dev/null
fi

sep "ROUTES"
if [ "$VERBOSITY" = "brief" ]; then
    ip route show default 2>/dev/null || route -n | grep "^0.0.0.0"
else
    ip route 2>/dev/null || route -n
fi

sep "DNS CONFIG"
if command -v resolvectl >/dev/null 2>&1; then
    resolvectl status 2>/dev/null | head -20
else
    cat /etc/resolv.conf 2>/dev/null
fi

sep "CONNECTIVITY"
echo "── Ping $PING_TARGET ──"
ping -c 3 -W 3 "$PING_TARGET" 2>&1 || echo "FAILED: cannot reach $PING_TARGET"
echo ""
echo "── DNS resolve $DNS_TARGET ──"
if command -v dig >/dev/null 2>&1; then
    dig +short "$DNS_TARGET" 2>&1
elif command -v nslookup >/dev/null 2>&1; then
    nslookup "$DNS_TARGET" 2>&1
elif command -v host >/dev/null 2>&1; then
    host "$DNS_TARGET" 2>&1
else
    getent hosts "$DNS_TARGET" 2>&1 || echo "No DNS tools available"
fi

if [ "$VERBOSITY" = "verbose" ]; then
    echo ""
    echo "── Traceroute $PING_TARGET ──"
    traceroute -m 15 -w 2 "$PING_TARGET" 2>&1 || tracepath "$PING_TARGET" 2>&1 || echo "(traceroute not available)"
fi

if [ "$VERBOSITY" != "brief" ]; then
    sep "LISTENING PORTS"
    ss -tlnp 2>/dev/null || netstat -tlnp 2>/dev/null || echo "(no ss/netstat)"
fi

# ── Firewall ────────────────────────────────────────────────────────────────

sep "FIREWALL"
if command -v ufw >/dev/null 2>&1; then
    echo "── UFW status ──"
    ufw status verbose 2>/dev/null || echo "(ufw requires root)"
elif command -v firewall-cmd >/dev/null 2>&1; then
    echo "── firewalld zones ──"
    firewall-cmd --list-all 2>/dev/null || echo "(firewalld requires root)"
elif command -v nft >/dev/null 2>&1; then
    echo "── nftables ──"
    nft list ruleset 2>/dev/null | head -30 || echo "(nft requires root)"
elif command -v iptables >/dev/null 2>&1; then
    echo "── iptables ──"
    iptables -L -n --line-numbers 2>/dev/null | head -40 || echo "(iptables requires root)"
else
    echo "(no firewall tool found)"
fi

# ── Services ────────────────────────────────────────────────────────────────

sep "SERVICES"
if command -v systemctl >/dev/null 2>&1; then
    echo "── Failed units ──"
    systemctl --failed --no-pager 2>/dev/null
    if [ "$VERBOSITY" != "brief" ]; then
        echo ""
        echo "── Running services ──"
        systemctl list-units --type=service --state=running --no-pager 2>/dev/null | head -30
    fi
elif command -v rc-status >/dev/null 2>&1; then
    echo "── OpenRC services ──"
    rc-status 2>/dev/null
elif [ -d /etc/init.d ]; then
    echo "── SysV init ──"
    ls /etc/init.d/
fi

if [ "$VERBOSITY" = "verbose" ]; then
    echo ""
    echo "── Recent boot log ──"
    journalctl -b --no-pager -p warning 2>/dev/null | tail -20 || dmesg | tail -20
fi

# ── Docker / Containers ────────────────────────────────────────────────────

if [ "$CHECK_DOCKER" = "true" ]; then
    if command -v docker >/dev/null 2>&1; then
        sep "DOCKER"
        echo "── Version ──"
        docker version --format '{{.Server.Version}}' 2>/dev/null || echo "(docker daemon not reachable)"
        echo ""
        echo "── Running containers ──"
        docker ps --format "table {{.Names}}\t{{.Image}}\t{{.Status}}\t{{.Ports}}" 2>/dev/null || echo "(requires docker access)"
        if [ "$VERBOSITY" = "verbose" ]; then
            echo ""
            echo "── All containers ──"
            docker ps -a --format "table {{.Names}}\t{{.Image}}\t{{.Status}}" 2>/dev/null
            echo ""
            echo "── Docker disk usage ──"
            docker system df 2>/dev/null
        fi
    fi

    if command -v podman >/dev/null 2>&1; then
        sep "PODMAN"
        echo "── Running containers ──"
        podman ps --format "table {{.Names}}\t{{.Image}}\t{{.Status}}" 2>/dev/null
    fi
fi

# ── Security Quick Check ───────────────────────────────────────────────────

if [ "$VERBOSITY" != "brief" ]; then
    sep "SECURITY"
    echo "── Users with shell access ──"
    grep -v '/nologin\|/false\|/sync\|/halt\|/shutdown' /etc/passwd 2>/dev/null | cut -d: -f1,7

    echo ""
    echo "── Recent auth failures (last 10) ──"
    if [ -f /var/log/auth.log ]; then
        grep -i "failed\|invalid" /var/log/auth.log 2>/dev/null | tail -10
    elif [ -f /var/log/secure ]; then
        grep -i "failed\|invalid" /var/log/secure 2>/dev/null | tail -10
    else
        journalctl _COMM=sshd --no-pager 2>/dev/null | grep -i "failed\|invalid" | tail -10
    fi

    if [ "$VERBOSITY" = "verbose" ]; then
        echo ""
        echo "── SSH config highlights ──"
        grep -E "^(PermitRoot|PasswordAuth|PubkeyAuth|Port |ListenAddr)" /etc/ssh/sshd_config 2>/dev/null || echo "(no sshd_config)"

        echo ""
        echo "── Pending security updates ──"
        if command -v apt >/dev/null 2>&1; then
            apt list --upgradable 2>/dev/null | grep -i secur | head -10 || echo "(none or apt not available)"
        elif command -v dnf >/dev/null 2>&1; then
            dnf check-update --security 2>/dev/null | head -10
        elif command -v yum >/dev/null 2>&1; then
            yum check-update --security 2>/dev/null | head -10
        fi
    fi
fi

sep "DONE"
echo "Diagnostics completed at $(date)"
```
