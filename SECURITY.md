# Security Policy

## Reporting a Vulnerability

If you discover a security vulnerability in sctl, **please report it responsibly**.

**Do NOT open a public GitHub issue for security vulnerabilities.**

Instead, please email: **security@gawd.ai**

Include:
- Description of the vulnerability
- Steps to reproduce
- Potential impact
- Suggested fix (if any)

We will acknowledge receipt within 48 hours and aim to release a fix within 7 days for critical issues.

## Supported Versions

| Version | Supported |
|---------|-----------|
| 0.3.x   | Yes       |
| < 0.3   | No        |

## Security Design

sctl is designed to run on network-accessible devices and takes security seriously:

- **Authentication**: Pre-shared API key with constant-time comparison
- **Path validation**: Rejects traversal attacks at the API boundary
- **Process isolation**: Sessions in own process groups with `kill_on_drop`
- **Atomic writes**: Temp-file-then-rename prevents partial reads
- **Resource limits**: Configurable caps on sessions, file sizes, timeouts

For a detailed security analysis, see [server/docs/REVIEW.md](server/docs/REVIEW.md).
