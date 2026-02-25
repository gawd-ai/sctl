---
name: openwrt-health-check
description: Health monitoring for OpenWrt — flash storage, RAM, processes, uptime, syslog errors, NTP, watchdog
params:
  disk_threshold:
    type: string
    description: Storage usage percentage threshold for alerts
    default: "90"
  verbosity:
    type: string
    description: Output detail level
    default: normal
    enum: [brief, normal, verbose]
---

```sh
#!/bin/sh
# OpenWrt Health Check Playbook
# POSIX sh compatible — works with busybox ash

DISK_THRESHOLD="{{disk_threshold}}"
VERBOSITY="{{verbosity}}"

sep() { printf '\n══════ %s ══════\n' "$1"; }

WARNINGS=0
CRITICALS=0

warn() { WARNINGS=$((WARNINGS + 1)); echo "  WARNING: $1"; }
crit() { CRITICALS=$((CRITICALS + 1)); echo "  CRITICAL: $1"; }

# ── Flash Storage ─────────────────────────────────────────────────────────

sep "FLASH STORAGE"
echo "Threshold: ${DISK_THRESHOLD}%"
echo ""
df -h
echo ""

DISK_ALERT=0
# Check overlay and rootfs specifically
for mount_point in / /overlay /rom /tmp; do
    line=$(df 2>/dev/null | grep " ${mount_point}$")
    if [ -n "$line" ]; then
        pct=$(echo "$line" | awk '{print $5}' | tr -d '%')
        if [ -n "$pct" ] && [ "$pct" -ge "$DISK_THRESHOLD" ] 2>/dev/null; then
            crit "Storage $mount_point is at ${pct}% (threshold: ${DISK_THRESHOLD}%)"
            DISK_ALERT=1
        fi
    fi
done

if [ "$DISK_ALERT" -eq 0 ]; then
    echo "  OK: All storage below ${DISK_THRESHOLD}%"
fi

if [ "$VERBOSITY" = "verbose" ]; then
    echo ""
    echo "── Overlay details ──"
    if [ -f /proc/mtd ]; then
        echo "── MTD partitions ──"
        cat /proc/mtd
    fi
    echo ""
    echo "── Mount points ──"
    mount
fi

# ── RAM Pressure ──────────────────────────────────────────────────────────

sep "RAM PRESSURE"
if [ -f /proc/meminfo ]; then
    MEM_TOTAL=$(grep MemTotal /proc/meminfo | awk '{print $2}')
    MEM_FREE=$(grep MemFree /proc/meminfo | awk '{print $2}')
    MEM_BUFFERS=$(grep Buffers /proc/meminfo | awk '{print $2}')
    MEM_CACHED=$(grep "^Cached:" /proc/meminfo | awk '{print $2}')

    # Available = Free + Buffers + Cached
    MEM_AVAIL=$((MEM_FREE + ${MEM_BUFFERS:-0} + ${MEM_CACHED:-0}))
    MEM_USED=$((MEM_TOTAL - MEM_AVAIL))

    if [ "$MEM_TOTAL" -gt 0 ] 2>/dev/null; then
        MEM_PCT=$((MEM_USED * 100 / MEM_TOTAL))
        MEM_TOTAL_MB=$((MEM_TOTAL / 1024))
        MEM_AVAIL_MB=$((MEM_AVAIL / 1024))
        MEM_USED_MB=$((MEM_USED / 1024))

        echo "Total: ${MEM_TOTAL_MB}MB  Used: ${MEM_USED_MB}MB  Available: ${MEM_AVAIL_MB}MB  (${MEM_PCT}%)"

        if [ "$MEM_PCT" -ge 95 ]; then
            crit "RAM usage at ${MEM_PCT}%"
        elif [ "$MEM_PCT" -ge 85 ]; then
            warn "RAM usage at ${MEM_PCT}%"
        else
            echo "  OK: RAM usage at ${MEM_PCT}%"
        fi
    fi

    if [ "$VERBOSITY" != "brief" ]; then
        echo ""
        free 2>/dev/null || cat /proc/meminfo | head -8
    fi

    if [ "$VERBOSITY" = "verbose" ]; then
        echo ""
        cat /proc/meminfo
    fi
else
    echo "(cannot read /proc/meminfo)"
fi

# ── Process Count & Load ─────────────────────────────────────────────────

sep "PROCESSES & LOAD"
PROC_COUNT=$(ps w 2>/dev/null | wc -l)
PROC_COUNT=$((PROC_COUNT - 1))  # subtract header line
echo "Running processes: $PROC_COUNT"
echo ""

LOADAVG=$(cat /proc/loadavg)
echo "Load average: $LOADAVG"

# Get CPU count (OpenWrt may have /sys/devices/system/cpu/present)
NPROC=1
if [ -f /sys/devices/system/cpu/present ]; then
    # Format: "0-N" or "0"
    RANGE=$(cat /sys/devices/system/cpu/present)
    LAST_CPU=$(echo "$RANGE" | sed 's/.*-//')
    NPROC=$((LAST_CPU + 1))
elif [ -f /proc/cpuinfo ]; then
    NPROC=$(grep -c "^processor" /proc/cpuinfo)
    [ "$NPROC" -eq 0 ] && NPROC=1
fi

LOAD1=$(echo "$LOADAVG" | awk '{print $1}')
LOAD_INT=$(echo "$LOAD1" | cut -d. -f1)
if [ "${LOAD_INT:-0}" -ge "$NPROC" ] 2>/dev/null; then
    warn "Load average ($LOAD1) >= CPU count ($NPROC)"
else
    echo "  OK: Load average ($LOAD1) within bounds (${NPROC} CPUs)"
fi

if [ "$VERBOSITY" != "brief" ]; then
    echo ""
    echo "── Top processes ──"
    top -b -n1 2>/dev/null | head -20 || ps w 2>/dev/null | head -20
fi

# ── Uptime ────────────────────────────────────────────────────────────────

sep "UPTIME"
uptime
if [ -f /proc/uptime ]; then
    UPTIME_SECS=$(cat /proc/uptime | awk '{print $1}' | cut -d. -f1)
    UPTIME_DAYS=$((UPTIME_SECS / 86400))
    UPTIME_HOURS=$(( (UPTIME_SECS % 86400) / 3600 ))
    echo "  Uptime: ${UPTIME_DAYS} days, ${UPTIME_HOURS} hours"
fi

# ── Syslog Errors ─────────────────────────────────────────────────────────

sep "SYSLOG ERRORS"
if command -v logread >/dev/null 2>&1; then
    case "$VERBOSITY" in
        brief)   LOG_LINES=5  ;;
        normal)  LOG_LINES=15 ;;
        verbose) LOG_LINES=50 ;;
    esac

    ERROR_LOG=$(logread 2>/dev/null | grep -i "error\|fail\|crit\|emerg\|panic" | tail -"$LOG_LINES")
    ERROR_COUNT=$(logread 2>/dev/null | grep -ic "error\|fail\|crit\|emerg\|panic")

    if [ -n "$ERROR_LOG" ]; then
        echo "Found $ERROR_COUNT error-level messages in syslog (showing last $LOG_LINES):"
        echo ""
        echo "$ERROR_LOG"

        # Check for recent errors (crude: check if any contain today's date)
        TODAY=$(date +"%b %e" | sed 's/  / /')
        RECENT=$(echo "$ERROR_LOG" | grep -c "$TODAY" 2>/dev/null || echo 0)
        if [ "$RECENT" -gt 0 ]; then
            warn "$RECENT error(s) from today in syslog"
        fi
    else
        echo "  OK: No error-level messages in syslog"
    fi

    if [ "$VERBOSITY" = "verbose" ]; then
        echo ""
        echo "── Full syslog (last 30 lines) ──"
        logread 2>/dev/null | tail -30
    fi
else
    echo "(logread not available)"
fi

# ── NTP Sync ──────────────────────────────────────────────────────────────

sep "NTP SYNC"
echo "Current time: $(date)"
echo ""

NTP_OK=0

# Check for busybox ntpd
if pidof ntpd >/dev/null 2>&1; then
    echo "  ntpd is running (PID: $(pidof ntpd))"
    NTP_OK=1

    # Check UCI NTP config
    if command -v uci >/dev/null 2>&1; then
        NTP_ENABLED=$(uci get system.ntp.enabled 2>/dev/null)
        NTP_SERVERS=$(uci get system.ntp.server 2>/dev/null)
        echo "  NTP enabled: ${NTP_ENABLED:-unknown}"
        echo "  NTP servers: ${NTP_SERVERS:-unknown}"
    fi
else
    warn "ntpd is NOT running"
fi

# Check if sysntpd service exists
if [ -f /etc/init.d/sysntpd ]; then
    RUNNING="stopped"
    /etc/init.d/sysntpd running 2>/dev/null && RUNNING="running"
    echo "  sysntpd service: $RUNNING"
fi

# Check time drift by comparing to build time if available
if [ -f /etc/openwrt_release ]; then
    BUILD_DATE=$(date -r /etc/openwrt_release +%s 2>/dev/null)
    NOW=$(date +%s)
    if [ -n "$BUILD_DATE" ] && [ "$NOW" -lt "$BUILD_DATE" ] 2>/dev/null; then
        crit "System clock ($NOW) is behind build date ($BUILD_DATE) — time may not be synced"
    fi
fi

if [ "$NTP_OK" -eq 0 ]; then
    # No NTP running, this is a concern
    warn "No NTP daemon detected — clock may drift"
else
    echo "  OK: NTP daemon is active"
fi

# ── Watchdog ──────────────────────────────────────────────────────────────

sep "WATCHDOG"
WATCHDOG_OK=0

if [ -c /dev/watchdog ]; then
    echo "  /dev/watchdog device: present"
else
    echo "  /dev/watchdog device: not found"
fi

if [ -c /dev/watchdog0 ]; then
    echo "  /dev/watchdog0 device: present"
fi

# Check if watchdog daemon is running
if pidof watchdog >/dev/null 2>&1; then
    echo "  watchdog daemon: running (PID: $(pidof watchdog))"
    WATCHDOG_OK=1
elif [ -f /etc/init.d/watchdog ]; then
    RUNNING="stopped"
    /etc/init.d/watchdog running 2>/dev/null && RUNNING="running"
    echo "  watchdog service: $RUNNING"
    [ "$RUNNING" = "running" ] && WATCHDOG_OK=1
fi

# Check UCI watchdog config (built into procd on OpenWrt)
if command -v uci >/dev/null 2>&1; then
    WD_ENABLED=$(uci get system.watchdog 2>/dev/null)
    if [ -n "$WD_ENABLED" ]; then
        echo "  UCI watchdog config:"
        uci show system.watchdog 2>/dev/null | while read -r line; do
            echo "    $line"
        done
    fi
fi

# procd has built-in watchdog support
if [ -c /dev/watchdog ] || [ -c /dev/watchdog0 ]; then
    if [ "$WATCHDOG_OK" -eq 0 ]; then
        # procd feeds watchdog directly on OpenWrt
        if pidof procd >/dev/null 2>&1; then
            echo "  procd is running — likely managing watchdog internally"
            WATCHDOG_OK=1
        fi
    fi
    if [ "$WATCHDOG_OK" -eq 1 ]; then
        echo "  OK: Watchdog is active"
    else
        warn "Watchdog device exists but no daemon is feeding it"
    fi
else
    echo "  (no watchdog hardware detected)"
fi

# ── Overall Health Summary ────────────────────────────────────────────────

sep "HEALTH SUMMARY"
echo "Checks completed at $(date)"
echo "Device: $(cat /tmp/sysinfo/model 2>/dev/null || echo 'unknown')"
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
