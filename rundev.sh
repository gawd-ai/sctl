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
#   ./rundev.sh tunnel [--cloudflared | --relay-url <url>]
#                        # build + start tunnel dev env (relay + clients + MCP via relay)
#
# Device management:
#   ./rundev.sh device add <name> <host>        # discover + register a device
#   ./rundev.sh device rm <name>                # remove a device
#   ./rundev.sh device ls                       # list devices with health status
#   ./rundev.sh device deploy <name>            # full deploy (binary + config + init)
#   ./rundev.sh device upgrade <name>           # binary-only upgrade via SSH
#   ./rundev.sh device deploy-watchdog <name>   # deploy watchdog + cron (SSH or API)
#   ./rundev.sh device upgrade-remote <name>    # binary upgrade via relay (no SSH)
#
# Relay VPS deployment:
#   ./rundev.sh relay setup <user@host>   # full VPS provisioning (Caddy + sctl + firewall)
#   ./rundev.sh relay deploy [user@host]  # deploy binary + service (preserves config)
#   ./rundev.sh relay upgrade [user@host] # binary-only upgrade
#   ./rundev.sh relay status [user@host]  # health check + connected devices
#
# Playbook library:
#   ./rundev.sh playbook ls                          # list playbooks in library
#   ./rundev.sh playbook deploy <device|all> [cat]   # deploy playbooks to device(s)
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
CLOUDFLARED_PID_FILE="$DATA_DIR/cloudflared.pid"

# Relay VPS deployment
RELAY_X86_BIN="$SCTL_DIR/target/x86_64-unknown-linux-musl/release/sctl"
RELAY_REMOTE_BIN="/usr/local/bin/sctl"
RELAY_REMOTE_CONFIG="/etc/sctl/relay.toml"

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

# Check if mcp-sctl supervisor is running (spawned by Claude Code).
# Returns 0 if alive — callers should skip killing/re-registering MCP.
is_mcp_alive() {
    pgrep -f "mcp-sctl.*--supervisor" &>/dev/null
}

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

    # Kill cloudflared
    if [[ -f "$CLOUDFLARED_PID_FILE" ]]; then
        pid=$(cat "$CLOUDFLARED_PID_FILE")
        if kill -0 "$pid" 2>/dev/null; then
            graceful_stop "$pid" "cloudflared"
        fi
        rm -f "$CLOUDFLARED_PID_FILE"
    fi

    # Kill mcp-sctl only if not managed by Claude Code
    if is_mcp_alive; then
        ok "mcp-sctl supervisor alive (managed by Claude Code) — leaving it running"
    else
        pkill -INT -f "mcp-sctl" 2>/dev/null && ok "Stopped mcp-sctl" || true
    fi
}

# ─── stop ────────────────────────────────────────────────────────────

