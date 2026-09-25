# Securefix setup

This repository uses the Client/Server Model around [`civitaspo/securefix-server`](https://github.com/civitaspo/securefix-server). Client workflows request signed machine commits or approvals; the server holds the strong credentials and performs the privileged operation.

## GitHub Apps

Install the Securefix client and server GitHub Apps on this repository and on `civitaspo/securefix-server`.

- The client app needs `issues: write` so it can create request labels on the server repository.
- The server app needs the permissions required by the server workflows, including `contents: write`, `actions: read`, `pull_requests: write`, and `workflows: write` when it updates workflow files.

The `civitaspo-bot` machine user must have write access to this repository for its approval to count toward the branch ruleset.

## Client variables and secret

Configure these values on `civitaspo/herdr-linear-agent`:

| Name | Type | Value or purpose |
| --- | --- | --- |
| `SECUREFIX_CLIENT_APP_ID` | Repository variable | Securefix client GitHub App ID |
| `SECUREFIX_SERVER_REPOSITORY` | Repository variable | `securefix-server` (repository name only) |
| `SECUREFIX_CLIENT_PRIVATE_KEY` | Repository secret | Securefix client GitHub App private key |

Strong credentials such as the server app private key, the `civitaspo-bot` approval token, GPG keys, and repository-settings tokens remain only on `civitaspo/securefix-server`.

## Workflows

- `CI` is the top-level pull request workflow. Its autofix job runs `pinact`, `ghalint`, and checkout-credential checks. If those tools change files, Securefix requests a signed commit through the server.
- `Approve Request` handles trusted pull request events and `/approve` comments from `civitaspo`. Its trusted actor and `allowed_committers` set is `civitaspo`, `cursoragent`, `renovate[bot]`, `dependabot[bot]`, and `civitaspo-securefix-server[bot]`.
- The server Securefix workflow must allow the client workflow name `CI`.
- `Release PR`, `Release PR Sync`, and `Release Tag` call the reusable release workflows on `civitaspo/securefix-server`. The server allows the Securefix client workflow name `Release PR`, lists this repository in `release-clients.yaml` (`publish: github-release`), and protects its tags through `repo-settings/tags-allowlist.json`. See [releasing.md](releasing.md).

No workflow uses `secrets: inherit`; the client private key is passed explicitly to the reusable workflow that needs it.

## Dependency updates and repository settings

Renovate owns version updates for GitHub Actions and mise tools. Non-major updates may be automerged after approval and green checks. Dependabot version updates are not configured; Dependabot security pull requests remain eligible for approval when enabled in GitHub settings.

Squash-only merges, auto-merge, branch-update suggestions, signed commits, the single required `status-check`, and the `civitaspo-bot` collaborator are reconciled from the `repo-settings/` files in `civitaspo/securefix-server`. See the server's [repository settings documentation](https://github.com/civitaspo/securefix-server/blob/main/docs/repo-settings.md).
