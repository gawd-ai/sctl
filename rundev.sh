#!/usr/bin/env bash
#
# rundev.sh — Build & run the full sctl dev stack:
#             sctl server + mcp proxy + web UI
#
# Usage:
#   ./rundev.sh          # build everything, start all services
#   ./rundev.sh build    # build only (server, mcp, web) — no start/stop
#   ./rundev.sh start    # restart all services without rebuilding
#   ./rundev.sh stop     # stop all services and deregister MCP
#   ./rundev.sh status   # show what's running
#   ./rundev.sh claude   # only register MCP in Claude Code (no build/start)
#   ./rundev.sh tunnel   # build + start tunnel dev env (relay + clients + MCP via relay)
#
# Device management:
#   ./rundev.sh device add <name> <host>   # discover + register a device
#   ./rundev.sh device rm <name>           # remove a device
#   ./rundev.sh device ls                  # list devices with health status
#   ./rundev.sh device deploy <name>       # full deploy (binary + config + init)
#   ./rundev.sh device upgrade <name>      # binary-only upgrade
#
set -euo pipefail

REPO_DIR="$(cd "$(dirname "$0")" && pwd)"
SCTL_DIR="$REPO_DIR/server"
MCP_DIR="$REPO_DIR/mcp"
WEB_DIR="$REPO_DIR/web"

# Dev config
API_KEY="dev-key"
LISTEN="127.0.0.1:1337"
DEVICE_URL="http://${LISTEN}"
DATA_DIR="/tmp/sctl-dev"
PLAYBOOKS_DIR="$DATA_DIR/playbooks"
PID_FILE="$DATA_DIR/sctl.pid"
WEB_PID_FILE="$DATA_DIR/web.pid"
MCP_NAME="sctl"
WEB_PORT=5173

# Persistent MCP devices config — survives reboots.
# Uses a dev-specific filename so rundev never pollutes a prod/stage config.
PERSISTENT_CONFIG="$HOME/.config/sctl/devices.dev.json"
CONFIG_FILE="$PERSISTENT_CONFIG"

# Tunnel relay config
RELAY_LISTEN="0.0.0.0:8443"
RELAY_PID_FILE="$DATA_DIR/relay.pid"
TUNNEL_KEY="dev-tunnel-key"
DEVICE_SERIAL="DEV-LOCAL-001"

# Binaries (release for speed, debug takes too long on PTY-heavy sessions)
SCTL_BIN="$SCTL_DIR/target/release/sctl"
MCP_BIN="$MCP_DIR/target/release/mcp-sctl"

# Architecture → cross-compile target mapping
declare -A ARCH_TARGET=(
    [riscv64]=riscv64gc-unknown-linux-musl
    [armv7l]=armv7-unknown-linux-musleabihf
    [aarch64]=aarch64-unknown-linux-musl
    [x86_64]=native
)

log()  { echo -e "\033[1;34m==>\033[0m $*"; }
err()  { echo -e "\033[1;31m==>\033[0m $*" >&2; }
ok()   { echo -e "\033[1;32m==>\033[0m $*"; }
warn() { echo -e "\033[1;33m==>\033[0m $*" >&2; }

# ─── Config helpers ──────────────────────────────────────────────────

require_jq() {
    if ! command -v jq &>/dev/null; then
        err "jq is required but not installed. Install it with: sudo apt install jq"
        exit 1
    fi
}

ensure_config() {
    mkdir -p "$(dirname "$CONFIG_FILE")"
    if [[ ! -f "$CONFIG_FILE" ]]; then
        cat > "$CONFIG_FILE" <<'EOF'
{
  "config_version": 2,
  "devices": {},
  "default_device": null
}
EOF
    fi
}

cfg_get() {
    jq -r "$1" "$CONFIG_FILE"
}

cfg_device_get() {
    local name="$1" field="$2"
    jq -r ".devices[\"$name\"].$field // empty" "$CONFIG_FILE"
}

cfg_device_exists() {
    local name="$1"
    jq -e ".devices[\"$name\"] != null" "$CONFIG_FILE" &>/dev/null
}

cfg_device_names() {
    jq -r '.devices | keys[]' "$CONFIG_FILE"
}

# ─── Architecture helpers ────────────────────────────────────────────

arch_to_target() {
    local arch="$1"
    echo "${ARCH_TARGET[$arch]:-unknown}"
}

arch_to_bin() {
    local arch="$1"
    local target
    target=$(arch_to_target "$arch")
    if [[ "$target" == "native" ]]; then
        echo "$SCTL_DIR/target/release/sctl"
    elif [[ "$target" == "unknown" ]]; then
        err "Unknown architecture: $arch"
        return 1
    else
        echo "$SCTL_DIR/target/$target/release/sctl"
    fi
}

# ─── Shared helpers ──────────────────────────────────────────────────

# wait_for_health <url> <pid> <name> <logfile> [max_attempts] [sleep_interval]
# Waits for a service to respond to health checks.
wait_for_health() {
    local url="$1" pid="$2" name="$3" logfile="$4"
    local max_attempts="${5:-30}" sleep_interval="${6:-0.2}"

    for _ in $(seq 1 "$max_attempts"); do
        if curl -sf "$url" >/dev/null 2>&1; then
            ok "$name running (PID $pid)"
            return 0
        fi
        if ! kill -0 "$pid" 2>/dev/null; then
            err "$name exited unexpectedly. Log:"
            tail -20 "$logfile"
            exit 1
        fi
        sleep "$sleep_interval"
    done

    err "$name failed to start. Log:"
    tail -20 "$logfile"
    exit 1
}

