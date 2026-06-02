# Security Policy

## Security posture

Indexa is **local-first by design**, which removes most of the usual attack surface: it runs
entirely on your machine, makes **no network calls while building or serving context**, and stores
everything in a single local SQLite file. Nothing leaves your device unless you explicitly configure
a cloud model. API keys are written to a `0600` config file and are never logged. The optional web UI
binds to `localhost`; config-write endpoints are gated behind an explicit opt-in env var.

## Supported versions

Indexa is pre-1.0 and ships on a steady release cadence. Security fixes land on the latest release
(currently **v0.11.0**) and `main`.

| Version | Supported |
|---------|-----------|
| latest release (v0.11.0) | ✅ |
| `main` | ✅ |

## Reporting a Vulnerability

**Please do not open a public GitHub issue for security vulnerabilities.**

Report security issues privately via one of these two methods:

1. **GitHub Security Advisories** — use the "Report a vulnerability" button on the [Security tab](../../security/advisories/new) of this repository. This is the preferred method.

2. **Email** — send details to **ahmed541991@gmail.com** with the subject line `[SECURITY] Indexa`.

Include as much of the following as possible:

- Description of the vulnerability and its potential impact
- Steps to reproduce
- Affected versions or commits
- Any suggested mitigations

You can expect an acknowledgement within 72 hours and a resolution plan within 14 days for confirmed issues.

We will credit reporters in the release notes unless you prefer to remain anonymous.
