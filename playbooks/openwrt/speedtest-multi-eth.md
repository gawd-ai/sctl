---
name: speedtest-multi-eth
description: Per-interface download AND upload throughput + latency through each WAN path. Forces each test out one interface via temporary /32 host routes; the default route is never changed and all temp routes are removed on exit. Uses curl (streams large uploads, reports speed directly); falls back to wget for download only when curl is absent.
params:
  interfaces:
    type: string
    description: Interfaces to test — auto (all ETH ports with a gateway), all (every default-route interface incl. LTE), or comma-separated list e.g. eth5,wwan0
    default: auto
  direction:
    type: string
    description: Which direction(s) to measure
    default: both
    enum: [both, down, up]
  size_mb:
    type: string
    description: Download size per interface in MB (fixed-size transfer; bigger = more accurate, slower)
    default: "20"
  up_size_mb:
    type: string
    description: Upload size per interface in MB (upload links are usually slower, so this defaults smaller)
    default: "8"
  url:
    type: string
    description: Download endpoint. Default Cloudflare __down (script appends ?bytes=N). Upload posts to the matching /__up on the same host.
    default: https://speed.cloudflare.com/__down
  verbosity:
    type: string
    description: Output detail level
    default: normal
    enum: [brief, normal, verbose]
---

```sh
#!/bin/sh
# Speedtest (multi-ETH) — per-interface download/upload throughput + latency,
# default route untouched. Each test is forced out one interface with TEMPORARY
# /32 routes to the test server's resolved IPs; a trap removes them on exit.
#
# Transfer engine: curl (streams large uploads, reports %{speed_*} directly).
# Falls back to uclient-fetch/wget for DOWNLOAD only when curl is absent —
# upload requires curl because OpenWrt's uclient-fetch cannot POST bodies
# larger than ~64KB (it sends Content-Length then stalls; verified 2026-05-26).

IFACES_REQ="{{interfaces}}"
DIRECTION="{{direction}}"
SIZE_MB="{{size_mb}}"
UP_MB="{{up_size_mb}}"
URL="{{url}}"
VERBOSITY="{{verbosity}}"
MAXT=40   # per-leg hard timeout (seconds)

sep() { printf '\n══════ %s ══════\n' "$1"; }
case "$SIZE_MB"   in ''|*[!0-9]*) SIZE_MB=20;; esac; [ "$SIZE_MB" -lt 1 ] && SIZE_MB=20
case "$UP_MB"     in ''|*[!0-9]*) UP_MB=8;;   esac; [ "$UP_MB"   -lt 1 ] && UP_MB=8
case "$DIRECTION" in down|up|both) ;; *) DIRECTION=both ;; esac
BYTES_REQ=$((SIZE_MB*1000000))

command -v ip >/dev/null 2>&1 || { echo "ERROR: ip not available"; exit 1; }
HAVE_CURL=0; command -v curl >/dev/null 2>&1 && HAVE_CURL=1
HAVE_WGET=0; command -v wget >/dev/null 2>&1 && HAVE_WGET=1
[ "$HAVE_CURL" = 1 ] || [ "$HAVE_WGET" = 1 ] || { echo "ERROR: neither curl nor wget present"; exit 1; }

# download URL: append ?bytes=N for the Cloudflare __down endpoint
case "$URL" in
  *\?*)    DOWN_URL="$URL" ;;
  *__down) DOWN_URL="$URL?bytes=$BYTES_REQ" ;;
  *)       DOWN_URL="$URL" ;;
esac
# scheme+host -> matching upload sink (Cloudflare __up on the same host)
SCHEME=$(echo "$URL" | sed -n 's#^\([a-zA-Z]*\)://.*#\1#p'); [ -z "$SCHEME" ] && SCHEME=https
HOST=$(echo "$URL" | sed 's#^[a-zA-Z]*://##; s#[/?].*##; s#:.*##')
UP_URL="$SCHEME://$HOST/__up"

# resolve test host to its IPv4 addresses (for /32 forcing)
IPS=$(nslookup "$HOST" 2>/dev/null | awk '/^Name:/{n=1} n&&/^Address/{print $NF}' \
      | grep -E '^([0-9]{1,3}\.){3}[0-9]{1,3}$' | grep -v '^127\.' | sort -u)
echo "$HOST" | grep -qE '^([0-9]{1,3}\.){3}[0-9]{1,3}$' && IPS="$HOST"

gw_of() { ip route show default 2>/dev/null \
  | awk -v d="$1" '$0 ~ ("dev " d) {for(i=1;i<=NF;i++) if($i=="via"){print $(i+1); exit}}'; }
DEFDEV=$(ip route show default 2>/dev/null | awk '{for(i=1;i<=NF;i++) if($i=="dev"){print $(i+1); exit}}')

sep "SPEEDTEST (multi-ETH)"
echo "Direction : $DIRECTION    download ${SIZE_MB} MB / upload ${UP_MB} MB per interface"
echo "Download  : $DOWN_URL"
[ "$DIRECTION" != down ] && echo "Upload    : $UP_URL"
echo "Server    : $HOST -> $(echo $IPS | tr '\n' ' ')"
echo "Engine    : $( [ "$HAVE_CURL" = 1 ] && echo curl || echo 'wget (download only)' )"
echo "Default route (unchanged): $(ip route show default | head -1)"

if [ "$DIRECTION" != down ] && [ "$HAVE_CURL" != 1 ]; then
  echo
  echo "NOTE: upload needs curl — uclient-fetch cannot POST bodies >~64KB."
  echo "      Install with:  opkg update && opkg install curl"
fi

# decide which interfaces to test
case "$IFACES_REQ" in
  auto) LIST=""
    for d in $(ip -br link 2>/dev/null | awk '{print $1}' | grep -E '^eth[0-9]'); do
      [ -n "$(gw_of "$d")" ] && LIST="$LIST $d"
    done ;;
  all)  LIST=$(ip route show default 2>/dev/null \
      | awk '{for(i=1;i<=NF;i++) if($i=="dev") print $(i+1)}' | sort -u | tr '\n' ' ') ;;
  *)    LIST=$(echo "$IFACES_REQ" | tr ',' ' ') ;;
esac
LIST=$(echo $LIST)
[ -z "$LIST" ] && { echo; echo "No testable interfaces (need a default-route gateway). Try interfaces=eth5."; exit 0; }
echo "Interfaces: $LIST"

# build the upload payload once (zeros — request bodies are not compressed)
PAYLOAD=/tmp/_spd_up.$$
if [ "$DIRECTION" != down ] && [ "$HAVE_CURL" = 1 ]; then
  dd if=/dev/zero of="$PAYLOAD" bs=1000000 count="$UP_MB" 2>/dev/null
fi

cleanup() { for ip in $IPS; do ip route del "$ip/32" 2>/dev/null; done; rm -f "$PAYLOAD" /tmp/_spd.bin; }
trap cleanup EXIT INT TERM

RES=/tmp/_spd_res.$$; : > "$RES"

# bytes/sec -> Mbit/s*10  (bytes*8*10/1e6 = bytes/12500)
fmt_mbps() { bps=$(echo "$1" | cut -d. -f1); [ -z "$bps" ] && bps=0; echo $((bps/12500)); }
mbps_str() { [ "$1" -lt 0 ] 2>/dev/null && { echo "-"; return; }; echo "$(($1/10)).$(($1%10))"; }

# echo "<Mbit_x10> <http_or_note> <bytes>"   (Mbit_x10 < 0 => failed)
do_download() {
  if [ "$HAVE_CURL" = 1 ]; then
    o=$(curl -s -o /dev/null --max-time "$MAXT" -w '%{speed_download} %{http_code} %{size_download}' "$DOWN_URL" 2>/dev/null)
    sp=${o%% *}; hc=$(echo "$o" | awk '{print $2}'); by=$(echo "$o" | awk '{print $3}')
    if [ "$hc" = 200 ] && [ "${by:-0}" -gt 0 ]; then echo "$(fmt_mbps "$sp") $hc $by"; else echo "-1 ${hc:-000} ${by:-0}"; fi
  else
    rm -f /tmp/_spd.bin
    t0=$(cut -d' ' -f1 /proc/uptime | tr -d '.')
    wget -q -T 15 -O /tmp/_spd.bin "$DOWN_URL" </dev/null >/dev/null 2>&1
    t1=$(cut -d' ' -f1 /proc/uptime | tr -d '.')
    by=$(wc -c < /tmp/_spd.bin 2>/dev/null); by=${by:-0}; rm -f /tmp/_spd.bin
    el=$((t1-t0)); [ "$el" -lt 1 ] && el=1
    if [ "$by" -gt 0 ]; then echo "$((by/(el*125))) 200 $by"; else echo "-1 000 0"; fi
  fi
}
do_upload() {
  [ "$HAVE_CURL" = 1 ] || { echo "-1 nocurl 0"; return; }
  o=$(curl -s -o /dev/null --max-time "$MAXT" -X POST -H 'Content-Type: application/octet-stream' \
        -T "$PAYLOAD" -w '%{speed_upload} %{http_code} %{size_upload}' "$UP_URL" 2>/dev/null)
  sp=${o%% *}; hc=$(echo "$o" | awk '{print $2}'); by=$(echo "$o" | awk '{print $3}')
  if [ "$hc" = 200 ] && [ "${by:-0}" -gt 0 ]; then echo "$(fmt_mbps "$sp") $hc $by"; else echo "-1 ${hc:-000} ${by:-0}"; fi
}

for d in $LIST; do
  sep "TEST $d"
  g=$(gw_of "$d")
  src=$(ip -4 addr show "$d" 2>/dev/null | awk '/inet /{print $2; exit}')
  link=$(cat /sys/class/net/"$d"/operstate 2>/dev/null || echo "?")
  echo "interface=$d gateway=${g:-none} src=${src:-none} link=$link"

  if [ -z "$g" ]; then echo "  -> no gateway, skip"; echo "$d|${src:-none}|-|-|-|no-gw" >> "$RES"; continue; fi
  # ethernet reports operstate=up; point-to-point links (wwan0/wg0/ppp) report
  # "unknown" even when fully up — accept both, reject only a real down state.
  case "$link" in up|unknown) ;; *) echo "  -> link $link, skip"; echo "$d|${src:-none}|-|-|-|$link" >> "$RES"; continue ;; esac
  if [ -z "$IPS" ]; then echo "  -> could not resolve $HOST"; echo "$d|${src:-none}|-|-|-|dns-fail" >> "$RES"; continue; fi

  # pin temporary /32 routes for every server IP via this interface
  for ip in $IPS; do ip route replace "$ip/32" via "$g" dev "$d" 2>/dev/null; done

  first=$(echo $IPS | awk '{print $1}')
  lat=$(ping -c 3 -W 2 "$first" 2>/dev/null | awk -F'/' '/min\/avg\/max/{print $4" ms"}')
  [ -z "$lat" ] && lat="(no icmp)"
  [ "$VERBOSITY" != brief ] && echo "  latency : $lat"

  dmbps=-1; umbps=-1; dnote=""; unote=""
  if [ "$DIRECTION" != up ]; then
    r=$(do_download); dmbps=$(echo "$r" | awk '{print $1}'); dnote=$(echo "$r" | awk '{print $2}')
    [ "$VERBOSITY" != brief ] && echo "  download: $(mbps_str "$dmbps") Mbit/s (http=$dnote)"
  fi
  if [ "$DIRECTION" != down ]; then
    r=$(do_upload);   umbps=$(echo "$r" | awk '{print $1}'); unote=$(echo "$r" | awk '{print $2}')
    [ "$VERBOSITY" != brief ] && echo "  upload  : $(mbps_str "$umbps") Mbit/s (http=$unote)"
  fi

  for ip in $IPS; do ip route del "$ip/32" via "$g" dev "$d" 2>/dev/null; done

  df=0; uf=0
  [ "$DIRECTION" != up ]   && [ "$dmbps" -lt 0 ] && df=1
  [ "$DIRECTION" != down ] && [ "$umbps" -lt 0 ] && uf=1
  st=OK
  [ "$df" = 1 ] && [ "$uf" = 1 ] && st="FAIL d:$dnote u:$unote"
  [ "$df" = 1 ] && [ "$uf" = 0 ] && st="dl:$dnote"
  [ "$df" = 0 ] && [ "$uf" = 1 ] && st="ul:$unote"
  echo "$d|${src:-none}|$lat|$(mbps_str "$dmbps")|$(mbps_str "$umbps")|$st" >> "$RES"
done

sep "RESULTS"
B='+--------+--------------------+-------------+------------+------------+--------------------+'
F='| %-6s | %-18s | %-11s | %-10s | %-10s | %-18s |\n'
printf '%s\n' "$B"
printf "$F" 'IFACE' 'Src IP' 'Latency' 'Down Mb/s' 'Up Mb/s' 'Result'
printf '%s\n' "$B"
while IFS='|' read d src lat dn up st; do
  [ "$d" = "$DEFDEV" ] && d="${d}*"
  printf "$F" "$d" "$src" "$lat" "$dn" "$up" "$st"
done < "$RES"
printf '%s\n' "$B"
echo "(* = current default-route interface — the default route was NOT changed during testing)"
rm -f "$RES"

sep "DONE"
echo "Speedtest completed at $(date)"
```