do_stop() {
    log "Stopping..."

    # Deregister MCP only if not managed by a live Claude session
    if is_mcp_alive; then
        ok "mcp-sctl managed by Claude Code — skipping deregister"
    elif claude mcp get "$MCP_NAME" &>/dev/null; then
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

# ─── sctlin seed ─────────────────────────────────────────────────
# Generate web/static/sctlin-seed.json from MCP config so sctlin
# auto-discovers devices without manual entry.

generate_sctlin_seed() {
    require_jq
    local seed_file="$WEB_DIR/static/sctlin-seed.json"

    # In tunnel mode the config is rewritten with relay URLs — use the
    # pre-tunnel backup so we get the real direct URLs for each device.
    local source_config="$CONFIG_FILE"
    if [[ -f "$CONFIG_FILE.pre-tunnel" ]]; then
        source_config="$CONFIG_FILE.pre-tunnel"
    fi

    # Build serial map: device name → serial (from config metadata or known defaults)
    # The relay proxy path is /d/{serial}/api/ws
    local serial_map="{\"local\": \"$DEVICE_SERIAL\"}"
    # Add serials from device metadata in config
    serial_map=$(jq -r --argjson base "$serial_map" '
        [.devices | to_entries[] | select(.value.serial) | {(.key): .value.serial}]
        | reduce .[] as $item ($base; . + $item)
    ' "$source_config")

    # Convert MCP device config → sctlin server entries (direct + local-relay)
    # Skip generating a localhost relay entry when:
    #   - the device URL already contains /d/ (it IS a relay entry), OR
    #   - a device named "{name}-relay" already exists in the config
    local all_device_names
    all_device_names=$(jq -r '[.devices | keys[]] | @json' "$source_config")

    jq --argjson serials "$serial_map" --argjson names "$all_device_names" '[
        .devices | to_entries[] |
        # Direct entry
        {
            id: .key,
            name: .key,
            wsUrl: (.value.url | sub("^https://"; "wss://") | sub("^http://"; "ws://") | . + "/api/ws"),
            apiKey: .value.api_key,
            shell: "",
            connected: false
        },
        # Local dev relay entry (skip if URL contains /d/ or {name}-relay already exists)
        if ($serials[.key] and (.value.url | test("/d/") | not) and ((.key + "-relay") as $rk | ($names | index($rk)) | not)) then {
            id: (.key + "-relay"),
            name: (.key + " (relay)"),
            wsUrl: ("ws://127.0.0.1:8443/d/" + $serials[.key] + "/api/ws"),
            apiKey: .value.api_key,
            shell: "",
            connected: false
        } else empty end
    ]' "$source_config" > "$seed_file"

    ok "sctlin seed generated: $seed_file ($(jq length "$seed_file") servers)"
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

    generate_sctlin_seed

    # Stop any running instances (clean slate)
    do_kill

    # Start sctl server
    log "Starting sctl on $LISTEN..."
    SCTL_API_KEY="$API_KEY" \
    SCTL_LISTEN="$LISTEN" \
    SCTL_DATA_DIR="$DATA_DIR" \
    SCTL_PLAYBOOKS_DIR="$PLAYBOOKS_DIR" \
    RUST_LOG=info \
        "$SCTL_BIN" serve &>"$DATA_DIR/sctl.log" &
    sctl_pid=$!
    echo "$sctl_pid" > "$PID_FILE"

    wait_for_health "http://${LISTEN}/api/health" "$sctl_pid" "sctl on $LISTEN" "$DATA_DIR/sctl.log"

    # Start web dev server
    start_web_dev_server

    # Register MCP server with Claude Code (skip if already managed)
    if is_mcp_alive; then
        ok "mcp-sctl already running (managed by Claude Code) — config hot-reload will pick up changes"
    else
        log "Registering mcp-sctl with Claude Code..."
        claude mcp remove "$MCP_NAME" 2>/dev/null || true
        claude mcp add --transport stdio \
            "$MCP_NAME" -- "$MCP_BIN" --supervisor --config "$CONFIG_FILE"
        ok "MCP server '$MCP_NAME' registered (supervisor mode)"
    fi

    echo ""
    echo "============================================"
    ok "Dev environment ready!"
    echo ""
    echo "  sctl:         http://${LISTEN} (PID $sctl_pid)"
    echo "  Web UI:       http://localhost:${WEB_PORT} (PID $web_pid)"
    echo "  MCP server:   $MCP_NAME (stdio, managed by Claude Code)"
    echo "  Config:       $CONFIG_FILE"
    echo ""
    if is_mcp_alive; then
        echo "  MCP is live — config changes picked up automatically."
    else
        echo "  Restart Claude Code or start a new conversation"
        echo "  to pick up the MCP server. Run /mcp to verify."
    fi
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

    # Signal supervisor to hot-reload if running (binary watcher will also detect it)
    pkill -USR1 -f "mcp-sctl.*--supervisor" 2>/dev/null && ok "Sent SIGUSR1 to mcp-sctl supervisor" || true

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

    generate_sctlin_seed

    if is_mcp_alive; then
        ok "mcp-sctl already running (managed by Claude Code) — config hot-reload will pick up changes"
    else
        log "Registering mcp-sctl with Claude Code..."
        claude mcp remove "$MCP_NAME" 2>/dev/null || true
        claude mcp add --transport stdio \
            "$MCP_NAME" -- "$MCP_BIN" --supervisor --config "$CONFIG_FILE"
        ok "MCP server '$MCP_NAME' registered (supervisor mode)"
        echo ""
        echo "  Restart Claude Code or start a new conversation"
        echo "  to pick up the MCP server. Run /mcp to verify."
    fi
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

# ─── device deploy-watchdog ──────────────────────────────────────────

# Watchdog script content — ash-compatible, deployed to /etc/sctl/watchdog.sh.
# Runs every 2 minutes via cron. Ensures sctl stays running and handles
# rollback if a bad binary is deployed.
WATCHDOG_SCRIPT='#!/bin/sh
# sctl watchdog — keeps sctl running, handles rollback on failed upgrades.
# Deployed by: rundev.sh device deploy-watchdog
# Schedule: */2 * * * * /etc/sctl/watchdog.sh

SCTL_BIN="/usr/bin/sctl"
ROLLBACK_BIN="/usr/bin/sctl.rollback"
FAIL_FILE="/tmp/sctl-watchdog-fails"
HEALTH_URL="http://127.0.0.1:1337/api/health"
LOG_TAG="sctl-watchdog"
MAX_FAILS=3

log() { logger -t "$LOG_TAG" "$1"; }

# Count consecutive failures
get_fails() {
    if [ -f "$FAIL_FILE" ]; then
        cat "$FAIL_FILE"
    else
        echo 0
    fi
}
set_fails() { echo "$1" > "$FAIL_FILE"; }

# Check if sctl process is running
is_running() {
    pgrep -f "sctl.*(serve|supervise)" >/dev/null 2>&1
}

# Health check via wget (BusyBox)
is_healthy() {
    wget -qO /dev/null -T 5 "$HEALTH_URL" 2>/dev/null
}

# ── Main logic ──

# 1. If sctl not running, start it
if ! is_running; then
    log "sctl not running, starting..."
    /etc/init.d/sctl start
    sleep 3
fi

# 2. Health check
if is_healthy; then
    # Healthy — reset fail counter
    if [ "$(get_fails)" -gt 0 ]; then
        log "sctl healthy again, resetting fail counter"
        set_fails 0
    fi

    # If rollback binary exists for >10 min, upgrade is confirmed — clean up
    if [ -f "$ROLLBACK_BIN" ]; then
        rollback_age=$(( $(date +%s) - $(date -r "$ROLLBACK_BIN" +%s 2>/dev/null || echo 0) ))
        if [ "$rollback_age" -gt 600 ]; then
            log "upgrade confirmed (healthy for >10min), removing rollback binary"
            rm -f "$ROLLBACK_BIN"
        fi
    fi
else
    # Unhealthy — increment fail counter
    fails=$(get_fails)
    fails=$((fails + 1))
    set_fails "$fails"
    log "health check failed ($fails/$MAX_FAILS)"

    if [ "$fails" -ge "$MAX_FAILS" ]; then
        if [ -f "$ROLLBACK_BIN" ]; then
            # Rollback to previous binary
            log "ROLLBACK: swapping to previous binary after $fails failures"
            cp "$ROLLBACK_BIN" "$SCTL_BIN"
            rm -f "$ROLLBACK_BIN"
            /etc/init.d/sctl restart
            set_fails 0
        else
            # No rollback available — just restart
            log "restarting sctl after $fails failures (no rollback available)"
            /etc/init.d/sctl restart
            set_fails 0
        fi
    fi
fi
'

do_device_deploy_watchdog() {
    local name="${1:-}"
    if [[ -z "$name" ]]; then
        err "Usage: $0 device deploy-watchdog <name>"
        exit 1
    fi

    require_jq
    ensure_config

    if ! cfg_device_exists "$name"; then
        err "Device '$name' not found"
        exit 1
    fi

    local host url api_key
    host=$(cfg_device_get "$name" "host")
    url=$(cfg_device_get "$name" "url")
    api_key=$(cfg_device_get "$name" "api_key")

    # Determine transport: SSH (if host reachable) or API (via relay)
    local use_ssh=false
    if [[ -n "$host" ]]; then
        if ssh -o ConnectTimeout=3 -o BatchMode=yes "root@$host" true 2>/dev/null; then
            use_ssh=true
        fi
    fi

    if [[ "$use_ssh" == "true" ]]; then
        log "Deploying watchdog to '$name' via SSH ($host)..."

        local ssh_opts="-o ConnectTimeout=5 -o StrictHostKeyChecking=accept-new"

        # Write watchdog script
        echo "$WATCHDOG_SCRIPT" | ssh $ssh_opts "root@$host" "cat > /etc/sctl/watchdog.sh && chmod +x /etc/sctl/watchdog.sh"
        ok "Watchdog script deployed"

        # Add cron entry (idempotent)
        ssh $ssh_opts "root@$host" '
            CRON_ENTRY="*/2 * * * * /etc/sctl/watchdog.sh"
            if ! crontab -l 2>/dev/null | grep -qF "sctl/watchdog.sh"; then
                (crontab -l 2>/dev/null; echo "$CRON_ENTRY") | crontab -
            fi
            /etc/init.d/cron restart 2>/dev/null || true
        '
        ok "Cron entry added (every 2 minutes)"
    elif [[ -n "$url" && -n "$api_key" ]]; then
        log "Deploying watchdog to '$name' via API ($url)..."

        # Write watchdog script via file API
        local escaped_script
        escaped_script=$(printf '%s' "$WATCHDOG_SCRIPT" | jq -Rs .)
        curl -sf -X PUT "$url/api/files?path=/etc/sctl/watchdog.sh" \
            -H "Authorization: Bearer $api_key" \
            -H "Content-Type: application/json" \
            -d "{\"content\": $escaped_script, \"mode\": \"0755\"}" >/dev/null
        ok "Watchdog script deployed"

        # Add cron entry via exec
        curl -sf -X POST "$url/api/exec" \
            -H "Authorization: Bearer $api_key" \
            -H "Content-Type: application/json" \
            -d '{"command": "if ! crontab -l 2>/dev/null | grep -qF \"sctl/watchdog.sh\"; then (crontab -l 2>/dev/null; echo \"*/2 * * * * /etc/sctl/watchdog.sh\") | crontab -; fi; /etc/init.d/cron restart 2>/dev/null || true"}' >/dev/null
        ok "Cron entry added (every 2 minutes)"
    else
        err "No SSH access and no API URL for device '$name'"
        exit 1
    fi

    echo ""
    ok "Watchdog deployed to '$name'"
    echo "  Script: /etc/sctl/watchdog.sh"
    echo "  Schedule: every 2 minutes via cron"
    echo "  Rollback: swaps to /usr/bin/sctl.rollback after 3 failed health checks"
}

# ─── device upgrade-remote ───────────────────────────────────────────

do_device_upgrade_remote() {
    local name="${1:-}"
    if [[ -z "$name" ]]; then
        err "Usage: $0 device upgrade-remote <name>"
        exit 1
    fi

    require_jq
    ensure_config

    if ! cfg_device_exists "$name"; then
        err "Device '$name' not found"
        exit 1
    fi

    local arch url api_key serial
    arch=$(cfg_device_get "$name" "arch")
    url=$(cfg_device_get "$name" "url")
    api_key=$(cfg_device_get "$name" "api_key")
    serial=$(cfg_device_get "$name" "serial")

    if [[ -z "$url" || -z "$api_key" ]]; then
        err "Device '$name' missing url or api_key in config"
        exit 1
    fi
    if [[ -z "$arch" ]]; then
        err "Device '$name' has no arch configured"
        exit 1
    fi

    # Step 1: Cross-compile
    local target bin_path
    target=$(arch_to_target "$arch")
    bin_path=$(arch_to_bin "$arch")

    if [[ "$target" == "native" ]]; then
        log "Building sctl for $arch (native)..."
        (cd "$SCTL_DIR" && cargo build --release 2>&1)
    else
        log "Building sctl for $arch (cross: $target)..."
        (cd "$SCTL_DIR" && cross build --release --target "$target" 2>&1)
    fi
    ok "Build complete: $bin_path"

    local file_size chunk_size total_chunks
    file_size=$(stat -c%s "$bin_path")
    chunk_size=262144  # 256 KiB
    total_chunks=$(( (file_size + chunk_size - 1) / chunk_size ))

    log "Binary: $bin_path ($file_size bytes, $total_chunks chunks)"

    # Step 2: Init upload via STP
    log "Initiating upload to $url..."
    local init_resp
    init_resp=$(curl -sf -X POST "$url/api/stp/upload" \
        -H "Authorization: Bearer $api_key" \
        -H "Content-Type: application/json" \
        -d "$(jq -n \
            --arg path "/tmp" \
            --arg filename "sctl-upgrade" \
            --argjson file_size "$file_size" \
            --argjson chunk_size "$chunk_size" \
            --argjson total_chunks "$total_chunks" \
            '{path: $path, filename: $filename, file_size: $file_size, chunk_size: $chunk_size, total_chunks: $total_chunks, file_hash: "", mode: "0755"}'
        )")

    if [[ -z "$init_resp" ]]; then
        err "Failed to init upload"
        exit 1
    fi

    local xfer_id
    xfer_id=$(echo "$init_resp" | jq -r '.transfer_id')
    if [[ -z "$xfer_id" || "$xfer_id" == "null" ]]; then
        err "Invalid upload init response: $init_resp"
        exit 1
    fi
    ok "Transfer ID: $xfer_id"

    # Step 3: Upload chunks
    log "Uploading $total_chunks chunks..."
    local idx=0
    local failed=false
    while [[ $idx -lt $total_chunks ]]; do
        local offset=$((idx * chunk_size))
        local this_size=$chunk_size
        if [[ $((offset + this_size)) -gt $file_size ]]; then
            this_size=$((file_size - offset))
        fi

        # Extract chunk and compute hash
        local chunk_hash
        chunk_hash=$(dd if="$bin_path" bs=1 skip="$offset" count="$this_size" 2>/dev/null | sha256sum | cut -d' ' -f1)

        local chunk_resp
        chunk_resp=$(dd if="$bin_path" bs=1 skip="$offset" count="$this_size" 2>/dev/null | \
            curl -sf -X POST "$url/api/stp/chunk/$xfer_id/$idx" \
                -H "Authorization: Bearer $api_key" \
                -H "Content-Type: application/octet-stream" \
                -H "X-Gx-Chunk-Hash: $chunk_hash" \
                --data-binary @-)

        local ok_val
        ok_val=$(echo "$chunk_resp" | jq -r '.ok // false')
        if [[ "$ok_val" != "true" ]]; then
            err "Chunk $idx failed: $chunk_resp"
            # Retry once
            sleep 1
            chunk_hash=$(dd if="$bin_path" bs=1 skip="$offset" count="$this_size" 2>/dev/null | sha256sum | cut -d' ' -f1)
            chunk_resp=$(dd if="$bin_path" bs=1 skip="$offset" count="$this_size" 2>/dev/null | \
                curl -sf -X POST "$url/api/stp/chunk/$xfer_id/$idx" \
                    -H "Authorization: Bearer $api_key" \
                    -H "Content-Type: application/octet-stream" \
                    -H "X-Gx-Chunk-Hash: $chunk_hash" \
                    --data-binary @-)
            ok_val=$(echo "$chunk_resp" | jq -r '.ok // false')
            if [[ "$ok_val" != "true" ]]; then
                err "Chunk $idx retry failed, aborting"
                curl -sf -X DELETE "$url/api/stp/$xfer_id" \
                    -H "Authorization: Bearer $api_key" >/dev/null 2>&1 || true
                failed=true
                break
            fi
        fi

        # Progress
        printf "\r  chunks: %d/%d" "$((idx + 1))" "$total_chunks"
        idx=$((idx + 1))
    done
    echo ""

    if [[ "$failed" == "true" ]]; then
        exit 1
    fi

    # Step 4: Wait for transfer completion (verification)
    log "Waiting for transfer verification..."
    local phase=""
    for _ in $(seq 1 30); do
        local status_resp
        status_resp=$(curl -sf "$url/api/stp/status/$xfer_id" \
            -H "Authorization: Bearer $api_key" 2>/dev/null) || true
        phase=$(echo "$status_resp" | jq -r '.phase // empty')
        case "$phase" in
            complete)
                ok "Upload complete and verified"
                break
                ;;
            failed)
                err "Transfer verification failed: $(echo "$status_resp" | jq -r '.error // "unknown"')"
                exit 1
                ;;
            *)
                sleep 0.5
                ;;
        esac
    done
    if [[ "$phase" != "complete" ]]; then
        err "Transfer did not complete (phase: $phase)"
        exit 1
    fi

    # Step 5: Verify binary on device
    log "Verifying uploaded binary..."
    local verify_resp
    verify_resp=$(curl -sf -X POST "$url/api/exec" \
        -H "Authorization: Bearer $api_key" \
        -H "Content-Type: application/json" \
        -d '{"command": "/tmp/sctl-upgrade --version", "timeout": 5000}')
    local verify_out
    verify_out=$(echo "$verify_resp" | jq -r '.stdout // empty')
    if [[ -z "$verify_out" ]]; then
        err "Binary verification failed — not executable or crashed"
        err "Response: $verify_resp"
        # Clean up
        curl -sf -X POST "$url/api/exec" \
            -H "Authorization: Bearer $api_key" \
            -H "Content-Type: application/json" \
            -d '{"command": "rm -f /tmp/sctl-upgrade"}' >/dev/null 2>&1 || true
        exit 1
    fi
    ok "Binary verified: $verify_out"

    # Step 6: Ensure watchdog is deployed
    log "Ensuring watchdog is deployed..."
    local wd_check
    wd_check=$(curl -sf -X POST "$url/api/exec" \
        -H "Authorization: Bearer $api_key" \
        -H "Content-Type: application/json" \
        -d '{"command": "test -f /etc/sctl/watchdog.sh && echo yes || echo no"}')
    local has_watchdog
    has_watchdog=$(echo "$wd_check" | jq -r '.stdout // empty' | tr -d '[:space:]')
    if [[ "$has_watchdog" != "yes" ]]; then
        warn "Watchdog not found, deploying..."
        do_device_deploy_watchdog "$name"
    else
        ok "Watchdog already deployed"
    fi

    # Step 7: Swap binary (sync cp) then restart (background, survives sctl death)
    log "Swapping binary and restarting sctl..."
    curl -sf -X POST "$url/api/exec" \
        -H "Authorization: Bearer $api_key" \
        -H "Content-Type: application/json" \
        -d '{"command": "cp /usr/bin/sctl /usr/bin/sctl.rollback && cp /tmp/sctl-upgrade /usr/bin/sctl && chmod +x /usr/bin/sctl && (nohup /etc/init.d/sctl restart > /tmp/sctl-upgrade.log 2>&1 &)", "timeout": 15000}' >/dev/null 2>&1 || true
    sleep 3

    # Step 8: Poll health through relay
    log "Waiting for device to come back up (60s timeout)..."
    local healthy=false
    for i in $(seq 1 60); do
        sleep 1
        local health_resp
        health_resp=$(curl -sf "$url/api/health" 2>/dev/null) || continue
        local version
        version=$(echo "$health_resp" | jq -r '.version // empty')
        if [[ -n "$version" ]]; then
            healthy=true
            ok "Device healthy after ${i}s (version: $version)"

            # Update version in config
            jq --arg name "$name" --arg ver "$version" \
                '.devices[$name].sctl_version = $ver' \
                "$CONFIG_FILE" > "$CONFIG_FILE.tmp" && mv "$CONFIG_FILE.tmp" "$CONFIG_FILE"
            break
        fi
    done

    if [[ "$healthy" == "true" ]]; then
        # Clean up
        log "Cleaning up..."
        curl -sf -X POST "$url/api/exec" \
            -H "Authorization: Bearer $api_key" \
            -H "Content-Type: application/json" \
            -d '{"command": "rm -f /tmp/sctl-upgrade /tmp/sctl-upgrade.log"}' >/dev/null 2>&1 || true

        echo ""
        ok "Remote upgrade complete for '$name'"
        echo "  Rollback binary at /usr/bin/sctl.rollback will be auto-removed after 10 min"
    else
        echo ""
        warn "Device did not come back within 60s"
        warn "The watchdog will attempt rollback within 6 minutes if health checks fail"
        warn "Check status: curl -sf $url/api/health"
    fi
}

