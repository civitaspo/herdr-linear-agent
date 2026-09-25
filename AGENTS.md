# Repository Guidelines

## Project scope

herdr-linear-agent is a Herdr plugin written in Rust. It polls Linear for issues delegated to its app user, starts one coordinator agent per issue in a Herdr pane, and lets the coordinator start one worker agent per repository in a Herdr worktree. The plugin binary is the only Linear writer; agents call plugin subcommands and never hold a Linear credential.

## Contribution rules

- Write commits, pull request titles and bodies, documentation, comments, and workflow messages in English only.
- Use Conventional Commits for pull request titles: `feat`, `fix`, `docs`, `refactor`, `test`, `ci`, `build`, `chore`, `perf`, or `revert`. Use `!` for breaking changes.
- Never push directly to `main`. Open a pull request and squash-merge it after the required checks and review pass.
- Sign commits. Do not amend or rewrite commits that have already been merged.
- Keep changes focused. Do not add implementation code to setup-only changes.
- Use the MIT License for this repository. Keep the copyright and permission notices of any imported MIT-licensed source.

## Local tooling

Install the pinned tools with:

```bash
mise install --locked
```

Run these checks before opening a pull request:

```bash
mise run lint
mise run test
mise run build
```

The Rust tasks skip project commands until `Cargo.toml` exists.

## GitHub Actions and credentials

- Pin every GitHub Action to an immutable commit SHA and keep `persist-credentials: false` on checkout steps.
- Keep workflow permissions at the least-privilege level.
- Do not use `secrets: inherit`; pass each secret explicitly.
- Use Securefix for automated workflow fixes and signed machine commits.
- Keep GPG keys, machine-user tokens, server app keys, and other strong credentials only in `civitaspo/securefix-server`.
- See [docs/securefix.md](docs/securefix.md) for the client/server setup.
