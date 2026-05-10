# Security Policy

## Supported Versions

SlashIt is pre-1.0 software. Security fixes are currently provided for the latest release and the `main` branch.

| Version | Supported |
| --- | --- |
| `main` | Yes |
| latest release | Yes |
| older releases | No |

## Reporting a Vulnerability

Please do not open a public GitHub issue for security vulnerabilities.

Report vulnerabilities by emailing <admin@barradev.com> with:

- A concise description of the issue
- Steps to reproduce or a proof of concept, if available
- Affected platform and version
- Any suggested mitigation

You should receive an acknowledgement within 7 days. We will coordinate remediation privately, publish a fix when available, and credit reporters when requested and appropriate.

## Scope

Security reports are especially helpful for:

- Unsafe command execution or shell injection
- Local privilege escalation
- Unauthorized filesystem access
- IPC socket abuse or authentication bypass
- Update signing, release, or distribution issues
- Leaks of secrets, credentials, or private workspace data

## Disclosure

Please give maintainers a reasonable amount of time to investigate and release a fix before public disclosure.
