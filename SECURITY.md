# Security Policy

## Supported Versions

Suds has no release yet. Until its first release, only the `main` branch
receives security fixes.

## Reporting a Vulnerability

Report a vulnerability privately. Do not open a public issue, pull request,
or discussion for a suspected vulnerability.

- **GitHub (preferred)**: [report a vulnerability](https://github.com/pithecene/suds/security/advisories/new)
  through private vulnerability reporting.
- **Email**: send the report to contact@pithecene.com.

A report states the affected commit or version, the issue and its impact,
the reproduction steps, and any known mitigation.

## Data Handling

Suds sends no telemetry and makes no network request. It reads filesystem
metadata and never the contents of a file. Suds never requests elevated
privileges, and its default profile includes no system-owned path. When the
caller names an explicit root, Suds reads only that root, using the
caller's own permissions. An inspection report can reveal private file and
directory names, so a reporter reviews the report for those names before
attaching it to an issue or a pull request.