# start_web_dev_server — starts vite, writes PID file, waits for ready
start_web_dev_server() {
    log "Starting web dev server on port $WEB_PORT..."
    local node_bin
    node_bin=$(command -v node 2>/dev/null || command -v node.exe 2>/dev/null) || { err "node not found in PATH"; exit 1; }
    (cd "$WEB_DIR" && exec "$node_bin" node_modules/vite/bin/vite.js dev --port "$WEB_PORT" --strictPort) &>"$DATA_DIR/web.log" &
    web_pid=$!
    echo "$web_pid" > "$WEB_PID_FILE"

    wait_for_health "http://localhost:${WEB_PORT}" "$web_pid" "Web dev server (port $WEB_PORT)" "$DATA_DIR/web.log" 75 0.2
}

# ─── collect all descendant PIDs of a process ────────────────────────

descendants() {
    local pid=$1
    local children
    children=$(pgrep -P "$pid" 2>/dev/null) || true
    for child in $children; do
        echo "$child"
        descendants "$child"
    done
}

# ─── gracefully stop a process tree: SIGINT → wait → SIGTERM → wait → SIGKILL ───
# Signals ALL descendants simultaneously so the WSL2 /init bridge process
# receives SIGINT and forwards it to Windows node.exe before dying.
graceful_stop() {
    local pid=$1
    local name=${2:-process}
    local all_pids="$pid $(descendants "$pid")"

    # shellcheck disable=SC2086
    kill -INT $all_pids 2>/dev/null || true
    for _ in $(seq 1 20); do
        kill -0 "$pid" 2>/dev/null || { ok "Stopped $name (PID $pid)"; return 0; }
        sleep 0.1
    done

    # shellcheck disable=SC2086
    kill -TERM $all_pids 2>/dev/null || true
    for _ in $(seq 1 10); do
        kill -0 "$pid" 2>/dev/null || { ok "Stopped $name (PID $pid)"; return 0; }
        sleep 0.1
    done

    # shellcheck disable=SC2086
    kill -9 $all_pids 2>/dev/null || true
    ok "Force-killed $name (PID $pid)"
}

# ─── kill running processes ──────────────────────────────────────────

do_kill() {
    # Kill sctl server
    if [[ -f "$PID_FILE" ]]; then
        pid=$(cat "$PID_FILE")
        if kill -0 "$pid" 2>/dev/null; then
            graceful_stop "$pid" "sctl"
        fi
        rm -f "$PID_FILE"
    else
        pkill -INT -f "sctl.*serve" 2>/dev/null && ok "Stopped sctl" || true
    fi

    # Kill web dev server
    if [[ -f "$WEB_PID_FILE" ]]; then
        pid=$(cat "$WEB_PID_FILE")
        if kill -0 "$pid" 2>/dev/null; then
            graceful_stop "$pid" "web dev server"
        fi
        rm -f "$WEB_PID_FILE"
    fi

    # Kill relay
    if [[ -f "$RELAY_PID_FILE" ]]; then
        pid=$(cat "$RELAY_PID_FILE")
        if kill -0 "$pid" 2>/dev/null; then
            graceful_stop "$pid" "relay"
        fi
        rm -f "$RELAY_PID_FILE"
    fi

    # Kill any lingering mcp-sctl processes
    pkill -INT -f "mcp-sctl" 2>/dev/null && ok "Stopped mcp-sctl" || true
}

# ─── stop ────────────────────────────────────────────────────────────

do_stop() {
    log "Stopping..."

    # Deregister MCP from Claude Code
    if claude mcp get "$MCP_NAME" &>/dev/null; then
        claude mcp remove "$MCP_NAME" 2>/dev/null && ok "Removed MCP server '$MCP_NAME' from Claude Code" || true
    fi

    do_kill
}

# ─── status ──────────────────────────────────────────────────────────

do_status() {
    echo "--- sctl server ---"
    if [[ -f "$PID_FILE" ]] && kill -0 "$(cat "$PID_FILE")" 2>/dev/null; then
        echo "  Running (PID $(cat "$PID_FILE")), listening on $LISTEN"
        if curl -sf "http://${LISTEN}/api/health" >/dev/null 2>&1; then
            echo "  Health: OK"
        else
            echo "  Health: not responding"
        fi
    else
        echo "  Not running"
    fi

    echo ""
    echo "--- web UI ---"
    if [[ -f "$WEB_PID_FILE" ]] && kill -0 "$(cat "$WEB_PID_FILE")" 2>/dev/null; then
        echo "  Running (PID $(cat "$WEB_PID_FILE")), port $WEB_PORT"
        if curl -sf "http://localhost:${WEB_PORT}" >/dev/null 2>&1; then
            echo "  Health: OK"
        else
            echo "  Health: not responding"
        fi
    else
        echo "  Not running"
    fi

    echo ""
    echo "--- tunnel relay ---"
    if [[ -f "$RELAY_PID_FILE" ]] && kill -0 "$(cat "$RELAY_PID_FILE")" 2>/dev/null; then
        echo "  Running (PID $(cat "$RELAY_PID_FILE")), listening on $RELAY_LISTEN"
        if curl -sf "http://${RELAY_LISTEN}/api/health" >/dev/null 2>&1; then
            echo "  Health: OK"
            # Show connected devices
            local devices
            devices=$(curl -sf "http://${RELAY_LISTEN}/api/tunnel/devices?token=$TUNNEL_KEY" 2>/dev/null) || true
            if [[ -n "$devices" ]]; then
                echo "  Devices: $devices"
            fi
        else
            echo "  Health: not responding"
        fi
    else
        echo "  Not running"
    fi

    echo ""
    echo "--- mcp-sctl ---"
    if claude mcp get "$MCP_NAME" 2>/dev/null; then
        echo "  Registered in Claude Code"
    else
        echo "  Not registered"
    fi
}