# ─── playbook library ────────────────────────────────────────────────

PLAYBOOKS_LIBRARY_DIR="$REPO_DIR/playbooks"

do_playbook_ls() {
    log "Playbook library ($PLAYBOOKS_LIBRARY_DIR):"
    echo ""
    local found=false
    for category_dir in "$PLAYBOOKS_LIBRARY_DIR"/*/; do
        [[ -d "$category_dir" ]] || continue
        local category
        category=$(basename "$category_dir")
        for pb_file in "$category_dir"*.md; do
            [[ -f "$pb_file" ]] || continue
            found=true
            local pb_name desc
            pb_name=$(basename "$pb_file" .md)
            desc=$(sed -n '/^description:/{ s/^description: *//; s/\r$//; p; q }' "$pb_file")
            printf "  \033[1;36m%-12s\033[0m / %-24s  %s\n" "$category" "$pb_name" "$desc"
        done
    done
    if [[ "$found" == "false" ]]; then
        echo "  (no playbooks found)"
    fi
}

do_playbook_deploy() {
    local target="${1:-}"
    local category="${2:-}"

    if [[ -z "$target" ]]; then
        err "Usage: $0 playbook deploy <device|all> [category]"
        err ""
        err "Categories: $(ls -1 "$PLAYBOOKS_LIBRARY_DIR" 2>/dev/null | tr '\n' ' ')"
        err "Omit category to deploy ALL playbooks to the device."
        exit 1
    fi

    require_jq
    ensure_config

    # Build list of devices to deploy to
    local devices=()
    if [[ "$target" == "all" ]]; then
        while IFS= read -r name; do
            [[ -n "$name" ]] && devices+=("$name")
        done < <(cfg_device_names)
    else
        if ! cfg_device_exists "$target"; then
            err "Device '$target' not found"
            exit 1
        fi
        devices+=("$target")
    fi

    if [[ ${#devices[@]} -eq 0 ]]; then
        err "No devices configured"
        exit 1
    fi

    # Build list of playbook files to deploy
    local pb_files=()
    if [[ -n "$category" ]]; then
        local cat_dir="$PLAYBOOKS_LIBRARY_DIR/$category"
        if [[ ! -d "$cat_dir" ]]; then
            err "Category '$category' not found in $PLAYBOOKS_LIBRARY_DIR"
            exit 1
        fi
        for f in "$cat_dir"/*.md; do
            [[ -f "$f" ]] && pb_files+=("$f")
        done
    else
        for cat_dir in "$PLAYBOOKS_LIBRARY_DIR"/*/; do
            [[ -d "$cat_dir" ]] || continue
            for f in "$cat_dir"*.md; do
                [[ -f "$f" ]] && pb_files+=("$f")
            done
        done
    fi

    if [[ ${#pb_files[@]} -eq 0 ]]; then
        err "No playbook files found"
        exit 1
    fi

    log "Deploying ${#pb_files[@]} playbook(s) to ${#devices[@]} device(s)..."

    local total=0 success=0 failed=0
    for dev_name in "${devices[@]}"; do
        local url api_key
        url=$(cfg_device_get "$dev_name" "url")
        api_key=$(cfg_device_get "$dev_name" "api_key")

        if [[ -z "$url" || -z "$api_key" ]]; then
            warn "Skipping '$dev_name': missing url or api_key"
            continue
        fi

        for pb_file in "${pb_files[@]}"; do
            local pb_name
            pb_name=$(basename "$pb_file" .md)
            total=$((total + 1))

            # Extract the playbook name from frontmatter (may differ from filename)
            local fm_name
            fm_name=$(sed -n '/^name:/{ s/^name: *//; s/\r$//; p; q }' "$pb_file")
            if [[ -z "$fm_name" ]]; then
                fm_name="$pb_name"
            fi

            local status_code
            status_code=$(curl -sf -o /dev/null -w "%{http_code}" \
                -X PUT \
                -H "Authorization: Bearer $api_key" \
                -H "Content-Type: text/plain" \
                --data-binary "@$pb_file" \
                "$url/api/playbooks/$fm_name" 2>/dev/null) || status_code="000"

            if [[ "$status_code" =~ ^2 ]]; then
                ok "  $dev_name ← $fm_name"
                success=$((success + 1))
            else
                err "  $dev_name ← $fm_name (HTTP $status_code)"
                failed=$((failed + 1))
            fi
        done
    done

    echo ""
    log "Done: $success/$total deployed ($failed failed)"
}

# ─── tunnel (build + start tunnel dev env with relay) ─────────────────

do_tunnel() {
    local use_cloudflared=false
    local remote_relay_url=""

    # Parse flags
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --cloudflared)
                use_cloudflared=true
                shift
                ;;
            --relay-url)
                if [[ -z "${2:-}" ]]; then
                    err "Missing URL for --relay-url"
                    exit 1
                fi
                remote_relay_url="$2"
                shift 2
                ;;
            *)
                err "Unknown flag: $1"
                err "Usage: $0 tunnel [--cloudflared | --relay-url <url>]"
                exit 1
                ;;
        esac
    done

    # Validate mutual exclusivity
    if [[ "$use_cloudflared" == "true" && -n "$remote_relay_url" ]]; then
        err "--cloudflared and --relay-url are mutually exclusive"
        exit 1
    fi

    # Check cloudflared binary exists
    if [[ "$use_cloudflared" == "true" ]]; then
        if ! command -v cloudflared &>/dev/null; then
            err "cloudflared not found in PATH"
            err "Install: https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/"
            exit 1
        fi
    fi

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
playbooks_dir = "$PLAYBOOKS_DIR"

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

    # Start cloudflared quick tunnel if requested
    if [[ "$use_cloudflared" == "true" ]]; then
        log "Starting cloudflared quick tunnel..."
        cloudflared tunnel --url http://localhost:8443 --no-autoupdate \
            &>"$DATA_DIR/cloudflared.log" &
        local cf_pid=$!
        echo "$cf_pid" > "$CLOUDFLARED_PID_FILE"

        # Poll log for the trycloudflare.com URL (up to 30s)
        local cf_url=""
        for _ in $(seq 1 60); do
            if ! kill -0 "$cf_pid" 2>/dev/null; then
                err "cloudflared exited unexpectedly. Log:"
                tail -20 "$DATA_DIR/cloudflared.log"
                mv "$CONFIG_FILE.pre-tunnel" "$CONFIG_FILE"
                exit 1
            fi
            cf_url=$(grep -oP 'https://[a-z0-9-]+\.trycloudflare\.com' "$DATA_DIR/cloudflared.log" 2>/dev/null | head -1) || true
            if [[ -n "$cf_url" ]]; then
                break
            fi
            sleep 0.5
        done

        if [[ -z "$cf_url" ]]; then
            err "cloudflared failed to produce a URL within 30s. Log:"
            tail -20 "$DATA_DIR/cloudflared.log"
            mv "$CONFIG_FILE.pre-tunnel" "$CONFIG_FILE"
            exit 1
        fi

        # Convert https:// → wss:// and append tunnel register path
        remote_relay_url="${cf_url/https:\/\//wss://}/api/tunnel/register"
        ok "cloudflared tunnel: $cf_url (PID $cf_pid)"
        ok "Remote relay URL: $remote_relay_url"
    fi

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

            # Determine the tunnel URL for this remote device
            local device_tunnel_url
            if [[ -n "$remote_relay_url" ]]; then
                device_tunnel_url="$remote_relay_url"
                log "  $rname ($rhost) — relay via cloudflared/external"
            else
                # Detect our LAN IP toward this device
                local our_ip
                our_ip=$(ip route get "$rhost" 2>/dev/null | grep -oP 'src \K\S+' || true)
                if [[ -z "$our_ip" ]]; then
                    warn "Cannot determine route to $rhost — skipping $rname"
                    continue
                fi
                device_tunnel_url="ws://$our_ip:8443/api/tunnel/register"
                log "  $rname ($rhost) — relay via $our_ip:8443"
            fi

            # Build optional DNS fixup and LTE bind for external tunnel URLs
            local dns_fixup=""
            local bind_address_line=""
            if [[ -n "$remote_relay_url" ]]; then
                dns_fixup='
# Ensure DNS can resolve external hostnames (dnsmasq may use a local
# upstream that returns NXDOMAIN before the real DNS servers respond).
# Uses dnsmasq conf-dir so the fix survives dnsmasq restarts.
DNSMASQ_CONFDIR=$(grep -o "conf-dir=[^ ]*" /var/etc/dnsmasq.conf.* 2>/dev/null | head -1 | cut -d= -f2)
if [ -n "$DNSMASQ_CONFDIR" ] && [ ! -f "$DNSMASQ_CONFDIR/tunnel-dns.conf" ]; then
    echo "server=8.8.8.8" > "$DNSMASQ_CONFDIR/tunnel-dns.conf"
    /etc/init.d/dnsmasq restart 2>/dev/null || killall -HUP dnsmasq 2>/dev/null || true
    sleep 1
fi'
                # Detect LTE interface for bind_address (force tunnel over cellular)
                bind_address_line='
# Check for wwan0 (LTE/cellular) interface — bind_address accepts interface names
WWAN_IFACE=""
if ip link show wwan0 >/dev/null 2>&1; then
    WWAN_IFACE="wwan0"
fi
'
            fi

            # SSH in: stop init.d, create temp config, start sctl with tunnel
            local remote_script
            remote_script=$(cat <<REOF
# Stop normal sctl (init.d stop tells procd, killall ensures the process is gone)
/etc/init.d/sctl stop 2>/dev/null || true
killall sctl 2>/dev/null || true
sleep 1
$dns_fixup
$bind_address_line

# Copy config as base
cp /etc/sctl/sctl.toml /tmp/sctl-relay.toml 2>/dev/null || true

# Strip any existing [tunnel] section and everything after it
sed -i '/^\[tunnel\]/,\$d' /tmp/sctl-relay.toml 2>/dev/null || true

# Keep the original listen address from sctl.toml (e.g. 0.0.0.0:1337) so the
# device is reachable both directly (LAN/WAN) AND via the tunnel relay. The
# server code supports both simultaneously — the HTTP listener runs alongside
# the outbound tunnel WS connection.

# Append tunnel config
cat >> /tmp/sctl-relay.toml <<TEOF

[tunnel]
tunnel_key = "$TUNNEL_KEY"
url = "$device_tunnel_url"
TEOF

# Add bind_address if LTE interface was detected (forces tunnel over cellular)
# Uses interface name so sctl resolves the current IP on each connect attempt,
# surviving carrier IP reassignment across reboots.
if [ -n "\$WWAN_IFACE" ]; then
    echo "bind_address = \"\$WWAN_IFACE\"" >> /tmp/sctl-relay.toml
fi

# Start sctl with tunnel config (no nohup on OpenWrt/ash)
/usr/bin/sctl serve --config /tmp/sctl-relay.toml >/tmp/sctl-relay.log 2>&1 &
echo \$!
REOF
            )

            local rpid ssh_stderr ssh_exit
            ssh_stderr=$(mktemp)
            rpid=$(ssh $ssh_opts "root@$rhost" "$remote_script" 2>"$ssh_stderr") || {
                ssh_exit=$?
                warn "  Failed to start tunnel on $rname ($rhost) — SSH exit code: $ssh_exit"
                if [[ -s "$ssh_stderr" ]]; then
                    warn "  SSH stderr: $(cat "$ssh_stderr")"
                fi
                rm -f "$ssh_stderr"
                continue
            }
            rm -f "$ssh_stderr"

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

    # Rewrite MCP config with direct + relay entries for all devices
    log "Updating MCP config for relay routing..."
    local relay_config='{"config_version":2,"devices":{},"default_device":"local"}'

    # Add local device: direct + relay
    relay_config=$(echo "$relay_config" | jq \
        --arg direct_url "http://$LISTEN" \
        --arg relay_url "http://127.0.0.1:8443/d/$DEVICE_SERIAL" \
        --arg key "$API_KEY" \
        --arg pb "$PLAYBOOKS_DIR" \
        '.devices.local = {url: $direct_url, api_key: $key, playbooks_dir: $pb}
         | .devices["local-relay"] = {url: $relay_url, api_key: $key, playbooks_dir: $pb}')

    # Add remote devices: direct + relay (preserving metadata from pre-tunnel config)
    for i in "${!remote_names[@]}"; do
        local rname="${remote_names[$i]}"
        local rhost="${remote_hosts[$i]}"
        local rserial="${remote_serials[$i]}"
        local rapi_key="${remote_api_keys[$i]}"

        if [[ -n "$rserial" && "$rserial" != "unknown" ]]; then
            # Read metadata from pre-tunnel backup so a mid-tunnel crash doesn't lose it
            local meta
            meta=$(jq -c ".devices[\"$rname\"] | {host, serial, arch, sctl_version, added_at} | with_entries(select(.value != null))" "$CONFIG_FILE.pre-tunnel" 2>/dev/null || echo '{}')

            relay_config=$(echo "$relay_config" | jq \
                --arg name "$rname" \
                --arg direct_url "http://$rhost:1337" \
                --arg relay_url "http://127.0.0.1:8443/d/$rserial" \
                --arg key "$rapi_key" \
                --argjson meta "$meta" \
                '.devices[$name] = ({url: $direct_url, api_key: $key} + $meta)
                 | .devices[$name + "-relay"] = ({url: $relay_url, api_key: $key} + $meta)')
        fi
    done

    echo "$relay_config" | jq '.' > "$CONFIG_FILE"
    ok "MCP config updated (direct + relay entries for all devices)"

    generate_sctlin_seed

    # Start web dev server
    start_web_dev_server

    # Register MCP server with Claude Code (skip if already managed)
    if is_mcp_alive; then
        ok "mcp-sctl already running (managed by Claude Code) — config hot-reload will pick up relay routing"
    else
        log "Registering mcp-sctl with Claude Code (via relay)..."
        claude mcp remove "$MCP_NAME" 2>/dev/null || true
        claude mcp add --transport stdio \
            "$MCP_NAME" -- "$MCP_BIN" --supervisor --config "$CONFIG_FILE"
        ok "MCP server '$MCP_NAME' registered (supervisor mode)"
    fi

    echo ""
    echo "============================================"
    ok "Tunnel dev environment ready!"
    echo ""
    echo "  Relay:        http://127.0.0.1:8443 (PID $relay_pid)"
    if [[ "$use_cloudflared" == "true" ]]; then
        echo "  Cloudflared:  ${remote_relay_url/wss:\/\//https://}"
    fi
    if [[ -n "$remote_relay_url" ]]; then
        echo "  Remote URL:   $remote_relay_url"
    fi
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
    if is_mcp_alive; then
        echo "  MCP is live — config changes picked up automatically."
    else
        echo "  Restart Claude Code or start a new conversation"
        echo "  to pick up the MCP server. Run /mcp to verify."
    fi
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
                    # Kill temp sctl process and clean up DNS fixup
                    if [[ -n "$rpid" ]]; then
                        ssh $ssh_opts "root@$rhost" "kill $rpid 2>/dev/null; rm -f /tmp/sctl-relay.toml /tmp/sctl-relay.log; "'DNSMASQ_CONFDIR=$(grep -o "conf-dir=[^ ]*" /var/etc/dnsmasq.conf.* 2>/dev/null | head -1 | cut -d= -f2); if [ -n "$DNSMASQ_CONFDIR" ] && [ -f "$DNSMASQ_CONFDIR/tunnel-dns.conf" ]; then rm -f "$DNSMASQ_CONFDIR/tunnel-dns.conf"; /etc/init.d/dnsmasq restart 2>/dev/null || killall -HUP dnsmasq 2>/dev/null || true; fi' 2>/dev/null || true
                    fi
                    # Restart normal sctl
                    ssh $ssh_opts "root@$rhost" "/etc/init.d/sctl start" 2>/dev/null || true
                    ok "  $rname: restored"
                ) &
            done
            wait
        fi

        # Kill cloudflared
        if [[ -f "$CLOUDFLARED_PID_FILE" ]]; then
            local cf_pid
            cf_pid=$(cat "$CLOUDFLARED_PID_FILE")
            if kill -0 "$cf_pid" 2>/dev/null; then
                graceful_stop "$cf_pid" "cloudflared"
            fi
            rm -f "$CLOUDFLARED_PID_FILE"
        fi

        # Restore config from backup and regenerate sctlin seed
        if [[ -f "$CONFIG_FILE.pre-tunnel" ]]; then
            mv "$CONFIG_FILE.pre-tunnel" "$CONFIG_FILE"
            ok "Config restored from backup"
            generate_sctlin_seed
        fi

        # Stop local services + deregister MCP
        do_stop
        exit 0
    }

    trap tunnel_cleanup INT TERM

    # Tail logs and wait
    local tail_files=("$DATA_DIR/relay.log" "$DATA_DIR/sctl.log" "$DATA_DIR/web.log")
    if [[ -f "$DATA_DIR/cloudflared.log" ]]; then
        tail_files+=("$DATA_DIR/cloudflared.log")
    fi
    tail -f "${tail_files[@]}" &
    TAIL_PID=$!
    wait $TAIL_PID
}

# ─── relay (production VPS deployment) ────────────────────────────────

do_relay_setup() {
    local remote="${1:-}"
    if [[ -z "$remote" ]]; then
        err "Usage: $0 relay setup <user@host>"
        exit 1
    fi

    # Build x86_64-musl binary
    log "Building sctl for x86_64-musl..."
    make -C "$SCTL_DIR" build-x86

    # Prompt for relay domain (optional — skip for IP-only staging)
    local domain=""
    read -rp "Relay domain (leave empty for IP-only, no TLS): " domain

    # Extract host IP from user@host for IP-only mode
    local remote_ip="${remote#*@}"

    # Determine listen address, URLs, and WS scheme based on domain vs IP-only
    local listen_addr relay_url tunnel_ws_url relay_port
    if [[ -n "$domain" ]]; then
        # Domain mode: Caddy handles TLS + reverse proxy on :443
        listen_addr="127.0.0.1:8443"
        relay_url="https://$domain"
        tunnel_ws_url="wss://$domain/api/tunnel/register"
        relay_port="443"
    else
        # IP-only mode: sctl binds directly, no Caddy
        listen_addr="0.0.0.0:8443"
        relay_url="http://$remote_ip:8443"
        tunnel_ws_url="ws://$remote_ip:8443/api/tunnel/register"
        relay_port="8443"
    fi

    # Generate tunnel key
    local tunnel_key
    tunnel_key=$(openssl rand -hex 16)
    log "Generated tunnel_key: $tunnel_key"

    local ssh_opts="-o ConnectTimeout=10 -o StrictHostKeyChecking=accept-new"

    # Upload binary
    log "Uploading sctl binary to $remote..."
    ssh $ssh_opts "$remote" "systemctl stop sctl-relay 2>/dev/null || true"
    scp $ssh_opts "$RELAY_X86_BIN" "$remote:$RELAY_REMOTE_BIN"
    ssh $ssh_opts "$remote" "chmod +x $RELAY_REMOTE_BIN"
    ok "Binary uploaded"

    # Generate and upload relay.toml
    log "Generating relay config..."
    local relay_toml
    relay_toml=$(cat <<EOF
[server]
listen = "$listen_addr"
max_connections = 100
journal_enabled = false
data_dir = "/var/lib/sctl"

[auth]
api_key = "relay-no-direct-api"

[device]
serial = "RELAY-001"

[logging]
level = "info"

[tunnel]
relay = true
tunnel_key = "$tunnel_key"
heartbeat_timeout_secs = 45
tunnel_proxy_timeout_secs = 60
EOF
    )
    ssh $ssh_opts "$remote" "mkdir -p /etc/sctl /var/lib/sctl"
    echo "$relay_toml" | ssh $ssh_opts "$remote" "cat > $RELAY_REMOTE_CONFIG"
    ok "Config uploaded"

    # Upload systemd service
    log "Installing systemd service..."
    scp $ssh_opts "$SCTL_DIR/files/sctl-relay.service" "$remote:/etc/systemd/system/sctl-relay.service"
    ssh $ssh_opts "$remote" "systemctl daemon-reload && systemctl enable sctl-relay"
    ok "Service installed"

    # Install + configure Caddy (domain mode only)
    if [[ -n "$domain" ]]; then
        log "Installing Caddy..."
        ssh $ssh_opts "$remote" bash <<'CADDY_EOF'
if ! command -v caddy &>/dev/null; then
    apt-get update -qq
    apt-get install -y -qq debian-keyring debian-archive-keyring apt-transport-https curl
    curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' | gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg 2>/dev/null
    curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' | tee /etc/apt/sources.list.d/caddy-stable.list >/dev/null
    apt-get update -qq
    apt-get install -y -qq caddy
fi
echo "Caddy $(caddy version 2>/dev/null || echo 'installed')"
CADDY_EOF
        ok "Caddy ready"

        log "Configuring Caddy for $domain..."
        echo "$domain {
    reverse_proxy localhost:8443
}" | ssh $ssh_opts "$remote" "cat > /etc/caddy/Caddyfile"
        ssh $ssh_opts "$remote" "systemctl restart caddy"
        ok "Caddy configured for $domain"
    fi

    # Configure ufw
    log "Configuring firewall..."
    if [[ -n "$domain" ]]; then
        ssh $ssh_opts "$remote" bash <<'UFW_EOF'
if command -v ufw &>/dev/null; then
    ufw allow 22/tcp >/dev/null 2>&1
    ufw allow 80/tcp >/dev/null 2>&1
    ufw allow 443/tcp >/dev/null 2>&1
    echo "y" | ufw enable 2>/dev/null || true
    ufw status | grep -E "^(22|80|443)"
else
    echo "ufw not found — skipping firewall config"
fi
UFW_EOF
    else
        ssh $ssh_opts "$remote" bash <<'UFW_EOF'
if command -v ufw &>/dev/null; then
    ufw allow 22/tcp >/dev/null 2>&1
    ufw allow 8443/tcp >/dev/null 2>&1
    echo "y" | ufw enable 2>/dev/null || true
    ufw status | grep -E "^(22|8443)"
else
    echo "ufw not found — skipping firewall config"
fi
UFW_EOF
    fi
    ok "Firewall configured"

    # Start relay service
    log "Starting sctl-relay service..."
    ssh $ssh_opts "$remote" "systemctl start sctl-relay"
    sleep 2

    # Health check
    log "Checking relay health..."
    local health
    health=$(ssh $ssh_opts "$remote" "curl -sf http://127.0.0.1:8443/api/health" 2>/dev/null) || true
    if [[ -n "$health" ]]; then
        ok "Relay healthy: $health"
    else
        warn "Health check failed — check logs: ssh $remote journalctl -u sctl-relay -n 50"
    fi

    # Save to .env.local
    local env_file="$REPO_DIR/.env.local"
    # Remove old relay vars if present
    if [[ -f "$env_file" ]]; then
        grep -v '^RELAY_URL=\|^RELAY_TUNNEL_KEY=\|^RELAY_HOST=\|^RELAY_DOMAIN=' "$env_file" > "$env_file.tmp" || true
        mv "$env_file.tmp" "$env_file"
    fi
    cat >> "$env_file" <<EOF

# Relay VPS (auto-generated by rundev.sh relay setup)
RELAY_HOST=$remote
RELAY_DOMAIN=$domain
RELAY_URL=$relay_url
RELAY_TUNNEL_KEY=$tunnel_key
EOF
    ok "Saved relay config to .env.local"

    echo ""
    echo "============================================"
    ok "Relay setup complete!"
    echo ""
    if [[ -n "$domain" ]]; then
        echo "  Domain:       $domain"
    fi
    echo "  Relay URL:    $relay_url"
    echo "  Tunnel key:   $tunnel_key"
    echo "  Service:      sctl-relay (systemd)"
    echo "  Host:         $remote"
    echo ""
    echo "  BPI config (/etc/sctl/sctl.toml):"
    echo "    [tunnel]"
    echo "    tunnel_key = \"$tunnel_key\""
    echo "    url = \"$tunnel_ws_url\""
    echo "    bind_address = \"wwan0\""
    echo ""
    echo "  MCP config (~/.config/sctl/devices.dev.json):"
    echo "    \"bpi-relay\": {"
    echo "      \"url\": \"$relay_url/d/<serial>\","
    echo "      \"api_key\": \"<bpi api key>\""
    echo "    }"
    echo "============================================"
}

do_relay_deploy() {
    local remote="${1:-}"
    if [[ -z "$remote" ]]; then
        # Try .env.local
        if [[ -f "$REPO_DIR/.env.local" ]]; then
            source "$REPO_DIR/.env.local"
            remote="${RELAY_HOST:-}"
        fi
        if [[ -z "$remote" ]]; then
            err "Usage: $0 relay deploy <user@host>"
            err "  (or set RELAY_HOST in .env.local via 'relay setup')"
            exit 1
        fi
        log "Using RELAY_HOST=$remote from .env.local"
    fi

    local ssh_opts="-o ConnectTimeout=10 -o StrictHostKeyChecking=accept-new"

    # Check config exists remotely
    if ! ssh $ssh_opts "$remote" "test -f $RELAY_REMOTE_CONFIG" 2>/dev/null; then
        err "No config found at $remote:$RELAY_REMOTE_CONFIG"
        err "Run '$0 relay setup $remote' first"
        exit 1
    fi

    # Build
    log "Building sctl for x86_64-musl..."
    make -C "$SCTL_DIR" build-x86

    # Stop, upload binary + service, start
    log "Deploying to $remote..."
    ssh $ssh_opts "$remote" "systemctl stop sctl-relay"
    scp $ssh_opts "$RELAY_X86_BIN" "$remote:$RELAY_REMOTE_BIN"
    ssh $ssh_opts "$remote" "chmod +x $RELAY_REMOTE_BIN"
    scp $ssh_opts "$SCTL_DIR/files/sctl-relay.service" "$remote:/etc/systemd/system/sctl-relay.service"
    ssh $ssh_opts "$remote" "systemctl daemon-reload && systemctl start sctl-relay"
    ok "Binary + service deployed"

    sleep 2

    # Health check
    local health
    health=$(ssh $ssh_opts "$remote" "curl -sf http://127.0.0.1:8443/api/health" 2>/dev/null) || true
    if [[ -n "$health" ]]; then
        ok "Relay healthy: $health"
    else
        warn "Health check failed — check logs: ssh $remote journalctl -u sctl-relay -n 50"
    fi
}

do_relay_upgrade() {
    local remote="${1:-}"
    if [[ -z "$remote" ]]; then
        if [[ -f "$REPO_DIR/.env.local" ]]; then
            source "$REPO_DIR/.env.local"
            remote="${RELAY_HOST:-}"
        fi
        if [[ -z "$remote" ]]; then
            err "Usage: $0 relay upgrade <user@host>"
            err "  (or set RELAY_HOST in .env.local via 'relay setup')"
            exit 1
        fi
        log "Using RELAY_HOST=$remote from .env.local"
    fi

    local ssh_opts="-o ConnectTimeout=10 -o StrictHostKeyChecking=accept-new"

    # Get old version
    local old_version
    old_version=$(ssh $ssh_opts "$remote" "$RELAY_REMOTE_BIN --version 2>/dev/null || echo unknown") || old_version="unknown"

    # Build
    log "Building sctl for x86_64-musl..."
    make -C "$SCTL_DIR" build-x86

    # Stop, upload, start
    log "Upgrading binary on $remote..."
    ssh $ssh_opts "$remote" "systemctl stop sctl-relay"
    scp $ssh_opts "$RELAY_X86_BIN" "$remote:$RELAY_REMOTE_BIN"
    ssh $ssh_opts "$remote" "chmod +x $RELAY_REMOTE_BIN && systemctl start sctl-relay"

    sleep 2

    # Get new version
    local new_version
    new_version=$(ssh $ssh_opts "$remote" "$RELAY_REMOTE_BIN --version 2>/dev/null || echo unknown") || new_version="unknown"

    # Health check
    local health
    health=$(ssh $ssh_opts "$remote" "curl -sf http://127.0.0.1:8443/api/health" 2>/dev/null) || true
    if [[ -n "$health" ]]; then
        ok "Upgrade complete: $old_version → $new_version"
    else
        warn "Health check failed after upgrade — check logs: ssh $remote journalctl -u sctl-relay -n 50"
    fi
}

do_relay_status() {
    local remote="${1:-}"

    # If no host given, try external health check via RELAY_URL
    if [[ -z "$remote" ]]; then
        if [[ -f "$REPO_DIR/.env.local" ]]; then
            source "$REPO_DIR/.env.local"
        fi

        if [[ -n "${RELAY_URL:-}" ]]; then
            log "Checking relay health at $RELAY_URL..."
            local health
            health=$(curl -sf "$RELAY_URL/api/health" 2>/dev/null) || true
            if [[ -n "$health" ]]; then
                ok "Relay healthy: $health"
            else
                err "Relay unreachable at $RELAY_URL"
            fi

            # Try tunnel devices endpoint
            if [[ -n "${RELAY_TUNNEL_KEY:-}" ]]; then
                local devices
                devices=$(curl -sf "$RELAY_URL/api/tunnel/devices?token=$RELAY_TUNNEL_KEY" 2>/dev/null) || true
                if [[ -n "$devices" ]]; then
                    echo ""
                    echo "Connected devices:"
                    echo "$devices" | python3 -m json.tool 2>/dev/null || echo "$devices"
                fi
            fi
            return
        fi

        # Fall back to RELAY_HOST for SSH
        remote="${RELAY_HOST:-}"
        if [[ -z "$remote" ]]; then
            err "Usage: $0 relay status [user@host]"
            err "  (or set RELAY_URL/RELAY_HOST in .env.local via 'relay setup')"
            exit 1
        fi
    fi

    local ssh_opts="-o ConnectTimeout=10 -o StrictHostKeyChecking=accept-new"

    log "Relay status on $remote..."
    echo ""

    # systemctl status
    echo "--- systemd service ---"
    ssh $ssh_opts "$remote" "systemctl status sctl-relay --no-pager -l 2>/dev/null || echo 'Service not found'" || true
    echo ""

    # Health check
    echo "--- health ---"
    ssh $ssh_opts "$remote" "curl -sf http://127.0.0.1:8443/api/health 2>/dev/null || echo 'Health check failed'" || true
    echo ""

    # Connected devices
    local tunnel_key
    tunnel_key=$(ssh $ssh_opts "$remote" "grep tunnel_key $RELAY_REMOTE_CONFIG 2>/dev/null | head -1 | sed 's/.*= *\"\\(.*\\)\"/\\1/'" 2>/dev/null) || true
    if [[ -n "$tunnel_key" ]]; then
        echo "--- connected devices ---"
        ssh $ssh_opts "$remote" "curl -sf 'http://127.0.0.1:8443/api/tunnel/devices?token=$tunnel_key' 2>/dev/null || echo 'No devices'" || true
        echo ""
    fi

    # Caddy status
    echo "--- caddy ---"
    ssh $ssh_opts "$remote" "systemctl is-active caddy 2>/dev/null && caddy version 2>/dev/null || echo 'Caddy not running'" || true
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
    tunnel) shift; do_tunnel "$@" ;;
    device)
        case "${2:-ls}" in
            add)     do_device_add "$3" "${4:-}" ;;
            rm)      do_device_rm "${3:-}" ;;
            ls)      do_device_ls ;;
            deploy)  do_device_deploy "${3:-}" ;;
            upgrade) do_device_upgrade "${3:-}" ;;
            deploy-watchdog) do_device_deploy_watchdog "${3:-}" ;;
            upgrade-remote)  do_device_upgrade_remote "${3:-}" ;;
            *)
                echo "Usage: $0 device <command>"
                echo ""
                echo "Commands:"
                echo "  ls                       list devices with health status (default)"
                echo "  add <name> <host>        discover + register a device via SSH"
                echo "  rm <name>                remove a device"
                echo "  deploy <name>            full deploy (binary + config + init script)"
                echo "  upgrade <name>           binary-only upgrade via SSH (stop → upload → start)"
                echo "  deploy-watchdog <name>   deploy watchdog script + cron (SSH or API)"
                echo "  upgrade-remote <name>    binary upgrade via relay (STP upload + swap)"
                exit 1
                ;;
        esac
        ;;
    relay)
        case "${2:-status}" in
            setup)   do_relay_setup "${3:-}" ;;
            deploy)  do_relay_deploy "${3:-}" ;;
            upgrade) do_relay_upgrade "${3:-}" ;;
            status)  do_relay_status "${3:-}" ;;
            *)
                echo "Usage: $0 relay <command> [user@host]"
                echo ""
                echo "Commands:"
                echo "  setup <user@host>     full VPS provisioning (Caddy + sctl + firewall)"
                echo "  deploy [user@host]    deploy binary + service (preserves config)"
                echo "  upgrade [user@host]   binary-only upgrade (stop → upload → start)"
                echo "  status [user@host]    health check + connected devices (default)"
                exit 1
                ;;
        esac
        ;;
    playbook)
        case "${2:-ls}" in
            ls)     do_playbook_ls ;;
            deploy) do_playbook_deploy "${3:-}" "${4:-}" ;;
            *)
                echo "Usage: $0 playbook <command>"
                echo ""
                echo "Commands:"
                echo "  ls                              list playbooks in library (default)"
                echo "  deploy <device|all> [category]  deploy playbooks to device(s) via API"
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
        echo "             --cloudflared        use Cloudflare Quick Tunnel (double CGNAT)"
        echo "             --relay-url <url>    use an external relay URL"
        echo ""
        echo "Device management:"
        echo "  device ls                       list devices with health status"
        echo "  device add <name> <host>        discover + register a device via SSH"
        echo "  device rm <name>                remove a device"
        echo "  device deploy <name>            full deploy (binary + config + init script)"
        echo "  device upgrade <name>           binary-only upgrade via SSH"
        echo "  device deploy-watchdog <name>   deploy watchdog script + cron"
        echo "  device upgrade-remote <name>    binary upgrade via relay (no SSH needed)"
        echo ""
        echo "Relay VPS deployment:"
        echo "  relay setup <user@host>     full VPS provisioning (Caddy + sctl + firewall)"
        echo "  relay deploy [user@host]    deploy binary + service (preserves config)"
        echo "  relay upgrade [user@host]   binary-only upgrade"
        echo "  relay status [user@host]    health check + connected devices"
        echo ""
        echo "Playbook library:"
        echo "  playbook ls                              list playbooks in library"
        echo "  playbook deploy <device|all> [category]  deploy playbooks to device(s)"
        exit 1
        ;;
esac
