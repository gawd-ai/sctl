---
name: linux-health-check
description: System health monitoring for Linux — disk alerts, CPU/memory hogs, zombie processes, failed services, log rotation, NTP sync
params:
  disk_threshold:
    type: string
    description: Disk usage percentage threshold for alerts
    default: "90"
  verbosity:
    type: string
    description: Output detail level
    default: normal
    enum: [brief, normal, verbose]
---

```sh
#!/bin/bash
# Linux Health Check Playbook
# Compatible with Ubuntu, Debian, RHEL, Fedora, Arch, Alpine, etc.

DISK_THRESHOLD="{{disk_threshold}}"
VERBOSITY="{{verbosity}}"

sep() { printf '\n══════ %s ══════\n' "$1"; }

WARNINGS=0
CRITICALS=0

warn() { WARNINGS=$((WARNINGS + 1)); echo "  WARNING: $1"; }
crit() { CRITICALS=$((CRITICALS + 1)); echo "  CRITICAL: $1"; }

# ── Disk Space ─────────────────────────────────────────────────────────────

sep "DISK SPACE"
echo "Threshold: ${DISK_THRESHOLD}%"
echo ""
echo "── Partition usage ──"
df -hT 2>/dev/null || df -h
echo ""

# Check partitions against threshold
DISK_ALERT=0
while read -r fs type size used avail pct mount; do
    usage="${pct%\%}"
    if [ "$usage" -ge "$DISK_THRESHOLD" ] 2>/dev/null; then
        crit "Disk $mount is at ${pct} (threshold: ${DISK_THRESHOLD}%)"
        DISK_ALERT=1
    fi
done <<EOF
$(df -hT 2>/dev/null | tail -n +2 | grep -v "^tmpfs\|^devtmpfs\|^overlay$" || df -h | tail -n +2 | grep -v "^tmpfs\|^devtmpfs")
EOF

if [ "$DISK_ALERT" -eq 0 ]; then
    echo "  OK: All partitions below ${DISK_THRESHOLD}%"
fi

echo ""
echo "── Inode usage ──"
df -hi 2>/dev/null | head -20 || echo "(inode info not available)"

INODE_ALERT=0
while read -r fs inodes iused ifree ipct mount; do
    iusage="${ipct%\%}"
    if [ "$iusage" -ge "$DISK_THRESHOLD" ] 2>/dev/null; then
        crit "Inodes on $mount at ${ipct} (threshold: ${DISK_THRESHOLD}%)"
        INODE_ALERT=1
    fi
done <<EOF
$(df -hi 2>/dev/null | tail -n +2 | grep -v "^tmpfs\|^devtmpfs")
EOF

if [ "$INODE_ALERT" -eq 0 ]; then
    echo "  OK: All inode usage below ${DISK_THRESHOLD}%"
fi

# ── CPU / Memory Hogs ─────────────────────────────────────────────────────

sep "HIGH CPU PROCESSES"
echo "── Top 10 by CPU ──"
ps aux --sort=-%cpu 2>/dev/null | head -11 || ps aux | head -11

if [ "$VERBOSITY" != "brief" ]; then
    echo ""
    echo "── Top 10 by memory ──"
    ps aux --sort=-%mem 2>/dev/null | head -11 || ps aux | head -11
fi

echo ""
echo "Load average: $(cat /proc/loadavg)"
NPROC=$(nproc 2>/dev/null || grep -c ^processor /proc/cpuinfo)
LOAD1=$(cat /proc/loadavg | awk '{print $1}')
# Compare load to CPU count (integer comparison)
LOAD_INT=$(echo "$LOAD1" | cut -d. -f1)
if [ "${LOAD_INT:-0}" -ge "$NPROC" ] 2>/dev/null; then
    warn "Load average ($LOAD1) >= CPU count ($NPROC)"
else
    echo "  OK: Load average ($LOAD1) within bounds (${NPROC} CPUs)"
fi

# ── Memory pressure ──
MEM_TOTAL=$(grep MemTotal /proc/meminfo | awk '{print $2}')
MEM_AVAIL=$(grep MemAvailable /proc/meminfo | awk '{print $2}')
if [ -n "$MEM_TOTAL" ] && [ -n "$MEM_AVAIL" ] && [ "$MEM_TOTAL" -gt 0 ] 2>/dev/null; then
    MEM_USED=$((MEM_TOTAL - MEM_AVAIL))
    MEM_PCT=$((MEM_USED * 100 / MEM_TOTAL))
    if [ "$MEM_PCT" -ge 95 ]; then
        crit "Memory usage at ${MEM_PCT}%"
    elif [ "$MEM_PCT" -ge 85 ]; then
        warn "Memory usage at ${MEM_PCT}%"
    else
        echo "  OK: Memory usage at ${MEM_PCT}%"
    fi
fi

# ── Zombie Processes ──────────────────────────────────────────────────────

sep "ZOMBIE PROCESSES"
ZOMBIES=$(ps aux 2>/dev/null | awk '$8 ~ /^Z/ {print}')
ZOMBIE_COUNT=$(echo "$ZOMBIES" | grep -c . 2>/dev/null || echo 0)
if [ -z "$ZOMBIES" ]; then
    ZOMBIE_COUNT=0
fi

if [ "$ZOMBIE_COUNT" -gt 0 ]; then
    warn "$ZOMBIE_COUNT zombie process(es) found"
    echo "$ZOMBIES"
    echo ""
    echo "── Parent PIDs of zombies ──"
    ps aux | awk '$8 ~ /^Z/ {print $2}' | while read -r zpid; do
        PPID_INFO=$(ps -o ppid=,comm= -p "$zpid" 2>/dev/null)
        echo "  Zombie PID $zpid -> Parent: $PPID_INFO"
    done
else
    echo "  OK: No zombie processes"
fi

# ── Failed Services ───────────────────────────────────────────────────────

sep "SYSTEMD FAILED UNITS"
if command -v systemctl >/dev/null 2>&1; then
    FAILED=$(systemctl --failed --no-pager --no-legend 2>/dev/null)
    FAILED_COUNT=$(echo "$FAILED" | grep -c . 2>/dev/null || echo 0)
    if [ -z "$FAILED" ]; then
        FAILED_COUNT=0
    fi

    if [ "$FAILED_COUNT" -gt 0 ]; then
        warn "$FAILED_COUNT failed unit(s)"
        systemctl --failed --no-pager 2>/dev/null
    else
        echo "  OK: No failed units"
    fi

    if [ "$VERBOSITY" = "verbose" ]; then
        echo ""
        echo "── Recently failed (journalctl) ──"
        journalctl -p err --since "24 hours ago" --no-pager 2>/dev/null | tail -20 || echo "(journalctl not available)"
    fi
elif command -v rc-status >/dev/null 2>&1; then
    echo "── OpenRC crashed services ──"
    rc-status --crashed 2>/dev/null || rc-status 2>/dev/null
else
    echo "(no systemd or OpenRC found)"
fi

# ── Log Rotation ──────────────────────────────────────────────────────────

sep "LOG ROTATION"
if [ -f /etc/logrotate.conf ]; then
    echo "  OK: /etc/logrotate.conf exists"
    if [ "$VERBOSITY" = "verbose" ]; then
        echo ""
        echo "── logrotate.conf ──"
        cat /etc/logrotate.conf
        echo ""
        echo "── logrotate.d/ entries ──"
        ls /etc/logrotate.d/ 2>/dev/null || echo "(no logrotate.d)"
    fi
else
    warn "/etc/logrotate.conf not found"
fi

echo ""
echo "── Recent logrotate runs ──"
if [ -f /var/lib/logrotate/status ] || [ -f /var/lib/logrotate.status ]; then
    STATUS_FILE="/var/lib/logrotate/status"
    [ -f "$STATUS_FILE" ] || STATUS_FILE="/var/lib/logrotate.status"
    LAST_RUN=$(stat -c %Y "$STATUS_FILE" 2>/dev/null)
    NOW=$(date +%s)
    if [ -n "$LAST_RUN" ]; then
        AGE_HOURS=$(( (NOW - LAST_RUN) / 3600 ))
        if [ "$AGE_HOURS" -gt 48 ]; then
            warn "Logrotate last ran ${AGE_HOURS} hours ago"
        else
            echo "  OK: Logrotate last ran ${AGE_HOURS} hours ago"
        fi
    fi
    if [ "$VERBOSITY" != "brief" ]; then
        echo ""
        head -10 "$STATUS_FILE"
    fi
elif command -v journalctl >/dev/null 2>&1; then
    journalctl -u logrotate --no-pager --since "7 days ago" 2>/dev/null | tail -5 || echo "(no logrotate journal entries)"
else
    echo "(cannot determine logrotate status)"
fi

# Check for large log files
if [ "$VERBOSITY" != "brief" ]; then
    echo ""
    echo "── Large log files (>100M) ──"
    find /var/log -type f -size +100M -exec ls -lh {} \; 2>/dev/null || echo "  None found"
fi

# ── NTP Sync ──────────────────────────────────────────────────────────────

sep "NTP SYNC"
if command -v timedatectl >/dev/null 2>&1; then
    SYNC_STATUS=$(timedatectl 2>/dev/null)
    echo "$SYNC_STATUS"
    if echo "$SYNC_STATUS" | grep -qi "synchronized: yes\|NTP synchronized: yes\|System clock synchronized: yes"; then
        echo "  OK: Clock is NTP synchronized"
    elif echo "$SYNC_STATUS" | grep -qi "synchronized: no\|NTP synchronized: no\|System clock synchronized: no"; then
        warn "Clock is NOT NTP synchronized"
    fi
elif command -v chronyc >/dev/null 2>&1; then
    echo "── chrony tracking ──"
    chronyc tracking 2>/dev/null || echo "(chronyc failed)"
    STRATUM=$(chronyc tracking 2>/dev/null | grep "Stratum" | awk '{print $3}')
    if [ -n "$STRATUM" ] && [ "$STRATUM" -le 15 ] 2>/dev/null; then
        echo "  OK: chrony synchronized (stratum $STRATUM)"
    else
        warn "chrony not synchronized"
    fi
elif command -v ntpq >/dev/null 2>&1; then
    echo "── ntpq peers ──"
    ntpq -pn 2>/dev/null || echo "(ntpq failed)"
else
    echo "(no NTP tool found: timedatectl, chronyc, ntpq)"
    warn "Cannot verify NTP synchronization"
fi

# ── Pending Reboot ────────────────────────────────────────────────────────

sep "PENDING REBOOT"
REBOOT_NEEDED=0

# Debian/Ubuntu
if [ -f /var/run/reboot-required ]; then
    warn "Reboot required ($(cat /var/run/reboot-required.pkgs 2>/dev/null | head -5 || echo 'see /var/run/reboot-required'))"
    REBOOT_NEEDED=1
fi

# RHEL/Fedora
if command -v needs-restarting >/dev/null 2>&1; then
    needs-restarting -r >/dev/null 2>&1
    if [ $? -eq 1 ]; then
        warn "Reboot required (needs-restarting)"
        REBOOT_NEEDED=1
    fi
fi

# Generic: check if running kernel != installed kernel
RUNNING_KERNEL=$(uname -r)
if [ -d /lib/modules ]; then
    LATEST_KERNEL=$(ls -t /lib/modules/ 2>/dev/null | head -1)
    if [ -n "$LATEST_KERNEL" ] && [ "$RUNNING_KERNEL" != "$LATEST_KERNEL" ]; then
        warn "Running kernel ($RUNNING_KERNEL) differs from installed ($LATEST_KERNEL)"
        REBOOT_NEEDED=1
    fi
fi

if [ "$REBOOT_NEEDED" -eq 0 ]; then
    echo "  OK: No reboot pending"
fi

echo ""
echo "Uptime: $(uptime -p 2>/dev/null || uptime)"

# ── Overall Health Summary ────────────────────────────────────────────────

sep "HEALTH SUMMARY"
echo "Checks completed at $(date)"
echo ""
echo "  Warnings:  $WARNINGS"
echo "  Criticals: $CRITICALS"
echo ""

if [ "$CRITICALS" -gt 0 ]; then
    echo "  *** OVERALL STATUS: CRITICAL ***"
elif [ "$WARNINGS" -gt 0 ]; then
    echo "  *** OVERALL STATUS: WARNING ***"
else
    echo "  *** OVERALL STATUS: OK ***"
fi
```
