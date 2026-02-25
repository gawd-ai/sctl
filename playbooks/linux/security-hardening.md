---
name: linux-security-hardening
description: Security audit and hardening for Linux systems — SSH config, open ports, users, passwords, fail2ban, SUID files
params:
  verbosity:
    type: string
    description: Output detail level
    default: normal
    enum: [brief, normal, verbose]
  fix_issues:
    type: string
    description: Whether to auto-fix safe issues or just report
    default: report-only
    enum: [report-only, auto-fix]
---

```sh
#!/bin/bash
# Linux Security Hardening Playbook
# Compatible with Ubuntu, Debian, RHEL, Fedora, Arch, Alpine, etc.

VERBOSITY="{{verbosity}}"
FIX_ISSUES="{{fix_issues}}"

sep() { printf '\n══════ %s ══════\n' "$1"; }

ISSUES=0
warn() { ISSUES=$((ISSUES + 1)); echo "  [!] $1"; }
ok()   { echo "  [ok] $1"; }
info() { echo "  [i] $1"; }

# ── SSH Config Audit ──────────────────────────────────────────────────────

sep "SSH CONFIG AUDIT"
SSHD_CONFIG="/etc/ssh/sshd_config"
if [ -f "$SSHD_CONFIG" ]; then
    # Helper: get effective sshd config value (first uncommented match)
    sshd_val() {
        grep -Ei "^\s*$1\s" "$SSHD_CONFIG" 2>/dev/null | head -1 | awk '{print $2}'
    }

    PERMIT_ROOT=$(sshd_val PermitRootLogin)
    PASS_AUTH=$(sshd_val PasswordAuthentication)
    PUBKEY_AUTH=$(sshd_val PubkeyAuthentication)
    SSH_PORT=$(sshd_val Port)
    X11_FWD=$(sshd_val X11Forwarding)
    MAX_AUTH=$(sshd_val MaxAuthTries)

    echo "── Current settings ──"
    printf "  %-26s %s\n" "PermitRootLogin:" "${PERMIT_ROOT:-"(default: prohibit-password)"}"
    printf "  %-26s %s\n" "PasswordAuthentication:" "${PASS_AUTH:-"(default: yes)"}"
    printf "  %-26s %s\n" "PubkeyAuthentication:" "${PUBKEY_AUTH:-"(default: yes)"}"
    printf "  %-26s %s\n" "Port:" "${SSH_PORT:-"(default: 22)"}"
    printf "  %-26s %s\n" "X11Forwarding:" "${X11_FWD:-"(default: no)"}"
    printf "  %-26s %s\n" "MaxAuthTries:" "${MAX_AUTH:-"(default: 6)"}"
    echo ""

    echo "── Assessment ──"
    # PermitRootLogin
    case "${PERMIT_ROOT:-prohibit-password}" in
        yes) warn "PermitRootLogin is 'yes' — root can login with password" ;;
        no) ok "PermitRootLogin disabled entirely" ;;
        prohibit-password|without-password) ok "PermitRootLogin allows key-only" ;;
        *) info "PermitRootLogin set to '${PERMIT_ROOT}'" ;;
    esac

    # PasswordAuthentication
    case "${PASS_AUTH:-yes}" in
        yes) warn "PasswordAuthentication enabled — brute force risk" ;;
        no) ok "PasswordAuthentication disabled" ;;
    esac

    # PubkeyAuthentication
    case "${PUBKEY_AUTH:-yes}" in
        yes) ok "PubkeyAuthentication enabled" ;;
        no) warn "PubkeyAuthentication disabled — key-based auth not available" ;;
    esac

    # X11Forwarding
    case "${X11_FWD:-no}" in
        yes) warn "X11Forwarding enabled — unnecessary attack surface" ;;
        no) ok "X11Forwarding disabled" ;;
    esac

    # MaxAuthTries
    if [ -n "$MAX_AUTH" ] && [ "$MAX_AUTH" -gt 6 ] 2>/dev/null; then
        warn "MaxAuthTries is $MAX_AUTH (recommended: 3-6)"
    else
        ok "MaxAuthTries is ${MAX_AUTH:-6}"
    fi

    # Port
    if [ "${SSH_PORT:-22}" = "22" ]; then
        info "SSH on default port 22 (non-standard port adds minor obscurity)"
    else
        ok "SSH on non-standard port $SSH_PORT"
    fi

    # Auto-fix
    if [ "$FIX_ISSUES" = "auto-fix" ]; then
        echo ""
        echo "── Applying safe fixes ──"
        CHANGED=0

        if [ "${PASS_AUTH:-yes}" = "yes" ]; then
            if grep -qE "^\s*PasswordAuthentication\s" "$SSHD_CONFIG"; then
                sed -i 's/^\s*PasswordAuthentication\s.*/PasswordAuthentication no/' "$SSHD_CONFIG"
            else
                echo "PasswordAuthentication no" >> "$SSHD_CONFIG"
            fi
            echo "  [fix] Set PasswordAuthentication no"
            CHANGED=1
        fi

        if [ "${PERMIT_ROOT:-prohibit-password}" = "yes" ]; then
            if grep -qE "^\s*PermitRootLogin\s" "$SSHD_CONFIG"; then
                sed -i 's/^\s*PermitRootLogin\s.*/PermitRootLogin prohibit-password/' "$SSHD_CONFIG"
            else
                echo "PermitRootLogin prohibit-password" >> "$SSHD_CONFIG"
            fi
            echo "  [fix] Set PermitRootLogin prohibit-password"
            CHANGED=1
        fi

        if [ "$CHANGED" -eq 1 ]; then
            echo ""
            info "sshd_config modified. Validate with: sshd -t"
            info "Reload with: systemctl reload sshd  (or: service sshd reload)"
        else
            echo "  No changes needed."
        fi
    fi
else
    info "No $SSHD_CONFIG found — SSH server may not be installed"
    if command -v dropbear >/dev/null 2>&1; then
        info "Dropbear SSH detected — use the OpenWrt playbook instead"
    fi
fi

# ── Open Port Scan ────────────────────────────────────────────────────────

sep "OPEN PORTS"
if command -v ss >/dev/null 2>&1; then
    echo "── Listening TCP ports ──"
    ss -tlnp 2>/dev/null
elif command -v netstat >/dev/null 2>&1; then
    echo "── Listening TCP ports ──"
    netstat -tlnp 2>/dev/null
else
    echo "(no ss or netstat available)"
fi

if [ "$VERBOSITY" != "brief" ]; then
    echo ""
    echo "── Listening UDP ports ──"
    ss -ulnp 2>/dev/null || netstat -ulnp 2>/dev/null || echo "(not available)"
fi

echo ""
echo "── Assessment ──"
# Check for common risky open ports
for PORT_INFO in "3306:MySQL" "5432:PostgreSQL" "6379:Redis" "27017:MongoDB" "11211:Memcached" "9200:Elasticsearch"; do
    PORT="${PORT_INFO%%:*}"
    NAME="${PORT_INFO##*:}"
    if ss -tlnp 2>/dev/null | grep -q ":${PORT}\b"; then
        BIND=$(ss -tlnp 2>/dev/null | grep ":${PORT}\b" | awk '{print $4}')
        if echo "$BIND" | grep -qE "^(0\.0\.0\.0|\*|\[::\])"; then
            warn "$NAME (port $PORT) listening on all interfaces"
        else
            ok "$NAME (port $PORT) bound to local address only"
        fi
    fi
done

# ── User / Permission Review ─────────────────────────────────────────────

sep "USER & PERMISSION REVIEW"
echo "── Users with shell access ──"
grep -v '/nologin\|/false\|/sync\|/halt\|/shutdown' /etc/passwd 2>/dev/null | while IFS=: read -r user _ uid gid _ home shell; do
    printf "  %-16s UID=%-6s Shell=%s\n" "$user" "$uid" "$shell"
done

echo ""
echo "── UID 0 accounts ──"
UID0=$(awk -F: '$3 == 0 {print $1}' /etc/passwd 2>/dev/null)
echo "$UID0" | while read -r user; do
    if [ "$user" = "root" ]; then
        ok "root (expected)"
    elif [ -n "$user" ]; then
        warn "Non-root UID 0 account: $user"
    fi
done

if [ "$VERBOSITY" != "brief" ]; then
    echo ""
    echo "── World-writable files in /etc ──"
    WW_FILES=$(find /etc -xdev -type f -perm -0002 2>/dev/null | head -20)
    if [ -n "$WW_FILES" ]; then
        echo "$WW_FILES" | while read -r f; do warn "World-writable: $f"; done
    else
        ok "No world-writable files found in /etc"
    fi
fi

# ── Password Policy ──────────────────────────────────────────────────────

sep "PASSWORD POLICY"
if [ -f /etc/login.defs ]; then
    echo "── /etc/login.defs ──"
    for KEY in PASS_MAX_DAYS PASS_MIN_DAYS PASS_MIN_LEN PASS_WARN_AGE; do
        VAL=$(grep -E "^\s*${KEY}\s" /etc/login.defs 2>/dev/null | awk '{print $2}')
        printf "  %-20s %s\n" "$KEY:" "${VAL:-(not set)}"
    done

    PASS_MAX=$(grep -E "^\s*PASS_MAX_DAYS\s" /etc/login.defs 2>/dev/null | awk '{print $2}')
    if [ -n "$PASS_MAX" ] && [ "$PASS_MAX" -gt 365 ] 2>/dev/null; then
        warn "PASS_MAX_DAYS is $PASS_MAX (>365 days)"
    elif [ "$PASS_MAX" = "99999" ]; then
        warn "PASS_MAX_DAYS is 99999 (passwords never expire)"
    fi
else
    info "No /etc/login.defs found"
fi

echo ""
echo "── Users without passwords ──"
if [ -r /etc/shadow ]; then
    EMPTY_PW=$(awk -F: '($2 == "" || $2 == "!!" || $2 == "!") && $1 != "!" {print $1}' /etc/shadow 2>/dev/null)
    if [ -n "$EMPTY_PW" ]; then
        echo "$EMPTY_PW" | while read -r user; do
            warn "No password set for user: $user"
        done
    else
        ok "All accounts have passwords or are locked"
    fi
else
    info "Cannot read /etc/shadow (need root)"
fi

# ── Automatic Updates ─────────────────────────────────────────────────────

sep "AUTOMATIC UPDATES"
if command -v apt >/dev/null 2>&1; then
    echo "── Unattended-upgrades (apt) ──"
    if dpkg -l unattended-upgrades 2>/dev/null | grep -q "^ii"; then
        ok "unattended-upgrades is installed"
        if systemctl is-active --quiet unattended-upgrades 2>/dev/null; then
            ok "unattended-upgrades service is active"
        else
            warn "unattended-upgrades installed but service not active"
        fi
        if [ "$VERBOSITY" = "verbose" ]; then
            echo ""
            echo "── Config ──"
            cat /etc/apt/apt.conf.d/50unattended-upgrades 2>/dev/null | grep -v "^//" | grep -v "^\s*$" | head -20
        fi
    else
        warn "unattended-upgrades not installed"
    fi
elif command -v dnf >/dev/null 2>&1; then
    echo "── dnf-automatic ──"
    if rpm -q dnf-automatic >/dev/null 2>&1; then
        ok "dnf-automatic is installed"
        systemctl is-active --quiet dnf-automatic.timer 2>/dev/null && ok "Timer is active" || warn "Timer is not active"
    else
        warn "dnf-automatic not installed"
    fi
elif command -v yum >/dev/null 2>&1; then
    echo "── yum-cron ──"
    if rpm -q yum-cron >/dev/null 2>&1; then
        ok "yum-cron is installed"
        systemctl is-active --quiet yum-cron 2>/dev/null && ok "Service is active" || warn "Service is not active"
    else
        warn "yum-cron not installed"
    fi
else
    info "Package manager not recognized — cannot check automatic updates"
fi

# ── fail2ban ──────────────────────────────────────────────────────────────

sep "FAIL2BAN"
if command -v fail2ban-client >/dev/null 2>&1; then
    ok "fail2ban is installed"
    if systemctl is-active --quiet fail2ban 2>/dev/null; then
        ok "fail2ban service is running"
    else
        warn "fail2ban installed but not running"
    fi

    echo ""
    echo "── Active jails ──"
    fail2ban-client status 2>/dev/null || echo "(cannot query fail2ban — need root?)"

    if [ "$VERBOSITY" != "brief" ]; then
        echo ""
        echo "── Banned IPs (sshd jail) ──"
        fail2ban-client status sshd 2>/dev/null | grep -A 999 "Banned IP" || echo "(sshd jail not active or not accessible)"
    fi

    if [ "$VERBOSITY" = "verbose" ]; then
        echo ""
        echo "── All jail details ──"
        JAILS=$(fail2ban-client status 2>/dev/null | grep "Jail list" | sed 's/.*://;s/,/ /g' | xargs)
        for jail in $JAILS; do
            echo "── Jail: $jail ──"
            fail2ban-client status "$jail" 2>/dev/null
            echo ""
        done
    fi
else
    warn "fail2ban is not installed — no brute-force protection"
    info "Install with: apt install fail2ban  (or: dnf install fail2ban)"
fi

# ── SUID / SGID File Scan ────────────────────────────────────────────────

sep "SUID / SGID FILE SCAN"
echo "── SUID binaries ──"
SUID_FILES=$(find / -xdev -type f -perm -4000 2>/dev/null | sort)
SUID_COUNT=$(echo "$SUID_FILES" | grep -c . 2>/dev/null)
echo "  Found $SUID_COUNT SUID files"

# Known safe SUID binaries
KNOWN_SUID="/usr/bin/sudo /usr/bin/su /usr/bin/passwd /usr/bin/chfn /usr/bin/chsh /usr/bin/newgrp /usr/bin/gpasswd /usr/bin/mount /usr/bin/umount /usr/bin/pkexec /usr/lib/dbus-1.0/dbus-daemon-launch-helper /usr/lib/openssh/ssh-keysign /usr/libexec/openssh/ssh-keysign /usr/bin/crontab /usr/bin/fusermount /usr/bin/fusermount3 /usr/sbin/pppd /usr/sbin/unix_chkpwd /usr/lib/polkit-1/polkit-agent-helper-1"

if [ "$VERBOSITY" = "brief" ]; then
    # In brief mode, only show unexpected SUID files
    echo "$SUID_FILES" | while read -r f; do
        [ -z "$f" ] && continue
        KNOWN=0
        for k in $KNOWN_SUID; do
            [ "$f" = "$k" ] && KNOWN=1 && break
        done
        [ "$KNOWN" -eq 0 ] && warn "Unusual SUID: $f"
    done
else
    echo "$SUID_FILES" | while read -r f; do
        [ -z "$f" ] && continue
        KNOWN=0
        for k in $KNOWN_SUID; do
            [ "$f" = "$k" ] && KNOWN=1 && break
        done
        if [ "$KNOWN" -eq 1 ]; then
            printf "  %-50s (known)\n" "$f"
        else
            warn "Unusual SUID: $f"
        fi
    done
fi

if [ "$VERBOSITY" != "brief" ]; then
    echo ""
    echo "── SGID binaries ──"
    SGID_FILES=$(find / -xdev -type f -perm -2000 2>/dev/null | sort)
    SGID_COUNT=$(echo "$SGID_FILES" | grep -c . 2>/dev/null)
    echo "  Found $SGID_COUNT SGID files"
    if [ "$VERBOSITY" = "verbose" ]; then
        echo "$SGID_FILES" | while read -r f; do
            [ -z "$f" ] && continue
            ls -la "$f"
        done
    fi
fi

# ── Summary ───────────────────────────────────────────────────────────────

sep "SUMMARY"
if [ "$ISSUES" -gt 0 ]; then
    echo "  Found $ISSUES potential issue(s) flagged with [!]"
    if [ "$FIX_ISSUES" = "report-only" ]; then
        echo "  Re-run with fix_issues=auto-fix to apply safe fixes"
    fi
else
    echo "  No issues found"
fi
echo ""
echo "Security audit completed at $(date)"
```