# ─── launch (shared: stop existing, start all services, register MCP) ─

do_launch() {
    # Create data dir and playbooks dir
    mkdir -p "$DATA_DIR"
    mkdir -p "$PLAYBOOKS_DIR"
    mkdir -p "$(dirname "$CONFIG_FILE")"

    # Merge local dev device into persistent config (preserves manually-added devices)
    if [[ -f "$CONFIG_FILE" ]] && command -v jq &>/dev/null; then
        jq --arg url "$DEVICE_URL" --arg key "$API_KEY" --arg pb "$PLAYBOOKS_DIR" \
            '.devices.local = {url: $url, api_key: $key, playbooks_dir: $pb} | .default_device = "local" | .config_version = 2' \
            "$CONFIG_FILE" > "$CONFIG_FILE.tmp" && mv "$CONFIG_FILE.tmp" "$CONFIG_FILE"
        ok "Config updated (merged dev device): $CONFIG_FILE"
    else
        cat > "$CONFIG_FILE" <<EOF
{
  "config_version": 2,
  "devices": {
    "local": {
      "url": "$DEVICE_URL",
      "api_key": "$API_KEY",
      "playbooks_dir": "$PLAYBOOKS_DIR"
    }
  },
  "default_device": "local"
}
EOF
        ok "Config created: $CONFIG_FILE"
    fi

    # Stop any running instances (clean slate)
    do_kill

    # Start sctl server
    log "Starting sctl on $LISTEN..."
    SCTL_API_KEY="$API_KEY" \
    SCTL_LISTEN="$LISTEN" \
    SCTL_DATA_DIR="$DATA_DIR" \
    RUST_LOG=info \
        "$SCTL_BIN" serve &>"$DATA_DIR/sctl.log" &
    sctl_pid=$!
    echo "$sctl_pid" > "$PID_FILE"

    wait_for_health "http://${LISTEN}/api/health" "$sctl_pid" "sctl on $LISTEN" "$DATA_DIR/sctl.log"

    # Start web dev server
    start_web_dev_server

    # Register MCP server with Claude Code
    log "Registering mcp-sctl with Claude Code..."
    claude mcp remove "$MCP_NAME" 2>/dev/null || true
    claude mcp add --transport stdio \
        "$MCP_NAME" -- "$MCP_BIN" --config "$CONFIG_FILE"
    ok "MCP server '$MCP_NAME' registered"

    echo ""
    echo "============================================"
    ok "Dev environment ready!"
    echo ""
    echo "  sctl:         http://${LISTEN} (PID $sctl_pid)"
    echo "  Web UI:       http://localhost:${WEB_PORT} (PID $web_pid)"
    echo "  MCP server:   $MCP_NAME (stdio, managed by Claude Code)"
    echo "  Config:       $CONFIG_FILE"
    echo ""
    echo "  Restart Claude Code or start a new conversation"
    echo "  to pick up the MCP server. Run /mcp to verify."
    echo ""
    echo "  Press Ctrl+C to stop all services."
    echo "============================================"
    echo ""

    # Stay alive: tail logs and wait for Ctrl+C
    trap 'echo ""; log "Shutting down..."; kill $TAIL_PID 2>/dev/null; do_stop; exit 0' INT TERM
    tail -f "$DATA_DIR/sctl.log" "$DATA_DIR/web.log" &
    TAIL_PID=$!
    wait $TAIL_PID
}

# ─── start (restart without rebuilding) ──────────────────────────────

do_start() {
    if [[ ! -x "$SCTL_BIN" ]]; then
        err "sctl binary not found: $SCTL_BIN"
        err "Run '$0' (without arguments) to build first."
        exit 1
    fi
    if [[ ! -x "$MCP_BIN" ]]; then
        err "mcp-sctl binary not found: $MCP_BIN"
        err "Run '$0' (without arguments) to build first."
        exit 1
    fi
    if [[ ! -d "$WEB_DIR/node_modules" ]]; then
        err "Web dependencies not installed: $WEB_DIR/node_modules"
        err "Run '$0' (without arguments) to build first."
        exit 1
    fi

    do_launch
}

# ─── build (just compile, no start/stop) ─────────────────────────────

do_build() {
    log "Building sctl (release)..."
    (cd "$SCTL_DIR" && cargo build --release -v 2>&1)
    ok "sctl built: $SCTL_BIN"

    log "Building mcp-sctl (release)..."
    (cd "$MCP_DIR" && cargo build --release -v 2>&1)
    ok "mcp-sctl built: $MCP_BIN"

    log "Installing web dependencies..."
    (cd "$WEB_DIR" && npm install 2>&1)
    ok "Web dependencies installed"
}

# ─── claude (register MCP only, no build or start) ───────────────────

