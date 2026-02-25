---
name: openwrt-security-hardening
description: Security audit for OpenWrt — dropbear SSH, firewall zones, uhttpd, default credentials, package updates
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
#!/bin/sh
# OpenWrt Security Hardening Playbook
# Audits SSH, firewall, web UI, credentials, and package update status

VERBOSITY="{{verbosity}}"
FIX_ISSUES="{{fix_issues}}"

sep() { printf '\n══════ %s ══════\n' "$1"; }

ISSUES=0
warn() { ISSUES=$((ISSUES + 1)); echo "  [!] $1"; }
ok()   { echo "  [ok] $1"; }
info() { echo "  [i] $1"; }

# ── Dropbear SSH Audit ────────────────────────────────────────────────────

sep "DROPBEAR SSH AUDIT"
if command -v dropbear >/dev/null 2>&1; then
    ok "Dropbear SSH is installed"

    echo "── Current config ──"
    uci show dropbear 2>/dev/null || echo "(uci not available)"
    echo ""

    echo "── Assessment ──"
    # Check each dropbear instance
    IDX=0
    while uci -q get dropbear.@dropbear[$IDX] >/dev/null 2>&1; do
        PREFIX="dropbear.@dropbear[$IDX]"
        PORT=$(uci -q get "${PREFIX}.Port" || echo "22")
        PASS_AUTH=$(uci -q get "${PREFIX}.PasswordAuth" || echo "on")
        ROOT_PASS=$(uci -q get "${PREFIX}.RootPasswordAuth" || echo "on")
        ROOT_LOGIN=$(uci -q get "${PREFIX}.RootLogin" || echo "1")
        GW_PORTS=$(uci -q get "${PREFIX}.GatewayPorts" || echo "off")
        INTERFACE=$(uci -q get "${PREFIX}.Interface" || echo "(all)")

        if [ "$IDX" -gt 0 ]; then echo ""; fi
        echo "  Instance $IDX (port $PORT, interface: $INTERFACE):"

        case "$PASS_AUTH" in
            on|1) warn "PasswordAuth enabled (instance $IDX)" ;;
            off|0) ok "PasswordAuth disabled (instance $IDX)" ;;
        esac

        case "$ROOT_PASS" in
            on|1) warn "RootPasswordAuth enabled (instance $IDX)" ;;
            off|0) ok "RootPasswordAuth disabled (instance $IDX)" ;;
        esac

        case "$ROOT_LOGIN" in
            1|on) info "RootLogin allowed (instance $IDX) — normal for OpenWrt" ;;
            0|off) ok "RootLogin disabled (instance $IDX)" ;;
        esac

        case "$GW_PORTS" in
            on|1) warn "GatewayPorts enabled (instance $IDX) — allows remote port forwards" ;;
            off|0) ok "GatewayPorts disabled (instance $IDX)" ;;
        esac

        if [ "$PORT" = "22" ]; then
            info "Using default port 22 (non-standard port adds minor obscurity)"
        fi

        IDX=$((IDX + 1))
    done

    if [ "$IDX" -eq 0 ]; then
        info "No dropbear instances configured via uci"
    fi

    # Check authorized keys
    if [ "$VERBOSITY" != "brief" ]; then
        echo ""
        echo "── Authorized keys ──"
        if [ -f /etc/dropbear/authorized_keys ]; then
            KEY_COUNT=$(grep -c . /etc/dropbear/authorized_keys 2>/dev/null)
            ok "$KEY_COUNT authorized key(s) found"
            if [ "$VERBOSITY" = "verbose" ]; then
                awk '{print "  " $1 " " $NF}' /etc/dropbear/authorized_keys 2>/dev/null
            fi
        else
            warn "No authorized_keys file — key-based auth not configured"
        fi
    fi

    # Auto-fix
    if [ "$FIX_ISSUES" = "auto-fix" ]; then
        echo ""
        echo "── Applying safe fixes ──"
        CHANGED=0
        IDX=0
        while uci -q get dropbear.@dropbear[$IDX] >/dev/null 2>&1; do
            PREFIX="dropbear.@dropbear[$IDX]"
            CUR_PASS=$(uci -q get "${PREFIX}.PasswordAuth" || echo "on")
            CUR_ROOT_PASS=$(uci -q get "${PREFIX}.RootPasswordAuth" || echo "on")

            if [ "$CUR_PASS" = "on" ] || [ "$CUR_PASS" = "1" ]; then
                uci set "${PREFIX}.PasswordAuth=off"
                echo "  [fix] Disabled PasswordAuth (instance $IDX)"
                CHANGED=1
            fi

            if [ "$CUR_ROOT_PASS" = "on" ] || [ "$CUR_ROOT_PASS" = "1" ]; then
                uci set "${PREFIX}.RootPasswordAuth=off"
                echo "  [fix] Disabled RootPasswordAuth (instance $IDX)"
                CHANGED=1
            fi

            IDX=$((IDX + 1))
        done

        if [ "$CHANGED" -eq 1 ]; then
            uci commit dropbear
            echo ""
            info "Dropbear config committed. Restart with: /etc/init.d/dropbear restart"
            info "WARNING: Ensure you have authorized_keys set before restarting!"
        else
            echo "  No changes needed."
        fi
    fi
