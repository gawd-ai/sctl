#!/usr/bin/env bash
# Assemble a release bundle (docs/upgrade.md, "The release bundle") from
# built artifacts: copy each file under its bundle name, write release.json
# with sizes and SHA-256s, and sign it when a key is given.
#
#   scripts/release-bundle.sh <out dir> <version> <target>:<role>:<path>[:gzip] ...
#
# Environment:
#   RELEASE_CHANNEL        stable (default) or candidate
#   MIN_FROM_VERSION       default 0.6.7.0
#   RELEASE_NOTES          one paragraph
#   SIGNING_KEY_FILE       ed25519 PEM to sign with (else the bundle is left unsigned)
#
# Example (a laptop bench for one target):
#   scripts/release-bundle.sh release 0.6.7.431 \
#     mips_24kc:server:.artifacts/xe300/sctl-server-mips_24kc.gz:gzip \
#     mips_24kc:plugin:.artifacts/xe300/sctl-comms-quectel-mips_24kc.so.gz:gzip
set -euo pipefail

die() { echo "release-bundle: $*" >&2; exit 1; }

out=${1:?out dir}
version=${2:?version}
shift 2
[ $# -ge 1 ] || die "no artifacts given"
case "$version" in
    *[!0-9.]*|.*|*.) die "'$version' is not a version" ;;
esac
command -v sha256sum >/dev/null || die "missing sha256sum"
command -v python3 >/dev/null || die "missing python3"

rm -rf "$out"
mkdir -p "$out"
entries=()
for spec in "$@"; do
    IFS=: read -r target role path gz <<<"$spec"
    [ -n "$target" ] && [ -n "$role" ] && [ -n "$path" ] || die "bad artifact spec '$spec'"
    [ -f "$path" ] || die "no such file: $path"
    name=$(basename "$path")
    cp "$path" "$out/$name"
    size=$(stat -c %s "$out/$name")
    sha=$(sha256sum "$out/$name" | cut -d' ' -f1)
    gzip_flag=false
    [ "${gz:-}" = "gzip" ] && gzip_flag=true
    entries+=("$target|$role|$name|$size|$sha|$gzip_flag")
done

RELEASE_VERSION="$version" \
RELEASE_CHANNEL="${RELEASE_CHANNEL:-stable}" \
MIN_FROM_VERSION="${MIN_FROM_VERSION:-0.6.7.0}" \
RELEASE_NOTES="${RELEASE_NOTES:-}" \
RELEASE_ENTRIES="$(printf '%s\n' "${entries[@]}")" \
python3 - "$out/release.json" <<'PY'
import json, os, sys, datetime
targets = {}
for line in os.environ["RELEASE_ENTRIES"].splitlines():
    target, role, name, size, sha, gz = line.split("|")
    targets.setdefault(target, {"files": []})["files"].append(
        {"role": role, "name": name, "size": int(size), "sha256": sha, "gzip": gz == "true"})
for t in targets.values():
    if not any(f["role"] == "server" for f in t["files"]):
        sys.exit(f"target has no server file: {t}")
manifest = {
    "v": 1,
    "version": os.environ["RELEASE_VERSION"],
    "channel": os.environ["RELEASE_CHANNEL"],
    "min_from_version": os.environ["MIN_FROM_VERSION"],
    "published_at": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    "notes": os.environ["RELEASE_NOTES"],
    "targets": dict(sorted(targets.items())),
}
with open(sys.argv[1], "w") as f:
    json.dump(manifest, f, indent=1, sort_keys=False)
    f.write("\n")
PY

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
if [ -n "${SIGNING_KEY_FILE:-}" ]; then
    "$here/release-sign.sh" sign "$out/release.json" "$SIGNING_KEY_FILE"
else
    echo "release-bundle: no SIGNING_KEY_FILE; $out/release.json is unsigned (devices refuse it)" >&2
fi
echo "bundle $version in $out:"
ls -l "$out"
