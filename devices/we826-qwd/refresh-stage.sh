#!/usr/bin/env bash
# Writes the stage that devices/we826-qwd/ramboot-refresh.sh runs from (TRD-8):
# the current shared init and ramboot.sh, a ramboot.conf naming the release's
# four mips_24kc payloads on its mirror, install.json with the musl loader as
# helper_prefix, the script itself, and files.sha256 over all of them.
# `rundev.sh device upgrade-remote` calls it for a WE826; so do the tests.
#
# Usage: devices/we826-qwd/refresh-stage.sh <release.json> <mirror base> <out dir>
#
# The manifest must already be verified (its signature, its version): this
# only reads it. Prints the release's version on stdout.

set -euo pipefail

if [[ $# -ne 3 ]]; then
    sed -n '2,11p' "$0" >&2
    exit 2
fi
manifest=$1
mirror=${2%/}
out=$3
HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
COMMON="$HERE/../common"

die() { echo "refresh-stage: $*" >&2; exit 1; }

command -v jq >/dev/null 2>&1 || die "jq is required"
[[ -r "$manifest" ]] || die "no manifest $manifest"
[[ "$mirror" =~ ^https?://[A-Za-z0-9._~:/@%+=-]+$ ]] || die "mirror base '$mirror' is not a plain URL"
version=$(jq -r '.version // empty' "$manifest")
[[ "$version" =~ ^[0-9]+(\.[0-9]+)+$ ]] || die "the manifest names no version"

conf_kv() {
    printf "%s='%s'\n" "$1" "$2"
}

mkdir -p "$out"
{
    conf_kv RUN_DIR /tmp/sctl
    conf_kv CACHE_DIR /tmp/sctl/cache
    conf_kv BIN /tmp/sctl/sctl-server
    conf_kv PLUGIN /tmp/sctl/lib/libsctl_comms_quectel.so
    conf_kv MUSL_LIBC /tmp/sctl/lib/libc.so
    conf_kv LIBGCC /tmp/sctl/lib/libgcc_s.so.1
    conf_kv SCTL_CONFIG /etc/sctl/sctl.toml
    for spec in "server SERVER" "plugin PLUGIN" "libc MUSL_LIBC" "libgcc LIBGCC"; do
        role=${spec% *}
        key=${spec#* }
        name='' sha='' gz=''
        read -r name sha gz < <(jq -r --arg role "$role" \
            '[.targets.mips_24kc.files[]? | select(.role == $role)] | first // empty | "\(.name) \(.sha256) \(.gzip)"' \
            "$manifest") || true
        [[ -n "${name:-}" ]] || die "the manifest has no mips_24kc $role"
        [[ "$name" =~ ^[A-Za-z0-9._-]+$ ]] || die "the mips_24kc $role is named '$name'"
        [[ "$sha" =~ ^[0-9a-f]{64}$ ]] || die "the mips_24kc $role has no SHA-256"
        conf_kv "${key}_URL" "$mirror/$name"
        conf_kv "${key}_SHA256" "$sha"
        if [[ "$gz" == "true" ]]; then
            printf '%s_GZIP=1\n' "$key"
        else
            printf '%s_GZIP=0\n' "$key"
        fi
    done
    # install.sh's defaults; ramboot-refresh.sh keeps the unit's own values.
    printf 'MIN_TMP_KB=24576\n'
    printf 'FETCH_TIMEOUT_SECS=90\n'
    printf 'CONNECT_TIMEOUT_SECS=10\n'
    printf 'FETCH_ATTEMPTS=20\n'
    printf 'FETCH_RETRY_SECS=15\n'
    printf 'ALLOW_UNSIGNED=0\n'
} > "$out/ramboot.conf"
# What install.sh writes (docs/upgrade.md, helper_prefix).
printf '{"v":1,"layout":"ramboot","target":"mips_24kc","helper_prefix":["/tmp/sctl/lib/libc.so","--library-path","/tmp/sctl/lib"]}\n' > "$out/install.json"
cp "$COMMON/sctl-ramboot.init" "$out/sctl.init"
cp "$COMMON/sctl-ramboot.sh" "$out/ramboot.sh"
cp "$HERE/ramboot-refresh.sh" "$out/ramboot-refresh.sh"
chmod 0600 "$out/ramboot.conf"
chmod 0644 "$out/install.json"
chmod 0755 "$out/sctl.init" "$out/ramboot.sh" "$out/ramboot-refresh.sh"
(cd "$out" && sha256sum sctl.init ramboot.sh ramboot.conf install.json ramboot-refresh.sh > files.sha256)
echo "$version"
