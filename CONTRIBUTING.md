# Contributing to Suds

## Prerequisites

The [mise](https://mise.jdx.dev/) version manager installs the pinned
versions of Rust, rustfmt, Clippy, Node.js, pnpm,
[Task](https://taskfile.dev/), and yamlfmt. A contributor runs this command from the repository's root:

```sh
mise install
```

## Development Tasks

| Command | Description |
| :--- | :--- |
| `task build` | Build the release binary |
| `task test` | Run the unit and command-line tests |
| `task lint` | Check formatting, Clippy lints, and YAML formatting |
| `task format` | Format the Rust sources and YAML files (alias `task fmt`) |
| `task check` | Build, test, and lint |
| `task inspect -- ARGS` | Run the debug build of `suds inspect` with `ARGS` |
| `task desktop` | Run the desktop application in development mode |
| `task desktop:check` | Build the desktop frontend, then test and lint the desktop crate |

Each pull request passes `task check` on macOS and on Linux in CI, and
`task desktop:check` on macOS.

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