else
    info "Dropbear not found — checking for openssh"
    if [ -f /etc/ssh/sshd_config ]; then
        info "OpenSSH detected — use the linux-security-hardening playbook instead"
    else
        warn "No SSH server found"
    fi
fi

# ── Firewall Zone Audit ──────────────────────────────────────────────────

sep "FIREWALL ZONE AUDIT"
if command -v uci >/dev/null 2>&1; then
    echo "── Zone policies ──"
    IDX=0
    while uci -q get firewall.@zone[$IDX] >/dev/null 2>&1; do
        PREFIX="firewall.@zone[$IDX]"
        ZONE_NAME=$(uci -q get "${PREFIX}.name" || echo "unknown")
        INPUT=$(uci -q get "${PREFIX}.input" || echo "ACCEPT")
        OUTPUT=$(uci -q get "${PREFIX}.output" || echo "ACCEPT")
        FORWARD=$(uci -q get "${PREFIX}.forward" || echo "REJECT")
        NETWORKS=$(uci -q get "${PREFIX}.network" || echo "(none)")

        printf "  %-10s  input=%-8s output=%-8s forward=%-8s  nets=%s\n" \
            "$ZONE_NAME" "$INPUT" "$OUTPUT" "$FORWARD" "$NETWORKS"

        # Assess WAN zone
        if [ "$ZONE_NAME" = "wan" ] || [ "$ZONE_NAME" = "WAN" ]; then
            case "$INPUT" in
                ACCEPT) warn "WAN zone input=ACCEPT — device is wide open to inbound traffic!" ;;
                DROP) ok "WAN zone input=DROP" ;;
                REJECT) ok "WAN zone input=REJECT" ;;
            esac
            case "$FORWARD" in
                ACCEPT) warn "WAN zone forward=ACCEPT — traffic forwarded without restriction" ;;
                DROP|REJECT) ok "WAN zone forward=$FORWARD" ;;
            esac
        fi

        IDX=$((IDX + 1))
    done

    # Check port forwards
    if [ "$VERBOSITY" != "brief" ]; then
        echo ""
        echo "── Port forwards ──"
        FWD_IDX=0
        FWD_COUNT=0
        while uci -q get firewall.@redirect[$FWD_IDX] >/dev/null 2>&1; do
            PREFIX="firewall.@redirect[$FWD_IDX]"
            FWD_NAME=$(uci -q get "${PREFIX}.name" || echo "(unnamed)")
            SRC=$(uci -q get "${PREFIX}.src" || echo "?")
            DEST=$(uci -q get "${PREFIX}.dest" || echo "?")
            DEST_IP=$(uci -q get "${PREFIX}.dest_ip" || echo "?")
            DEST_PORT=$(uci -q get "${PREFIX}.dest_port" || echo "?")
            SRC_DPORT=$(uci -q get "${PREFIX}.src_dport" || echo "?")
            PROTO=$(uci -q get "${PREFIX}.proto" || echo "?")
            ENABLED=$(uci -q get "${PREFIX}.enabled" || echo "1")

            if [ "$ENABLED" = "0" ]; then
                STATE="disabled"
            else
                STATE="ACTIVE"
            fi

            printf "  %-20s %s:%s -> %s:%s (%s) [%s]\n" \
                "$FWD_NAME" "$SRC" "$SRC_DPORT" "$DEST_IP" "$DEST_PORT" "$PROTO" "$STATE"

            # Flag risky forwards
            if [ "$ENABLED" != "0" ] && [ "$SRC" = "wan" ]; then
                case "$DEST_PORT" in
                    22|23|80|443|3306|5432)
                        warn "Port forward from WAN:$SRC_DPORT to $DEST_IP:$DEST_PORT ($PROTO) — sensitive service exposed" ;;
                esac
            fi

            FWD_IDX=$((FWD_IDX + 1))
            FWD_COUNT=$((FWD_COUNT + 1))
        done

        if [ "$FWD_COUNT" -eq 0 ]; then
            ok "No port forwards configured"
        else
            echo "  Total: $FWD_COUNT redirect rule(s)"
        fi
    fi

    # Check traffic rules for risky WAN accepts
    if [ "$VERBOSITY" = "verbose" ]; then
        echo ""
        echo "── Traffic rules (WAN input) ──"
        RULE_IDX=0
        while uci -q get firewall.@rule[$RULE_IDX] >/dev/null 2>&1; do
            PREFIX="firewall.@rule[$RULE_IDX]"
            RULE_NAME=$(uci -q get "${PREFIX}.name" || echo "(unnamed)")
            SRC=$(uci -q get "${PREFIX}.src" || echo "")
            TARGET=$(uci -q get "${PREFIX}.target" || echo "")
            DEST_PORT=$(uci -q get "${PREFIX}.dest_port" || echo "")
            ENABLED=$(uci -q get "${PREFIX}.enabled" || echo "1")

            if [ "$SRC" = "wan" ] && [ "$ENABLED" != "0" ]; then
                printf "  %-25s target=%-8s port=%s\n" "$RULE_NAME" "$TARGET" "${DEST_PORT:-(any)}"
                if [ "$TARGET" = "ACCEPT" ]; then
                    info "WAN accept rule: $RULE_NAME (port ${DEST_PORT:-(any)})"
                fi
            fi

            RULE_IDX=$((RULE_IDX + 1))
        done
    fi

    # Auto-fix WAN input policy
    if [ "$FIX_ISSUES" = "auto-fix" ]; then
        IDX=0
        while uci -q get firewall.@zone[$IDX] >/dev/null 2>&1; do
            PREFIX="firewall.@zone[$IDX]"
            ZONE_NAME=$(uci -q get "${PREFIX}.name")
            if [ "$ZONE_NAME" = "wan" ] || [ "$ZONE_NAME" = "WAN" ]; then
                CUR_INPUT=$(uci -q get "${PREFIX}.input" || echo "ACCEPT")
                if [ "$CUR_INPUT" = "ACCEPT" ]; then
                    echo ""
                    uci set "${PREFIX}.input=REJECT"
                    uci commit firewall
                    echo "  [fix] Set WAN input policy to REJECT"
                    info "Reload with: /etc/init.d/firewall reload"
                fi
            fi
            IDX=$((IDX + 1))
        done
    fi
