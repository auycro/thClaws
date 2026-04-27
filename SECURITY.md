# Security Policy

## Supported versions

Only the latest minor release of thClaws receives security updates. Older versions are not patched — please upgrade to the current release when a fix is published.

| Version | Supported |
|---|---|
| 0.3.x | ✅ |
| < 0.3 | ❌ |

## Reporting a vulnerability

Please **do not open a public GitHub issue** for security vulnerabilities. Use one of these private channels:

### Preferred: GitHub Private Vulnerability Reporting

[**Open a private security advisory →**](https://github.com/thClaws/thClaws/security/advisories/new)

Reports filed through this form become draft GitHub Security Advisories — private by default, with built-in collaboration on the fix and one-click CVE publishing when we coordinate disclosure.

### Alternate: Email

📧 **security@thaigpt.com** — for researchers who prefer email or use PGP.

Whichever channel you choose, please include:

- A clear description of the vulnerability
- Steps to reproduce (minimal proof-of-concept preferred)
- The version of thClaws affected (output of `thclaws --version`)
- Your assessment of impact and severity
- Your name / handle for credit (optional — anonymous reports are welcome)

We will acknowledge receipt within **72 hours** and provide a substantive response within **7 days**. Critical vulnerabilities are triaged immediately.

## Disclosure process

1. Report received — acknowledged within 72 hours
2. Triage + reproduction — usually within 7 days
3. Fix developed and tested
4. Coordinated release — patched version published
5. Advisory published (GitHub Security Advisory + release notes)
6. Credit given to reporter (unless anonymity requested)

We aim to resolve critical issues within **14 days** of confirmation.

## Scope

The following are in scope for security reports:

- The `thclaws` binary and core Rust crates
- The `thclaws-core` library
- Bundled frontend (Vite + React)
- Official GitHub Actions workflows
- Documentation that could mislead users into unsafe configurations

Out of scope:

- Third-party plugins, skills, or MCP servers (report to their maintainers)
- Third-party LLM providers (Anthropic, OpenAI, Google, etc.)
- Issues requiring physical access to the user's machine
- Social-engineering attacks that don't exploit a thClaws defect

## What to report

Examples of issues we want to hear about:

- Path traversal or sandbox escape in file tools
- Secret leakage (API keys written to logs, session files, or telemetry)
- Tool execution that bypasses the permission system
- Remote code execution via MCP server handshake, plugin install, or skill dispatch
- Authentication bypass in OAuth flows
- Cross-site scripting or XSS-equivalent issues in the desktop webview
- Supply-chain concerns in our own published binaries

## Safe harbor

We will not pursue legal action against researchers who:

- Follow this responsible disclosure policy
- Avoid privacy violations, destruction of data, or service disruption
- Only interact with accounts and systems they own or have explicit permission to test
- Give us reasonable time to fix before public disclosure

Thank you for helping keep thClaws and its users safe.
