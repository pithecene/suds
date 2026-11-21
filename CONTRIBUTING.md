# Contributing to Suds

## Prerequisites

The [mise](https://mise.jdx.dev/) version manager installs the pinned
versions of Rust, rustfmt, Clippy, Node.js, pnpm,
[Task](https://taskfile.dev/), and yamlfmt. A contributor runs this command from the repository's root:

```sh
mise install
```

## Development Tasks

The README's Development section lists each task and what it runs.

Each pull request passes `task check` on macOS and on Linux in CI, and
`task webview:check` on macOS.

## Tests

Tests build temporary fixture trees and never read the home directory of
the test machine. A test that depends on one platform carries a
`cfg` gate for that platform. When the environment cannot create a
condition that a test requires, such as a mount point, the test returns
early and states the reason.

## Code Style

- The `task format` command formats the Rust sources and YAML files.
- Clippy runs with every warning denied.
- [AGENTS.md](AGENTS.md) states the guardrails that apply to every change.

## Commits

Use [Conventional Commits](https://www.conventionalcommits.org/), with a
[gitmoji](https://gitmoji.dev/) after the scope:

```text
type(scope): <emoji> short imperative description
```

These commit subjects illustrate the format:

```text
feat(inspect): ✨ report skipped mount points
fix(scan): 🐛 count a broken symbolic link once
docs(readme): 📝 describe the exit statuses
```

## Pull Requests

- Branch from `main`, and never commit to `main` directly.
- Title each pull request in the format of a commit subject.
- Keep each pull request to one concern.

## Security

Report a vulnerability privately, as [SECURITY.md](SECURITY.md) describes,
and never in a public issue or pull request.

## License

Each contributor licenses every contribution to Suds under the Apache
License 2.0.