else
    echo "(uci not available — cannot inspect firewall config)"
fi

# ── uhttpd Exposure ──────────────────────────────────────────────────────

sep "WEB UI (UHTTPD) EXPOSURE"
if command -v uhttpd >/dev/null 2>&1 || [ -f /etc/config/uhttpd ]; then
    echo "── Listening addresses ──"
    if command -v uci >/dev/null 2>&1; then
        HTTP_LISTEN=$(uci -q get uhttpd.main.listen_http)
        HTTPS_LISTEN=$(uci -q get uhttpd.main.listen_https)
        REDIRECT=$(uci -q get uhttpd.main.redirect_https)

        echo "  HTTP:  ${HTTP_LISTEN:-(not set)}"
        echo "  HTTPS: ${HTTPS_LISTEN:-(not set)}"
        echo "  Redirect to HTTPS: ${REDIRECT:-(not set)}"
        echo ""

        echo "── Assessment ──"
        # Check if listening on all interfaces
        if echo "$HTTP_LISTEN" | grep -q "0\.0\.0\.0\|::"; then
            warn "uhttpd HTTP listening on all interfaces — may be exposed on WAN"
        else
            ok "uhttpd HTTP bound to specific address(es)"
        fi

        if echo "$HTTPS_LISTEN" | grep -q "0\.0\.0\.0\|::"; then
            warn "uhttpd HTTPS listening on all interfaces — may be exposed on WAN"
        else
            ok "uhttpd HTTPS bound to specific address(es)"
        fi

        case "$REDIRECT" in
            1|on) ok "HTTP to HTTPS redirect enabled" ;;
            *) info "HTTP to HTTPS redirect not enabled" ;;
        esac
    fi

    # Verify with actual listening sockets
    if [ "$VERBOSITY" != "brief" ]; then
        echo ""
        echo "── Actual listening sockets ──"
        netstat -tlnp 2>/dev/null | grep uhttpd || ss -tlnp 2>/dev/null | grep uhttpd || echo "(cannot determine)"
    fi
