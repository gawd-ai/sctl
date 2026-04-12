#!/bin/sh
# Pre-commit secret scan — blocks commits containing likely secrets.
# Install: ln -sf ../../scripts/pre-commit-secret-scan.sh .git/hooks/pre-commit-secrets
# Or integrate into an existing pre-commit hook by sourcing this script.
#
# Lines containing "secret-scan: allow" are exempted.
# Test fixtures in #[test] blocks are exempted for hex patterns.
#
# Exit 0 = clean, exit 1 = secrets found.

set -e

STAGED=$(git diff --cached --diff-filter=ACM -U0)

if [ -z "$STAGED" ]; then
    exit 0
fi

found=0

check_pattern() {
    label="$1"
    pattern="$2"
    # Search staged diff for added lines (^+) matching the pattern,
    # excluding lines with the allow comment.
    matches=$(echo "$STAGED" | grep -E '^\+' | grep -v '^+++' | grep -v 'secret-scan: allow' | grep -E "$pattern" || true)
    if [ -n "$matches" ]; then
        echo "secret-scan: possible $label detected in staged changes:"
        echo "$matches" | head -10
        echo ""
        found=1
    fi
}

# 32+ char hex string (API keys, tokens)
# Exclude common false positives: git SHAs in diff headers, hash literals in test assertions
hex_matches=$(echo "$STAGED" | grep -E '^\+' | grep -v '^+++' | grep -v 'secret-scan: allow' \
    | grep -v 'assert' | grep -v '#\[test\]' | grep -v '\.sha256' | grep -v 'sha256sum' \
    | grep -v 'commit ' | grep -v 'index ' \
    | grep -oE '\b[a-fA-F0-9]{32,}\b' || true)
if [ -n "$hex_matches" ]; then
    echo "secret-scan: possible hex token (32+ chars) detected in staged changes:"
    echo "$hex_matches" | head -10
    echo ""
    found=1
fi

# URL-embedded tokens
check_pattern "URL token" '[?&](token|access_token|api_key|key|secret|password)=[A-Za-z0-9._-]{16,}'

# AWS access key
check_pattern "AWS access key" 'AKIA[0-9A-Z]{16}'

# GitHub token
check_pattern "GitHub token" 'gh[pousr]_[A-Za-z0-9]{36}'

# JWT
check_pattern "JWT" 'eyJ[A-Za-z0-9_-]+\.eyJ[A-Za-z0-9_-]+'

# PEM private key
check_pattern "PEM private key" 'BEGIN (RSA |DSA |EC |OPENSSH |PGP )?PRIVATE KEY'

if [ "$found" -eq 1 ]; then
    echo "Commit blocked by secret scan."
    echo "If these are intentional (e.g. test fixtures), add '# secret-scan: allow' to the line."
    exit 1
fi

exit 0
