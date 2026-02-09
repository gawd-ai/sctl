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
#   ./rundev.sh relay    # build + start with tunnel relay (relay + client + MCP via relay)
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
CONFIG_FILE="$DATA_DIR/devices.json"
MCP_NAME="sctl"
WEB_PORT=5173

# Relay mode config
RELAY_LISTEN="127.0.0.1:8443"
RELAY_PID_FILE="$DATA_DIR/relay.pid"
TUNNEL_KEY="dev-tunnel-key"
DEVICE_SERIAL="DEV-LOCAL-001"

# Binaries (release for speed, debug takes too long on PTY-heavy sessions)
SCTL_BIN="$SCTL_DIR/target/release/sctl"
MCP_BIN="$MCP_DIR/target/release/mcp-sctl"

log()  { echo -e "\033[1;34m==>\033[0m $*"; }
err()  { echo -e "\033[1;31m==>\033[0m $*" >&2; }
ok()   { echo -e "\033[1;32m==>\033[0m $*"; }

# --- collect all descendant PIDs of a process ---
descendants() {
    local pid=$1
    local children
    children=$(pgrep -P "$pid" 2>/dev/null) || true
    for child in $children; do
        echo "$child"
        descendants "$child"
    done
}

# --- gracefully stop a process tree: SIGINT → wait → SIGTERM → wait → SIGKILL ---
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

# --- kill running processes ---
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

# --- stop ---
do_stop() {
    log "Stopping..."

    # Deregister MCP from Claude Code
    if claude mcp get "$MCP_NAME" &>/dev/null; then
        claude mcp remove "$MCP_NAME" 2>/dev/null && ok "Removed MCP server '$MCP_NAME' from Claude Code" || true
    fi

    do_kill
}

# --- status ---
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
            devices=$(curl -sf -H "Authorization: Bearer $TUNNEL_KEY" "http://${RELAY_LISTEN}/api/tunnel/devices" 2>/dev/null) || true
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