else
    info "uhttpd not found — no web UI installed"
fi

# ── Default Credential Check ─────────────────────────────────────────────

sep "DEFAULT CREDENTIALS"
echo "── Root password check ──"
if [ -r /etc/shadow ]; then
    ROOT_HASH=$(awk -F: '$1 == "root" {print $2}' /etc/shadow 2>/dev/null)
    case "$ROOT_HASH" in
        ""|"!"|"!!"|"*")
            warn "Root has no password set — anyone can login via serial/console"
            info "Set with: passwd root"
            ;;
        '$'*)
            ok "Root password is set (hashed)"
            ;;
        "x")
            info "Root password hash in /etc/shadow (standard)"
            ;;
        *)
            ok "Root password entry present"
            ;;
    esac
else
    # OpenWrt may use /etc/passwd for password hash
    ROOT_HASH=$(awk -F: '$1 == "root" {print $2}' /etc/passwd 2>/dev/null)
    case "$ROOT_HASH" in
        ""|"x"|"!"|"*")
            if [ "$ROOT_HASH" = "x" ]; then
                info "Password in /etc/shadow (cannot read without root)"
            else
                warn "Root may have no password set"
            fi
            ;;
        '$'*)
            ok "Root password is set (hashed)"
            ;;
    esac
fi

if [ "$VERBOSITY" != "brief" ]; then
    echo ""
    echo "── Other accounts ──"
    awk -F: '$1 != "root" && $2 != "x" && $2 != "*" && $2 != "!" && $2 != "!!" && $2 != "" {print "  [i] Account with direct password hash: " $1}' /etc/passwd 2>/dev/null
    awk -F: '$1 != "root" && ($2 == "" || $2 == "!") {print "  [!] No password: " $1}' /etc/shadow 2>/dev/null
    echo "  (done)"
fi

# ── Package Update Status ────────────────────────────────────────────────

sep "PACKAGE UPDATES"
if command -v opkg >/dev/null 2>&1; then
    echo "── Updating package lists ──"
    opkg update >/dev/null 2>&1
    echo ""
    echo "── Upgradable packages ──"
    UPGRADABLE=$(opkg list-upgradable 2>/dev/null)
    if [ -n "$UPGRADABLE" ]; then
        UPG_COUNT=$(echo "$UPGRADABLE" | wc -l)
        warn "$UPG_COUNT package(s) have updates available"
        if [ "$VERBOSITY" = "brief" ]; then
            echo "$UPGRADABLE" | head -10
            [ "$UPG_COUNT" -gt 10 ] && echo "  ... and $((UPG_COUNT - 10)) more"
        else
            echo "$UPGRADABLE"
        fi
    else
        ok "All packages are up to date"
    fi

    if [ "$VERBOSITY" = "verbose" ]; then
        echo ""
        echo "── Installed packages ──"
        opkg list-installed | wc -l | xargs printf "  Total installed: %s packages\n"
    fi
else
    info "opkg not available — cannot check package updates"
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
