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
#   relay-route.sh rp-filter
#       Make reverse-path filtering loose (2), now and at every boot. Run
#       again, it changes nothing.
#
# busybox sh and awk; `ip` may be iproute2 or the busybox applet.

HOTPLUG_DIR=/etc/hotplug.d/iface
HOTPLUGS="96-wg-repin 97-sctl-rehome"
# Where rp-filter writes, and with what; tests point them elsewhere.
SYSCTL_CONF=${RELAY_ROUTE_SYSCTL_CONF:-/etc/sysctl.conf}
IPV4_CONF=${RELAY_ROUTE_IPV4_CONF:-/proc/sys/net/ipv4/conf}
SYSCTL=${RELAY_ROUTE_SYSCTL:-sysctl}

usage() {
    echo "usage: relay-route.sh config <off|follow_default> <sctl.toml> | adopt <backup_dir> | restore <backup_dir> | rp-filter" >&2
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

# Loose reverse-path filtering (rp_filter = 2), now and at every boot. The
# agent probes the relay over an uplink the relay route does not use, and the
# answer comes back on that uplink; strict filtering (1) drops it there, so a
# route that failed over could never come back. The kernel applies the larger
# of conf/all and conf/<dev>, so all = 2 makes every interface loose, default
# = 2 covers interfaces made later, and every existing one is written as well,
# through /proc (sysctl would read the dot in a name like eth0.2 as a
# separator). At boot /etc/init.d/sysctl applies /etc/sysctl.conf after
# /etc/sysctl.d/*.conf, and the fleet's onboarding already keeps
# ignore_routes_with_linkdown there.
rp_filter() {
    if [ -f "$SYSCTL_CONF" ]; then
        sed -i \
            -e '/^[[:space:]]*net\.ipv4\.conf\.all\.rp_filter[[:space:]]*=/d' \
            -e '/^[[:space:]]*net\.ipv4\.conf\.default\.rp_filter[[:space:]]*=/d' \
            "$SYSCTL_CONF" || return 1
        # A last line without its newline would swallow the first one added.
        if [ -s "$SYSCTL_CONF" ] && [ -n "$(tail -c 1 "$SYSCTL_CONF")" ]; then
            echo >> "$SYSCTL_CONF" || return 1
        fi
    fi
    printf '%s\n' net.ipv4.conf.all.rp_filter=2 net.ipv4.conf.default.rp_filter=2 \
        >> "$SYSCTL_CONF" || return 1
    "$SYSCTL" -w net.ipv4.conf.all.rp_filter=2 >/dev/null || return 1
    "$SYSCTL" -w net.ipv4.conf.default.rp_filter=2 >/dev/null || return 1
    for f in "$IPV4_CONF"/*/rp_filter; do
        [ -e "$f" ] || continue
        [ "$(cat "$f")" = 2 ] || echo 2 > "$f" || return 1
    done
    echo "relay route: rp_filter is 2 (loose) on every interface, and set so in $SYSCTL_CONF"
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
    rp-filter)
        [ $# -eq 1 ] || usage
        rp_filter
        ;;
    *)
        usage
        ;;
esac
