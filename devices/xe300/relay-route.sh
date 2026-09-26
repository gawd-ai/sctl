#!/bin/sh
# The relay route on a GL-XE300, shared by install.sh and upgrade.sh.
#
# With `[tunnel] relay_route = "follow_default"` the agent keeps the host route
# to the relay on the best uplink that answers and moves its tunnel without a
# restart. Two shell hotplugs did that job before it, and go when it takes over:
#
#   96-wg-repin     pinned the WireGuard endpoint (the relay) /32 at metric 0
#                   through the lowest-metric default route
#   97-sctl-rehome  restarted sctl when the wan came up
#
# The pin 96-wg-repin (or a person) left behind goes too. The agent never
# replaces a route it did not install, so while such a pin holds the relay at
# metric 0 the agent leaves the relay to it.
#
# Usage:
#   relay-route.sh config <mode> <sctl.toml>
#       Print the file with `relay_route = "<mode>"` in [tunnel], every other
#       line intact. Exit 2 when [tunnel] sets bind_address (the agent refuses
#       both), 3 when there is no [tunnel] section.
#   relay-route.sh adopt <backup_dir>
#       Remove the two hotplugs and the pins, saving them in <backup_dir>.
#   relay-route.sh restore <backup_dir>
#       Put them back, and drop the route sctl installed (protocol 83).
#
# busybox sh and awk; `ip` may be iproute2 or the busybox applet.

HOTPLUG_DIR=/etc/hotplug.d/iface
HOTPLUGS="96-wg-repin 97-sctl-rehome"

usage() {
    echo "usage: relay-route.sh config <off|follow_default> <sctl.toml> | adopt <backup_dir> | restore <backup_dir>" >&2
    exit 1
}

config() {
    awk -v mode="$1" '
        BEGIN { st = 3 }
        /^[ \t]*\[/ {
            tunnel = ($0 ~ /^[ \t]*\[tunnel\][ \t]*(#.*)?$/)
            print
            if (tunnel) {
                st = 0
                print "relay_route = \"" mode "\""
            }
            next
        }
        tunnel && /^[ \t]*relay_route[ \t]*=/ { next }
        tunnel && /^[ \t]*bind_address[ \t]*=/ { pinned = 1 }
        { print }
        END {
            if (st == 0 && pinned && mode != "off") st = 2
            exit st
        }' "$2"
}

# The IPv4 WireGuard endpoints: the address 96-wg-repin pinned. On the fleet
# it is also the relay the tunnel dials.
endpoints() {
    command -v wg >/dev/null 2>&1 || return 0
    wg show all endpoints 2>/dev/null | awk '{
        if (split($3, p, ":") == 2 && p[1] ~ /^[0-9.]+$/) print p[1]
    }' | sort -u
}

# Routes to $1/32 at metric 0 with protocol boot (`ip route add` by a person
# or a hotplug; iproute2 prints neither `proto boot` nor `metric 0`), as the
# "via <gw> dev <dev>" that names them. busybox ip prints no protocol at all,
# so the delete in adopt names `proto boot` and the kernel refuses the rest.
pins() {
    ip -4 route show 2>/dev/null | awk -v ip="$1" '
        $1 != ip && $1 != ip "/32" { next }
        {
            proto = "boot"; metric = 0; via = ""; dev = ""
            for (i = 2; i < NF; i++) {
                if ($i == "proto") proto = $(i + 1)
                else if ($i == "metric") metric = $(i + 1)
                else if ($i == "via") via = $(i + 1)
                else if ($i == "dev") dev = $(i + 1)
            }
            if (proto == "boot" && metric == 0 && dev != "")
                print (via != "" ? "via " via " " : "") "dev " dev
        }'
}

adopt() {
    backup=$1
    mkdir -p "$backup/hotplug" || return 1
    : > "$backup/pins" || return 1
    for f in $HOTPLUGS; do
        [ -e "$HOTPLUG_DIR/$f" ] || continue
        cp -p "$HOTPLUG_DIR/$f" "$backup/hotplug/" || return 1
        rm -f "$HOTPLUG_DIR/$f" || return 1
        echo "relay route: removed $HOTPLUG_DIR/$f"
    done
    for ip in $(endpoints); do
        pins "$ip" | while read -r args; do
            # shellcheck disable=SC2086 # args is "via <gw> dev <dev>", split on purpose
            if ip route del "$ip/32" $args proto boot; then
                echo "$ip/32 $args" >> "$backup/pins"
                echo "relay route: removed the pin $ip/32 $args"
            fi
        done
    done
}

restore() {
    backup=$1
    for f in "$backup"/hotplug/*; do
        [ -e "$f" ] || continue
        cp -p "$f" "$HOTPLUG_DIR/" && echo "relay route: restored $HOTPLUG_DIR/${f##*/}"
    done
    # The agent being restored does not manage the route the newer one left.
    # busybox ip ignores `proto` when listing but not when deleting, so the
    # kernel's protocol match is what keeps other routes safe here.
    ip -4 route show proto 83 2>/dev/null | while read -r dst _; do
        ip route del "$dst" proto 83 2>/dev/null && echo "relay route: removed sctl's route to $dst"
    done
    [ -f "$backup/pins" ] || return 0
    while read -r dst args; do
        [ -n "$dst" ] || continue
        # shellcheck disable=SC2086 # args is "via <gw> dev <dev>", split on purpose
        ip route replace "$dst" $args && echo "relay route: restored the pin $dst $args"
    done < "$backup/pins"
}

case ${1:-} in
    config)
        [ $# -eq 3 ] || usage
        case $2 in off|follow_default) ;; *) usage ;; esac
        config "$2" "$3"
        ;;
    adopt)
        [ $# -eq 2 ] || usage
        adopt "$2"
        ;;
    restore)
        [ $# -eq 2 ] || usage
        restore "$2"
        ;;
    *)
        usage
        ;;
esac