do_claude() {
    if [[ ! -x "$MCP_BIN" ]]; then
        err "mcp-sctl binary not found: $MCP_BIN"
        err "Run '$0 build' first."
        exit 1
    fi

    # Merge local dev device into persistent config (preserves manually-added devices)
    mkdir -p "$DATA_DIR" "$PLAYBOOKS_DIR" "$(dirname "$CONFIG_FILE")"
    if [[ -f "$CONFIG_FILE" ]] && command -v jq &>/dev/null; then
        jq --arg url "$DEVICE_URL" --arg key "$API_KEY" --arg pb "$PLAYBOOKS_DIR" \
            '.devices.local = {url: $url, api_key: $key, playbooks_dir: $pb} | .default_device = "local" | .config_version = 2' \
            "$CONFIG_FILE" > "$CONFIG_FILE.tmp" && mv "$CONFIG_FILE.tmp" "$CONFIG_FILE"
        ok "Config updated (merged dev device): $CONFIG_FILE"
    else
        cat > "$CONFIG_FILE" <<EOF
{
  "config_version": 2,
  "devices": {
    "local": {
      "url": "$DEVICE_URL",
      "api_key": "$API_KEY",
      "playbooks_dir": "$PLAYBOOKS_DIR"
    }
  },
  "default_device": "local"
}
EOF
        ok "Config created: $CONFIG_FILE"
    fi

    log "Registering mcp-sctl with Claude Code..."
    claude mcp remove "$MCP_NAME" 2>/dev/null || true
    claude mcp add --transport stdio \
        "$MCP_NAME" -- "$MCP_BIN" --config "$CONFIG_FILE"
    ok "MCP server '$MCP_NAME' registered"
    echo ""
    echo "  Restart Claude Code or start a new conversation"
    echo "  to pick up the MCP server. Run /mcp to verify."
}

# ─── device add ──────────────────────────────────────────────────────

