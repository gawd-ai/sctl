#!/usr/bin/env bash
# Sign (or verify) a release manifest: ed25519 over the exact bytes of
# release.json, written as one line `ed25519:<key id>:<base64>` to
# release.json.sig (docs/upgrade.md, "Signing").
#
#   scripts/release-sign.sh sign   <release.json> <private-key.pem>
#   scripts/release-sign.sh verify <release.json> <public-key.pem|hex>
#   scripts/release-sign.sh pubkey <private-key.pem>      # prints the 64-hex public key + id
#
# The private key is an OpenSSL ed25519 PEM (`openssl genpkey -algorithm
# ed25519`). In CI it arrives as the secret SCTL_RELEASE_SIGNING_KEY; the
# agent embeds the public key in server/src/upgrade/keys.rs.
set -euo pipefail

die() { echo "release-sign: $*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "missing $1"; }
need openssl
need base64

pubkey_hex_of_private() {
    openssl pkey -in "$1" -pubout -outform DER | tail -c 32 | od -An -v -tx1 | tr -d ' \n'
}

key_id_of_hex() {
    printf '%s' "$1" | xxd -r -p | sha256sum | cut -c1-8
}

pubkey_pem_of_hex() {
    # The DER prefix of an ed25519 SubjectPublicKeyInfo, then the raw key.
    { printf '\x30\x2a\x30\x05\x06\x03\x2b\x65\x70\x03\x21\x00'; printf '%s' "$1" | xxd -r -p; } \
        | openssl pkey -pubin -inform DER -outform PEM
}

cmd=${1:-}
case "$cmd" in
    sign)
        manifest=${2:?release.json}
        key=${3:?private key PEM}
        [ -f "$manifest" ] || die "no $manifest"
        [ -f "$key" ] || die "no $key"
        hex=$(pubkey_hex_of_private "$key")
        id=$(key_id_of_hex "$hex")
        sig=$(openssl pkeyutl -sign -inkey "$key" -rawin -in "$manifest" | base64 -w0)
        printf 'ed25519:%s:%s\n' "$id" "$sig" > "$manifest.sig"
        echo "signed $manifest with key $id -> $manifest.sig"
        ;;
    verify)
        manifest=${2:?release.json}
        pub=${3:?public key PEM or 64-hex}
        [ -f "$manifest" ] || die "no $manifest"
        [ -f "$manifest.sig" ] || die "no $manifest.sig"
        line=$(head -n1 "$manifest.sig")
        case "$line" in ed25519:*:*) ;; *) die "$manifest.sig is not ed25519:<id>:<base64>" ;; esac
        b64=${line##*:}
        tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
        printf '%s' "$b64" | base64 -d > "$tmp/sig.bin"
        if [ -f "$pub" ]; then
            cp "$pub" "$tmp/pub.pem"
        else
            pubkey_pem_of_hex "$pub" > "$tmp/pub.pem"
        fi
        openssl pkeyutl -verify -pubin -inkey "$tmp/pub.pem" -rawin -in "$manifest" -sigfile "$tmp/sig.bin" >/dev/null \
            || die "signature does not verify"
        echo "ok: $manifest verifies"
        ;;
    pubkey)
        key=${2:?private key PEM}
        hex=$(pubkey_hex_of_private "$key")
        echo "public_key_hex=$hex"
        echo "key_id=$(key_id_of_hex "$hex")"
        ;;
    *)
        sed -n '2,12p' "$0" >&2
        exit 2
        ;;
esac
