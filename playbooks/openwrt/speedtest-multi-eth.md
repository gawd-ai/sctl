---
name: speedtest-multi-eth
description: Fixed-size download throughput and latency through each wired (ETH) WAN path. Forces each test out one interface via temporary /32 host routes; the default route is never changed and all temp routes are removed on exit.
params:
  interfaces:
    type: string
    description: Interfaces to test — auto (all ETH ports with a gateway), all (every default-route interface incl. LTE), or comma-separated list e.g. eth5,eth4
    default: auto
  size_mb:
    type: string
    description: Download size per interface in MB (transfer is fixed-size; bigger = more accurate, slower)
    default: "20"
  url:
    type: string
    description: Test endpoint. Default Cloudflare __down (script appends ?bytes=N from size_mb). A full custom file URL is used as-is.
    default: https://speed.cloudflare.com/__down
  verbosity:
    type: string
    description: Output detail level
    default: normal
    enum: [brief, normal, verbose]
---

```sh
#!/bin/sh
# Speedtest (multi-ETH) — per-interface throughput + latency, default route untouched.
# Each test is forced out one interface with TEMPORARY /32 routes to the test
# server's resolved IPs. A trap removes every temp route on exit.
# Method: fixed-size FOREGROUND download (bounded by wget -T), timed via /proc/uptime.

IFACES_REQ="{{interfaces}}"
SIZE_MB="{{size_mb}}"
URL="{{url}}"
VERBOSITY="{{verbosity}}"

sep() { printf '\n══════ %s ══════\n' "$1"; }
case "$SIZE_MB" in ''|*[!0-9]*) SIZE_MB=20;; esac
[ "$SIZE_MB" -lt 1 ] && SIZE_MB=20
BYTES_REQ=$((SIZE_MB*1000000))

command -v wget >/dev/null 2>&1 || { echo "ERROR: wget not available"; exit 1; }
command -v ip   >/dev/null 2>&1 || { echo "ERROR: ip not available"; exit 1; }

# build full URL (append ?bytes=N for the Cloudflare __down endpoint)
case "$URL" in
  *\?*)     FULL="$URL" ;;
  *__down)  FULL="$URL?bytes=$BYTES_REQ" ;;
  *)        FULL="$URL" ;;
esac

# resolve test host to its IPv4 addresses
HOST=$(echo "$URL" | sed 's#^[a-zA-Z]*://##; s#[/?].*##; s#:.*##')
IPS=$(nslookup "$HOST" 2>/dev/null | awk '/^Name:/{n=1} n&&/^Address/{print $NF}' \
      | grep -E '^([0-9]{1,3}\.){3}[0-9]{1,3}$' | grep -v '^127\.' | sort -u)
echo "$HOST" | grep -qE '^([0-9]{1,3}\.){3}[0-9]{1,3}$' && IPS="$HOST"

gw_of() { ip route show default 2>/dev/null \
  | awk -v d="$1" '$0 ~ ("dev " d) {for(i=1;i<=NF;i++) if($i=="via"){print $(i+1); exit}}'; }
DEFDEV=$(ip route show default 2>/dev/null | awk '{for(i=1;i<=NF;i++) if($i=="dev"){print $(i+1); exit}}')

sep "SPEEDTEST (multi-ETH)"
echo "Endpoint  : $FULL"
echo "Server    : $HOST -> $(echo $IPS | tr '\n' ' ')"
echo "Size      : ${SIZE_MB} MB per interface"
echo "Default route (unchanged): $(ip route show default | head -1)"

# decide which interfaces to test
case "$IFACES_REQ" in
  auto)
    LIST=""
    for d in $(ip -br link 2>/dev/null | awk '{print $1}' | grep -E '^eth[0-9]'); do
      [ -n "$(gw_of "$d")" ] && LIST="$LIST $d"
    done ;;
  all)
    LIST=$(ip route show default 2>/dev/null \
      | awk '{for(i=1;i<=NF;i++) if($i=="dev") print $(i+1)}' | sort -u | tr '\n' ' ') ;;
  *)
    LIST=$(echo "$IFACES_REQ" | tr ',' ' ') ;;
esac
LIST=$(echo $LIST)
[ -z "$LIST" ] && { echo; echo "No testable interfaces (need a default-route gateway). Try interfaces=eth5."; exit 0; }
echo "Interfaces: $LIST"

cleanup() { for ip in $IPS; do ip route del "$ip/32" 2>/dev/null; done; rm -f /tmp/_spd.bin; }
trap cleanup EXIT INT TERM

RES=/tmp/_spd_res.$$
: > "$RES"

for d in $LIST; do
  sep "TEST $d"
  g=$(gw_of "$d")
  src=$(ip -4 addr show "$d" 2>/dev/null | awk '/inet /{print $2; exit}')
  link=$(cat /sys/class/net/"$d"/operstate 2>/dev/null || echo "?")
  echo "interface=$d gateway=${g:-none} src=${src:-none} link=$link"

  if [ -z "$g" ]; then echo "  -> no gateway, skip"; echo "$d|none|${src:-none}|-|-|-|no-gw" >> "$RES"; continue; fi
  if [ "$link" != "up" ]; then echo "  -> link $link, skip"; echo "$d|$g|${src:-none}|-|-|-|down" >> "$RES"; continue; fi
  if [ -z "$IPS" ]; then echo "  -> could not resolve $HOST"; echo "$d|$g|${src:-none}|-|-|-|dns-fail" >> "$RES"; continue; fi

  # pin temporary /32 routes for every server IP via this interface
  for ip in $IPS; do ip route replace "$ip/32" via "$g" dev "$d" 2>/dev/null; done

  first=$(echo $IPS | awk '{print $1}')
  lat=$(ping -c 3 -W 2 "$first" 2>/dev/null | awk -F'/' '/min\/avg\/max/{print $4" ms"}')
  [ -z "$lat" ] && lat="(no icmp)"
  [ "$VERBOSITY" != "brief" ] && echo "  latency: $lat"

  # fixed-size foreground download, timed via /proc/uptime (centiseconds)
  rm -f /tmp/_spd.bin
  t0=$(cut -d' ' -f1 /proc/uptime | tr -d '.')
  wget -q -T 15 -O /tmp/_spd.bin "$FULL" </dev/null >/dev/null 2>&1
  rc=$?
  t1=$(cut -d' ' -f1 /proc/uptime | tr -d '.')
  bytes=$(wc -c < /tmp/_spd.bin 2>/dev/null); bytes=${bytes:-0}
  rm -f /tmp/_spd.bin

  for ip in $IPS; do ip route del "$ip/32" via "$g" dev "$d" 2>/dev/null; done

  el_cs=$((t1-t0)); [ "$el_cs" -lt 1 ] && el_cs=1
  if [ "$bytes" -lt 1 ]; then
    echo "  -> 0 bytes (wget rc=$rc; server unreachable via $d?)"
    echo "$d|$g|${src:-none}|$lat|0.0|-|FAIL" >> "$RES"; continue
  fi
  mb=$((bytes/1000000)); mdec=$(((bytes/100000)%10))
  mbps10=$((bytes/(el_cs*125)))
  printf '  -> %d.%d MB in %d.%02ds  =>  %d.%d Mbit/s\n' "$mb" "$mdec" "$((el_cs/100))" "$((el_cs%100))" "$((mbps10/10))" "$((mbps10%10))"
  echo "$d|$g|${src:-none}|$lat|$((mbps10/10)).$((mbps10%10))|$((el_cs/100)).$((el_cs%100))s|OK" >> "$RES"
done

sep "RESULTS"
B='+--------+-----------------+--------------------+-------------+------------+--------+--------+'
F='| %-6s | %-15s | %-18s | %-11s | %-10s | %-6s | %-6s |\n'
printf '%s\n' "$B"
printf "$F" 'IFACE' 'Gateway' 'Src IP' 'Latency' 'Mbit/s' 'Time' 'Result'
printf '%s\n' "$B"
while IFS='|' read d g src lat mbps tm st; do
  [ "$d" = "$DEFDEV" ] && d="${d}*"
  printf "$F" "$d" "$g" "$src" "$lat" "$mbps" "$tm" "$st"
done < "$RES"
printf '%s\n' "$B"
echo "(* = current default-route interface — note the default route was NOT changed)"
rm -f "$RES"

sep "DONE"
echo "Speedtest completed at $(date)"
```