do_device_add() {
    local name="${1:-}" host="${2:-}"
    if [[ -z "$name" || -z "$host" ]]; then
        err "Usage: $0 device add <name> <host>"
        exit 1
    fi

    require_jq
    ensure_config

    if cfg_device_exists "$name"; then
        warn "Device '$name' already exists, will update"
    fi

    log "Probing $host via SSH..."

    # Try key auth first, fall back to interactive
    local ssh_opts="-o ConnectTimeout=5 -o StrictHostKeyChecking=accept-new"
    local ssh_cmd="ssh $ssh_opts"

    # Probe script — ash-compatible (no bash on OpenWrt)
    local probe_script='
ARCH=$(uname -m)
SERIAL=""
API_KEY=""
VERSION=""

# Parse sctl.toml for serial and api_key
if [ -f /etc/sctl/sctl.toml ]; then
    SERIAL=$(grep "^serial" /etc/sctl/sctl.toml | head -1 | sed "s/.*= *\"//" | sed "s/\".*//")
    API_KEY=$(grep "^api_key" /etc/sctl/sctl.toml | head -1 | sed "s/.*= *\"//" | sed "s/\".*//")
fi

# Check if sctl binary exists
INSTALLED="no"
if [ -x /usr/bin/sctl ]; then
    INSTALLED="yes"
    # Try to get version from running instance
    VERSION=$(wget -qO- http://127.0.0.1:1337/api/health 2>/dev/null | sed -n "s/.*\"version\":\"\([^\"]*\)\".*/\1/p") || true
fi

echo "ARCH=$ARCH"
echo "SERIAL=$SERIAL"
echo "API_KEY=$API_KEY"
echo "INSTALLED=$INSTALLED"
echo "VERSION=$VERSION"
'

    local probe_output
    # Try BatchMode first (key auth), fall back to interactive
    if probe_output=$($ssh_cmd -o BatchMode=yes "root@$host" "$probe_script" 2>/dev/null); then
        ok "Connected via key auth"
    elif probe_output=$($ssh_cmd "root@$host" "$probe_script" 2>/dev/null); then
        ok "Connected (interactive auth)"
    else
        err "Failed to SSH to root@$host"
        err "Make sure SSH is accessible and you can log in as root"
        exit 1
    fi

    # Parse probe output
    local arch serial api_key installed version
    arch=$(echo "$probe_output" | grep "^ARCH=" | cut -d= -f2)
    serial=$(echo "$probe_output" | grep "^SERIAL=" | cut -d= -f2)
    api_key=$(echo "$probe_output" | grep "^API_KEY=" | cut -d= -f2)
    installed=$(echo "$probe_output" | grep "^INSTALLED=" | cut -d= -f2)
    version=$(echo "$probe_output" | grep "^VERSION=" | cut -d= -f2)

    if [[ -z "$arch" ]]; then
        err "Failed to detect architecture"
        exit 1
    fi

    local target
    target=$(arch_to_target "$arch")
    if [[ "$target" == "unknown" ]]; then
        warn "Unknown architecture '$arch' — cross-compilation may not work"
    fi

    # Build device URL
    local device_url="http://$host:1337"

    # Save to config
    local now
    now=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

    local jq_filter
    jq_filter=$(cat <<JQEOF
.devices["$name"] = {
    url: \$url,
    api_key: \$api_key,
    host: \$host,
    serial: \$serial,
    arch: \$arch,
    sctl_version: \$version,
    added_at: \$now
}
| if .default_device == null or .default_device == "" then .default_device = "$name" else . end
| .config_version = 2
JQEOF
    )

    jq --arg url "$device_url" \
       --arg api_key "${api_key:-change-me}" \
       --arg host "$host" \
       --arg serial "${serial:-unknown}" \
       --arg arch "$arch" \
       --arg version "${version:-}" \
       --arg now "$now" \
       "$jq_filter" "$CONFIG_FILE" > "$CONFIG_FILE.tmp" && mv "$CONFIG_FILE.tmp" "$CONFIG_FILE"

    echo ""
    ok "Device '$name' added to config"
    echo ""
    echo "  Host:         $host"
    echo "  Architecture: $arch (target: $target)"
    echo "  Serial:       ${serial:-unknown}"
    echo "  API key:      ${api_key:-change-me}"
    echo "  sctl:         ${installed:-no}"
    if [[ -n "$version" ]]; then
        echo "  Version:      $version"
    fi
    echo ""

    if [[ "$installed" != "yes" ]]; then
        echo "  sctl is not installed on this device."
        echo "  Deploy with: $0 device deploy $name"
    fi
}

# ─── device rm ───────────────────────────────────────────────────────

do_device_rm() {
    local name="${1:-}"
    if [[ -z "$name" ]]; then
        err "Usage: $0 device rm <name>"
        exit 1
    fi

    require_jq
    ensure_config

    if ! cfg_device_exists "$name"; then
        err "Device '$name' not found"
        exit 1
    fi

    # Remove device, fix default if needed
    jq --arg name "$name" '
        del(.devices[$name])
        | if .default_device == $name then
            .default_device = (.devices | keys | first // null)
          else . end
    ' "$CONFIG_FILE" > "$CONFIG_FILE.tmp" && mv "$CONFIG_FILE.tmp" "$CONFIG_FILE"

    ok "Device '$name' removed"

    # Warn if it was the default
    local new_default
    new_default=$(cfg_get '.default_device // empty')
    if [[ -n "$new_default" && "$new_default" != "null" ]]; then
        log "Default device is now '$new_default'"
    else
        warn "No devices remaining. Add one with: $0 device add <name> <host>"
    fi
}

# ─── device ls ───────────────────────────────────────────────────────

do_device_ls() {
    require_jq
    ensure_config

    local names
    names=$(cfg_device_names)

    if [[ -z "$names" ]]; then
        echo "No devices configured."
        echo "Add one with: $0 device add <name> <host>"
        return
    fi

    local default_dev
    default_dev=$(cfg_get '.default_device // empty')

    # Print header
    printf "\n   %-12s %-16s %-20s %-8s %-10s %s\n" \
        "NAME" "HOST" "SERIAL" "ARCH" "VERSION" "HEALTH"
    printf "%.0s─" {1..80}
    echo ""

    for name in $names; do
        local host serial arch version url api_key
        host=$(cfg_device_get "$name" "host")
        serial=$(cfg_device_get "$name" "serial")
        arch=$(cfg_device_get "$name" "arch")
        version=$(cfg_device_get "$name" "sctl_version")
        url=$(cfg_device_get "$name" "url")
        api_key=$(cfg_device_get "$name" "api_key")

        # Default marker
        local marker="  "
        if [[ "$name" == "$default_dev" ]]; then
            marker="* "
        fi

        # Health check
        local health health_color
        if [[ -n "$url" ]]; then
            if curl -sf -H "Authorization: Bearer $api_key" "$url/api/health" >/dev/null 2>&1; then
                health="OK"
                health_color="\033[1;32m"  # green
            else
                health="DOWN"
                health_color="\033[1;31m"  # red
            fi
        else
            health="???"
            health_color="\033[1;33m"  # yellow
        fi

        printf "%s %-12s %-16s %-20s %-8s %-10s ${health_color}%s\033[0m\n" \
            "$marker" "$name" "${host:--}" "${serial:--}" "${arch:--}" "${version:--}" "$health"
    done

    echo ""
    echo "  * = default device"
    echo ""
}

# ─── device deploy ───────────────────────────────────────────────────

do_device_deploy() {
    local name="${1:-}"
    if [[ -z "$name" ]]; then
        err "Usage: $0 device deploy <name>"
        exit 1
    fi

    require_jq
    ensure_config

    if ! cfg_device_exists "$name"; then
        err "Device '$name' not found. Add it first with: $0 device add $name <host>"
        exit 1
    fi

    local host arch
    host=$(cfg_device_get "$name" "host")
    arch=$(cfg_device_get "$name" "arch")

    if [[ -z "$host" ]]; then
        err "Device '$name' has no host configured. Re-add with: $0 device add $name <host>"
        exit 1
    fi
    if [[ -z "$arch" ]]; then
        err "Device '$name' has no arch configured. Re-add with: $0 device add $name $host"
        exit 1
    fi

    local target bin_path
    target=$(arch_to_target "$arch")
    bin_path=$(arch_to_bin "$arch")

    # Cross-compile
    if [[ "$target" == "native" ]]; then
        log "Building sctl for $arch (native)..."
        (cd "$SCTL_DIR" && cargo build --release 2>&1)
    else
        log "Building sctl for $arch (cross: $target)..."
        (cd "$SCTL_DIR" && cross build --release --target "$target" 2>&1)
    fi
    ok "Build complete: $bin_path"

    local ssh_opts="-o ConnectTimeout=5 -o StrictHostKeyChecking=accept-new"

    # Upload binary
    log "Uploading sctl binary to $host..."
    scp $ssh_opts "$bin_path" "root@$host:/usr/bin/sctl"
    ok "Binary uploaded"

    # Upload config template if missing
    log "Checking config on device..."
    if ! ssh $ssh_opts "root@$host" "test -f /etc/sctl/sctl.toml" 2>/dev/null; then
        log "No config found, uploading template..."
        ssh $ssh_opts "root@$host" "mkdir -p /etc/sctl"
        scp $ssh_opts "$SCTL_DIR/sctl.toml.example" "root@$host:/etc/sctl/sctl.toml"
        ok "Config template uploaded — edit /etc/sctl/sctl.toml on the device to set serial + api_key"
    else
        ok "Config already exists on device"
    fi

    # Upload init script
    log "Uploading init script..."
    scp $ssh_opts "$SCTL_DIR/files/sctl.init" "root@$host:/etc/init.d/sctl"
    ssh $ssh_opts "root@$host" "sed -i 's/\r//' /etc/init.d/sctl && chmod +x /etc/init.d/sctl && /etc/init.d/sctl enable"
    ok "Init script installed and enabled"

    echo ""
    ok "Deploy complete for '$name' ($host)"
    echo ""
    echo "  Next steps:"
    echo "    1. SSH in and edit /etc/sctl/sctl.toml (set serial, api_key)"
    echo "    2. Start: ssh root@$host /etc/init.d/sctl start"
    echo "    3. Re-probe: $0 device add $name $host"
    echo ""
}

# ─── device upgrade ──────────────────────────────────────────────────

do_device_upgrade() {
    local name="${1:-}"
    if [[ -z "$name" ]]; then
        err "Usage: $0 device upgrade <name>"
        exit 1
    fi

    require_jq
    ensure_config

    if ! cfg_device_exists "$name"; then
        err "Device '$name' not found"
        exit 1
    fi

    local host arch url api_key
    host=$(cfg_device_get "$name" "host")
    arch=$(cfg_device_get "$name" "arch")
    url=$(cfg_device_get "$name" "url")
    api_key=$(cfg_device_get "$name" "api_key")

    if [[ -z "$host" || -z "$arch" ]]; then
        err "Device '$name' missing host or arch. Re-add with: $0 device add $name <host>"
        exit 1
    fi

    local target bin_path
    target=$(arch_to_target "$arch")
    bin_path=$(arch_to_bin "$arch")

    # Cross-compile
    if [[ "$target" == "native" ]]; then
        log "Building sctl for $arch (native)..."
        (cd "$SCTL_DIR" && cargo build --release 2>&1)
    else
        log "Building sctl for $arch (cross: $target)..."
        (cd "$SCTL_DIR" && cross build --release --target "$target" 2>&1)
    fi
    ok "Build complete: $bin_path"

    local ssh_opts="-o ConnectTimeout=5 -o StrictHostKeyChecking=accept-new"

    # Stop, upload, start
    log "Stopping sctl on $host..."
    ssh $ssh_opts "root@$host" "/etc/init.d/sctl stop; rm -f /usr/bin/sctl" 2>/dev/null || true

    log "Uploading new binary..."
    scp $ssh_opts "$bin_path" "root@$host:/usr/bin/sctl"

    log "Starting sctl on $host..."
    ssh $ssh_opts "root@$host" "/etc/init.d/sctl start"

    # Wait for health
    log "Waiting for device to come back up..."
    local healthy=false
    for _ in $(seq 1 30); do
        if curl -sf -H "Authorization: Bearer $api_key" "$url/api/health" >/dev/null 2>&1; then
            healthy=true
            break
        fi
        sleep 0.5
    done

    if [[ "$healthy" == "true" ]]; then
        # Update version in config
        local new_version
        new_version=$(curl -sf -H "Authorization: Bearer $api_key" "$url/api/health" 2>/dev/null \
            | jq -r '.version // empty' 2>/dev/null) || true

        if [[ -n "$new_version" ]]; then
            jq --arg name "$name" --arg ver "$new_version" \
                '.devices[$name].sctl_version = $ver' \
                "$CONFIG_FILE" > "$CONFIG_FILE.tmp" && mv "$CONFIG_FILE.tmp" "$CONFIG_FILE"
        fi

        ok "Upgrade complete for '$name' ($host)"
        if [[ -n "$new_version" ]]; then
            echo "  Version: $new_version"
        fi
    else
        warn "Upgrade deployed but device health check failed"
        warn "Check manually: ssh root@$host /etc/init.d/sctl status"
    fi
}

# ─── tunnel (build + start tunnel dev env with relay) ─────────────────

do_tunnel() {
    do_build

    mkdir -p "$DATA_DIR" "$PLAYBOOKS_DIR"

    require_jq
    ensure_config

    # Backup config — restored on cleanup
    cp "$CONFIG_FILE" "$CONFIG_FILE.pre-tunnel"

    # Collect remote device info for orchestration
    local -a remote_names=()
    local -a remote_hosts=()
    local -a remote_serials=()
    local -a remote_api_keys=()
    local -a remote_pids=()

    for dev_name in $(cfg_device_names); do
        local dev_host
        dev_host=$(cfg_device_get "$dev_name" "host")
        if [[ -n "$dev_host" && "$dev_name" != "local" ]]; then
            remote_names+=("$dev_name")
            remote_hosts+=("$dev_host")
            remote_serials+=("$(cfg_device_get "$dev_name" "serial")")
            remote_api_keys+=("$(cfg_device_get "$dev_name" "api_key")")
        fi
    done

    # Generate relay TOML config
    cat > "$DATA_DIR/relay.toml" <<EOF
[server]
listen = "$RELAY_LISTEN"
journal_enabled = false

[auth]
api_key = "unused"

[device]
serial = "RELAY"

[tunnel]
relay = true
tunnel_key = "$TUNNEL_KEY"
EOF

    # Generate tunnel client TOML config
    cat > "$DATA_DIR/client.toml" <<EOF
[server]
listen = "$LISTEN"
data_dir = "$DATA_DIR"

[auth]
api_key = "$API_KEY"

[device]
serial = "$DEVICE_SERIAL"

[tunnel]
tunnel_key = "$TUNNEL_KEY"
url = "ws://127.0.0.1:8443/api/tunnel/register"
EOF

    # Stop any running instances
    do_kill

    # Start relay
    log "Starting relay on $RELAY_LISTEN..."
    "$SCTL_BIN" serve --config "$DATA_DIR/relay.toml" &>"$DATA_DIR/relay.log" &
    relay_pid=$!
    echo "$relay_pid" > "$RELAY_PID_FILE"

    wait_for_health "http://127.0.0.1:8443/api/health" "$relay_pid" "Relay on $RELAY_LISTEN" "$DATA_DIR/relay.log"

    # Start local sctl as tunnel client
    log "Starting sctl (tunnel client) on $LISTEN..."
    RUST_LOG=info \
        "$SCTL_BIN" serve --config "$DATA_DIR/client.toml" &>"$DATA_DIR/sctl.log" &
    sctl_pid=$!
    echo "$sctl_pid" > "$PID_FILE"

    wait_for_health "http://${LISTEN}/api/health" "$sctl_pid" "sctl on $LISTEN" "$DATA_DIR/sctl.log"

    # Wait for local device to register with relay
    log "Waiting for local tunnel registration..."
    local registered=false
    for _ in $(seq 1 30); do
        local devices
        devices=$(curl -sf "http://127.0.0.1:8443/api/tunnel/devices?token=$TUNNEL_KEY" 2>/dev/null) || true
        if echo "$devices" | grep -q "$DEVICE_SERIAL" 2>/dev/null; then
            ok "Local device $DEVICE_SERIAL registered with relay"
            registered=true
            break
        fi
        sleep 0.2
    done

    if [[ "$registered" != "true" ]]; then
        err "Local device failed to register with relay within 6s"
        echo "  Relay log:"
        tail -10 "$DATA_DIR/relay.log"
        echo "  Client log:"
        tail -10 "$DATA_DIR/sctl.log"
        # Restore config before exit
        mv "$CONFIG_FILE.pre-tunnel" "$CONFIG_FILE"
        exit 1
    fi

    # Connect remote devices via SSH tunnel
    if [[ ${#remote_names[@]} -gt 0 ]]; then
        log "Connecting ${#remote_names[@]} remote device(s) via tunnel..."

        local ssh_opts="-o ConnectTimeout=5 -o StrictHostKeyChecking=accept-new -o BatchMode=yes"

        for i in "${!remote_names[@]}"; do
            local rname="${remote_names[$i]}"
            local rhost="${remote_hosts[$i]}"

            # Detect our LAN IP toward this device
            local our_ip
            our_ip=$(ip route get "$rhost" 2>/dev/null | grep -oP 'src \K\S+' || true)
            if [[ -z "$our_ip" ]]; then
                warn "Cannot determine route to $rhost — skipping $rname"
                continue
            fi

            log "  $rname ($rhost) — relay via $our_ip:8443"

            # SSH in: stop init.d, create temp config, start sctl with tunnel
            local remote_script
            remote_script=$(cat <<REOF
# Stop normal sctl
/etc/init.d/sctl stop 2>/dev/null || true

# Copy config as base
cp /etc/sctl/sctl.toml /tmp/sctl-relay.toml 2>/dev/null || true

# Strip any existing [tunnel] section and everything after it
sed -i '/^\[tunnel\]/,\$d' /tmp/sctl-relay.toml 2>/dev/null || true

# Override listen to avoid conflict (pick a random high port)
sed -i 's/^listen = .*/listen = "127.0.0.1:0"/' /tmp/sctl-relay.toml 2>/dev/null || true

# Append tunnel config
cat >> /tmp/sctl-relay.toml <<TEOF

[tunnel]
tunnel_key = "$TUNNEL_KEY"
url = "ws://$our_ip:8443/api/tunnel/register"
TEOF

# Start sctl with tunnel config
nohup /usr/bin/sctl serve --config /tmp/sctl-relay.toml >/tmp/sctl-relay.log 2>&1 &
echo \$!
REOF
            )

            local rpid
            rpid=$(ssh $ssh_opts "root@$rhost" "$remote_script" 2>/dev/null) || {
                warn "  Failed to start tunnel on $rname ($rhost)"
                continue
            }

            remote_pids+=("$rpid")
            ok "  $rname: tunnel process started (remote PID $rpid)"
        done

        # Wait for all remote devices to register
        if [[ ${#remote_serials[@]} -gt 0 ]]; then
            log "Waiting for remote devices to register..."
            local all_registered=true
            for _ in $(seq 1 60); do
                local devs
                devs=$(curl -sf "http://127.0.0.1:8443/api/tunnel/devices?token=$TUNNEL_KEY" 2>/dev/null) || true
                all_registered=true
                for serial in "${remote_serials[@]}"; do
                    if [[ -n "$serial" && "$serial" != "unknown" ]] && ! echo "$devs" | grep -q "$serial" 2>/dev/null; then
                        all_registered=false
                        break
                    fi
                done
                if [[ "$all_registered" == "true" ]]; then
                    break
                fi
                sleep 0.5
            done

            if [[ "$all_registered" == "true" ]]; then
                ok "All remote devices registered"
            else
                warn "Some remote devices did not register within 30s (continuing anyway)"
            fi
        fi
    fi

    # Rewrite MCP config with all devices routed via relay
    log "Updating MCP config for relay routing..."
    local relay_config='{"config_version":2,"devices":{},"default_device":"local"}'

    # Add local device via relay
    relay_config=$(echo "$relay_config" | jq \
        --arg url "http://127.0.0.1:8443/d/$DEVICE_SERIAL" \
        --arg key "$API_KEY" \
        --arg pb "$PLAYBOOKS_DIR" \
        '.devices.local = {url: $url, api_key: $key, playbooks_dir: $pb}')

    # Add remote devices via relay
    for i in "${!remote_names[@]}"; do
        local rname="${remote_names[$i]}"
        local rserial="${remote_serials[$i]}"
        local rapi_key="${remote_api_keys[$i]}"

        if [[ -n "$rserial" && "$rserial" != "unknown" ]]; then
            relay_config=$(echo "$relay_config" | jq \
                --arg name "$rname" \
                --arg url "http://127.0.0.1:8443/d/$rserial" \
                --arg key "$rapi_key" \
                '.devices[$name] = {url: $url, api_key: $key}')
        fi
    done

    echo "$relay_config" | jq '.' > "$CONFIG_FILE"
    ok "MCP config updated for relay routing"

    # Start web dev server
    start_web_dev_server

    # Register MCP server with Claude Code (via relay)
    log "Registering mcp-sctl with Claude Code (via relay)..."
    claude mcp remove "$MCP_NAME" 2>/dev/null || true
    claude mcp add --transport stdio \
        "$MCP_NAME" -- "$MCP_BIN" --config "$CONFIG_FILE"
    ok "MCP server '$MCP_NAME' registered"

    echo ""
    echo "============================================"
    ok "Tunnel dev environment ready!"
    echo ""
    echo "  Relay:        http://127.0.0.1:8443 (PID $relay_pid)"
    echo "  Local sctl:   http://$LISTEN (PID $sctl_pid, tunnel client)"
    echo "  Device URL:   http://127.0.0.1:8443/d/$DEVICE_SERIAL"
    if [[ ${#remote_names[@]} -gt 0 ]]; then
        echo ""
        echo "  Remote devices:"
        for i in "${!remote_names[@]}"; do
            echo "    ${remote_names[$i]}: http://127.0.0.1:8443/d/${remote_serials[$i]}"
        done
    fi
    echo ""
    echo "  Web UI:       http://localhost:$WEB_PORT (PID $web_pid)"
    echo "  MCP server:   $MCP_NAME (stdio, routed via tunnel relay)"
    echo "  Tunnel key:   $TUNNEL_KEY"
    echo ""
    echo "  Traffic flow: client -> relay -> tunnel -> sctl device"
    echo ""
    echo "  Restart Claude Code or start a new conversation"
    echo "  to pick up the MCP server. Run /mcp to verify."
    echo ""
    echo "  Press Ctrl+C to stop all services."
    echo "============================================"
    echo ""

    # Cleanup function for tunnel mode
    tunnel_cleanup() {
        echo ""
        log "Shutting down tunnel environment..."

        # Kill log tail
        kill $TAIL_PID 2>/dev/null || true

        # Stop remote devices (restore normal operation)
        if [[ ${#remote_names[@]} -gt 0 ]]; then
            log "Restoring remote devices..."
            local ssh_opts="-o ConnectTimeout=5 -o StrictHostKeyChecking=accept-new -o BatchMode=yes"
            for i in "${!remote_names[@]}"; do
                local rname="${remote_names[$i]}"
                local rhost="${remote_hosts[$i]}"
                local rpid="${remote_pids[$i]:-}"
                (
                    # Kill temp sctl process
                    if [[ -n "$rpid" ]]; then
                        ssh $ssh_opts "root@$rhost" "kill $rpid 2>/dev/null; rm -f /tmp/sctl-relay.toml /tmp/sctl-relay.log" 2>/dev/null || true
                    fi
                    # Restart normal sctl
                    ssh $ssh_opts "root@$rhost" "/etc/init.d/sctl start" 2>/dev/null || true
                    ok "  $rname: restored"
                ) &
            done
            wait
        fi

        # Restore config from backup
        if [[ -f "$CONFIG_FILE.pre-tunnel" ]]; then
            mv "$CONFIG_FILE.pre-tunnel" "$CONFIG_FILE"
            ok "Config restored from backup"
        fi

        # Stop local services + deregister MCP
        do_stop
        exit 0
    }

    trap tunnel_cleanup INT TERM

    # Tail logs and wait
    tail -f "$DATA_DIR/relay.log" "$DATA_DIR/sctl.log" "$DATA_DIR/web.log" &
    TAIL_PID=$!
    wait $TAIL_PID
}

# ─── setup (default: build + start) ─────────────────────────────────

do_setup() {
    do_build
    do_launch
}

# ─── main ────────────────────────────────────────────────────────────

case "${1:-setup}" in
    setup)  do_setup ;;
    build)  do_build ;;
    start)  do_start ;;
    stop)   do_stop ;;
    status) do_status ;;
    claude) do_claude ;;
    tunnel) do_tunnel ;;
    device)
        case "${2:-ls}" in
            add)     do_device_add "$3" "${4:-}" ;;
            rm)      do_device_rm "${3:-}" ;;
            ls)      do_device_ls ;;
            deploy)  do_device_deploy "${3:-}" ;;
            upgrade) do_device_upgrade "${3:-}" ;;
            *)
                echo "Usage: $0 device <command>"
                echo ""
                echo "Commands:"
                echo "  ls                  list devices with health status (default)"
                echo "  add <name> <host>   discover + register a device via SSH"
                echo "  rm <name>           remove a device"
                echo "  deploy <name>       full deploy (binary + config + init script)"
                echo "  upgrade <name>      binary-only upgrade (stop → upload → start)"
                exit 1
                ;;
        esac
        ;;
    *)
        echo "Usage: $0 <command>"
        echo ""
        echo "Dev stack:"
        echo "  setup    build everything + start all services + register MCP (default)"
        echo "  build    build only (server, mcp, web) — no start/stop"
        echo "  start    restart all services without rebuilding"
        echo "  stop     stop all services + deregister MCP"
        echo "  status   show what's running"
        echo "  claude   only register MCP in Claude Code (no build/start)"
        echo "  tunnel   build + start tunnel dev env (relay + clients via tunnel)"
        echo ""
        echo "Device management:"
        echo "  device ls                  list devices with health status"
        echo "  device add <name> <host>   discover + register a device via SSH"
        echo "  device rm <name>           remove a device"
        echo "  device deploy <name>       full deploy (binary + config + init script)"
        echo "  device upgrade <name>      binary-only upgrade"
        exit 1
        ;;
esac