# --- launch (shared: stop existing, start all services, register MCP) ---
do_launch() {
    # Create data dir, playbooks dir, and config
    mkdir -p "$DATA_DIR"
    mkdir -p "$PLAYBOOKS_DIR"

    # Merge local device into existing config (preserve other devices)
    if [[ -f "$CONFIG_FILE" ]] && command -v jq &>/dev/null; then
        jq --arg url "$DEVICE_URL" --arg key "$API_KEY" --arg pb "$PLAYBOOKS_DIR" \
            '.devices.local = {url: $url, api_key: $key, playbooks_dir: $pb} | .default_device = "local"' \
            "$CONFIG_FILE" > "$CONFIG_FILE.tmp" && mv "$CONFIG_FILE.tmp" "$CONFIG_FILE"
    else
        cat > "$CONFIG_FILE" <<EOF
{
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
    fi
    ok "Config written: $CONFIG_FILE"

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

    for i in $(seq 1 30); do
        if curl -sf "http://${LISTEN}/api/health" >/dev/null 2>&1; then
            ok "sctl running (PID $sctl_pid) on $LISTEN"
            break
        fi
        if ! kill -0 "$sctl_pid" 2>/dev/null; then
            err "sctl exited unexpectedly. Log:"
            tail -20 "$DATA_DIR/sctl.log"
            exit 1
        fi
        sleep 0.2
    done

    if ! curl -sf "http://${LISTEN}/api/health" >/dev/null 2>&1; then
        err "sctl failed to start within 6s. Log:"
        tail -20 "$DATA_DIR/sctl.log"
        exit 1
    fi

    # Start web dev server
    log "Starting web dev server on port $WEB_PORT..."
    local node_bin
    node_bin=$(command -v node 2>/dev/null || command -v node.exe 2>/dev/null) || { err "node not found in PATH"; exit 1; }
    (cd "$WEB_DIR" && exec "$node_bin" node_modules/vite/bin/vite.js dev --port "$WEB_PORT" --strictPort) &>"$DATA_DIR/web.log" &
    web_pid=$!
    echo "$web_pid" > "$WEB_PID_FILE"

    # Wait for vite to be ready
    for i in $(seq 1 75); do
        if curl -sf "http://localhost:${WEB_PORT}" >/dev/null 2>&1; then
            ok "Web dev server running (PID $web_pid) on http://localhost:$WEB_PORT"
            break
        fi
        if ! kill -0 "$web_pid" 2>/dev/null; then
            err "Web dev server exited unexpectedly. Log:"
            tail -20 "$DATA_DIR/web.log"
            exit 1
        fi
        sleep 0.2
    done

    if ! curl -sf "http://localhost:${WEB_PORT}" >/dev/null 2>&1; then
        err "Web dev server failed to start within 15s. Log:"
        tail -20 "$DATA_DIR/web.log"
        exit 1
    fi

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

# --- start (restart without rebuilding) ---
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

# --- build (just compile, no start/stop) ---
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

# --- claude (register MCP only, no build or start) ---
do_claude() {
    if [[ ! -x "$MCP_BIN" ]]; then
        err "mcp-sctl binary not found: $MCP_BIN"
        err "Run '$0 build' first."
        exit 1
    fi

    # Create or merge config (preserve other devices)
    mkdir -p "$DATA_DIR" "$PLAYBOOKS_DIR"
    if [[ -f "$CONFIG_FILE" ]] && command -v jq &>/dev/null; then
        jq --arg url "$DEVICE_URL" --arg key "$API_KEY" --arg pb "$PLAYBOOKS_DIR" \
            '.devices.local = {url: $url, api_key: $key, playbooks_dir: $pb} | .default_device = "local"' \
            "$CONFIG_FILE" > "$CONFIG_FILE.tmp" && mv "$CONFIG_FILE.tmp" "$CONFIG_FILE"
    else
        cat > "$CONFIG_FILE" <<EOF
{
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

# --- relay (build + start with tunnel relay in front) ---
do_relay() {
    do_build

    mkdir -p "$DATA_DIR" "$PLAYBOOKS_DIR"

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
url = "ws://$RELAY_LISTEN/api/tunnel/register"
EOF

    # Merge relay device into MCP config (via relay URL, not direct)
    local relay_device_url="http://$RELAY_LISTEN/d/$DEVICE_SERIAL"
    if [[ -f "$CONFIG_FILE" ]] && command -v jq &>/dev/null; then
        jq --arg url "$relay_device_url" --arg key "$API_KEY" --arg pb "$PLAYBOOKS_DIR" \
            '.devices.local = {url: $url, api_key: $key, playbooks_dir: $pb} | .default_device = "local"' \
            "$CONFIG_FILE" > "$CONFIG_FILE.tmp" && mv "$CONFIG_FILE.tmp" "$CONFIG_FILE"
    else
        cat > "$CONFIG_FILE" <<EOF
{
  "devices": {
    "local": {
      "url": "$relay_device_url",
      "api_key": "$API_KEY",
      "playbooks_dir": "$PLAYBOOKS_DIR"
    }
  },
  "default_device": "local"
}
EOF
    fi
    ok "Config written: $CONFIG_FILE (MCP via relay)"

    # Stop any running instances
    do_kill

    # Start relay first
    log "Starting relay on $RELAY_LISTEN..."
    "$SCTL_BIN" serve --config "$DATA_DIR/relay.toml" &>"$DATA_DIR/relay.log" &
    relay_pid=$!
    echo "$relay_pid" > "$RELAY_PID_FILE"

    for _ in $(seq 1 30); do
        if curl -sf "http://${RELAY_LISTEN}/api/health" >/dev/null 2>&1; then
            ok "Relay running (PID $relay_pid) on $RELAY_LISTEN"
            break
        fi
        if ! kill -0 "$relay_pid" 2>/dev/null; then
            err "Relay exited unexpectedly. Log:"
            tail -20 "$DATA_DIR/relay.log"
            exit 1
        fi
        sleep 0.2
    done

    if ! curl -sf "http://${RELAY_LISTEN}/api/health" >/dev/null 2>&1; then
        err "Relay failed to start within 6s. Log:"
        tail -20 "$DATA_DIR/relay.log"
        exit 1
    fi

    # Start sctl as tunnel client
    log "Starting sctl (tunnel client) on $LISTEN..."
    RUST_LOG=info \
        "$SCTL_BIN" serve --config "$DATA_DIR/client.toml" &>"$DATA_DIR/sctl.log" &
    sctl_pid=$!
    echo "$sctl_pid" > "$PID_FILE"

    for _ in $(seq 1 30); do
        if curl -sf "http://${LISTEN}/api/health" >/dev/null 2>&1; then
            ok "sctl running (PID $sctl_pid) on $LISTEN"
            break
        fi
        if ! kill -0 "$sctl_pid" 2>/dev/null; then
            err "sctl exited unexpectedly. Log:"
            tail -20 "$DATA_DIR/sctl.log"
            exit 1
        fi
        sleep 0.2
    done

    if ! curl -sf "http://${LISTEN}/api/health" >/dev/null 2>&1; then
        err "sctl failed to start within 6s. Log:"
        tail -20 "$DATA_DIR/sctl.log"
        exit 1
    fi

    # Wait for device to register with relay
    log "Waiting for tunnel registration..."
    local registered=false
    for _ in $(seq 1 30); do
        local devices
        devices=$(curl -sf -H "Authorization: Bearer $TUNNEL_KEY" "http://${RELAY_LISTEN}/api/tunnel/devices" 2>/dev/null) || true
        if echo "$devices" | grep -q "$DEVICE_SERIAL" 2>/dev/null; then
            ok "Device $DEVICE_SERIAL registered with relay"
            registered=true
            break
        fi
        sleep 0.2
    done

    if [[ "$registered" != "true" ]]; then
        err "Device failed to register with relay within 6s"
        echo "  Relay log:"
        tail -10 "$DATA_DIR/relay.log"
        echo "  Client log:"
        tail -10 "$DATA_DIR/sctl.log"
        exit 1
    fi

    # Start web dev server
    log "Starting web dev server on port $WEB_PORT..."
    local node_bin
    node_bin=$(command -v node 2>/dev/null || command -v node.exe 2>/dev/null) || { err "node not found in PATH"; exit 1; }
    (cd "$WEB_DIR" && exec "$node_bin" node_modules/vite/bin/vite.js dev --port "$WEB_PORT" --strictPort) &>"$DATA_DIR/web.log" &
    web_pid=$!
    echo "$web_pid" > "$WEB_PID_FILE"

    for _ in $(seq 1 75); do
        if curl -sf "http://localhost:${WEB_PORT}" >/dev/null 2>&1; then
            ok "Web dev server running (PID $web_pid) on http://localhost:$WEB_PORT"
            break
        fi
        if ! kill -0 "$web_pid" 2>/dev/null; then
            err "Web dev server exited unexpectedly. Log:"
            tail -20 "$DATA_DIR/web.log"
            exit 1
        fi
        sleep 0.2
    done

    if ! curl -sf "http://localhost:${WEB_PORT}" >/dev/null 2>&1; then
        err "Web dev server failed to start within 15s. Log:"
        tail -20 "$DATA_DIR/web.log"
        exit 1
    fi

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
    echo "  Relay:        http://$RELAY_LISTEN (PID $relay_pid)"
    echo "  sctl:         http://$LISTEN (PID $sctl_pid, tunnel client)"
    echo "  Device URL:   http://$RELAY_LISTEN/d/$DEVICE_SERIAL"
    echo "  Web UI:       http://localhost:$WEB_PORT (PID $web_pid)"
    echo "  MCP server:   $MCP_NAME (stdio, routed through relay)"
    echo "  Tunnel key:   $TUNNEL_KEY"
    echo ""
    echo "  All MCP/sctlin traffic goes: client -> relay -> tunnel -> sctl"
    echo ""
    echo "  Restart Claude Code or start a new conversation"
    echo "  to pick up the MCP server. Run /mcp to verify."
    echo ""
    echo "  Press Ctrl+C to stop all services."
    echo "============================================"
    echo ""

    # Stay alive: tail logs and wait for Ctrl+C
    trap 'echo ""; log "Shutting down..."; kill $TAIL_PID 2>/dev/null; do_stop; exit 0' INT TERM
    tail -f "$DATA_DIR/relay.log" "$DATA_DIR/sctl.log" "$DATA_DIR/web.log" &
    TAIL_PID=$!
    wait $TAIL_PID
}

# --- setup (default: build + start) ---
do_setup() {
    do_build
    do_launch
}

# --- main ---
case "${1:-setup}" in
    setup)  do_setup ;;
    build)  do_build ;;
    start)  do_start ;;
    stop)   do_stop ;;
    status) do_status ;;
    claude) do_claude ;;
    relay)  do_relay ;;
    *)
        echo "Usage: $0 [setup|build|start|stop|status|claude|relay]"
        echo "  (default)  build everything + start all services + register MCP"
        echo "  build      build only (server, mcp, web) — no start/stop"
        echo "  start      restart all services without rebuilding"
        echo "  stop       stop all services + deregister MCP"
        echo "  status     show what's running"
        echo "  claude     only register MCP in Claude Code (no build/start)"
        echo "  relay      build + start with tunnel relay (tests full tunnel path)"
        exit 1
        ;;
esac
