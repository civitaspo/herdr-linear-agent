# Releasing

This repository uses the shared Securefix client release flow hosted in [`civitaspo/securefix-server`](https://github.com/civitaspo/securefix-server). The canonical specification is [client-releases.md](https://github.com/civitaspo/securefix-server/blob/main/docs/client-releases.md).

## Flow

1. Commits land on `main` through squash-merged pull requests.
2. **Release PR** runs git-cliff, writes `.release-version` and `CHANGELOG.md`, and asks Securefix to open or update `release/next`.
3. **Release PR Sync** keeps the open `release/next` pull request title and body aligned with `.release-version`.
4. A human squash-merges `chore(release): vX.Y.Z`. This merge is the only human gate; release pull requests are never automerged.
5. **Release Tag** creates the annotated tag `vX.Y.Z` on the merge commit and creates a `release-request-*` label on `civitaspo/securefix-server`.
6. The server **Release** workflow checks `release-clients.yaml` and publishes a GitHub Release.

The workflows in `.github/workflows/release-*.yml` are thin wrappers around the server's reusable workflows, pinned to a commit SHA, except **Release Assets**.

**Release Assets** runs when a release is published (or by hand with a tag). It builds the binary at the tag for macOS (`aarch64-apple-darwin`, `x86_64-apple-darwin`) and for Linux as static musl binaries (`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`), checks that it reports the tag's version, and uploads `herdr-linear-agent-<target>` with its `herdr-linear-agent-<target>.sha256` to the release with the workflow's own token. `scripts/install.sh`, the plugin's build step, downloads the binary that matches `.release-version` and falls back to `cargo build --release --locked`.

`.release-version` is the release version: the binary reports it (`herdr-linear-agent --version`). The versions in `Cargo.toml` and `herdr-plugin.toml` are not bumped by releases.

## Versions

While major is 0, git-cliff bumps versions from Conventional Commit pull request titles:

- breaking change (`type!:` or `BREAKING CHANGE`) → minor
- `feat:` → minor
- any other releasable commit → patch
- only `chore(release):` commits since the last tag → nothing to release

Automatic bumps start from the latest stable tag (`vX.Y.Z`). Until the first stable tag exists, git-cliff returns `v0.0.0` and Release PR does nothing, so the first release needs an explicit version.

## Explicit and pre-release versions

Run **Release PR** from the Actions tab (`workflow_dispatch`) with `version` set, without the leading `v`:

```bash
gh workflow run release-pr.yml -R civitaspo/herdr-linear-agent -f version=0.0.1-pre.1
```

- A semver pre-release part is allowed (`0.0.1-pre.1`, `0.0.1-pre.2`, ...). Use a dot before the number so that `pre.10` sorts after `pre.9`.
- Pre-release tags are published as GitHub pre-releases and never become a changelog or bump boundary.
- The next push to `main` may rewrite an open pre-release `release/next` pull request to the computed version. Dispatch again with `version` to restore it.
- Every published tag is permanent: the `all-tags` ruleset blocks tag deletion and force updates.

## Changelog

Do not edit `CHANGELOG.md` on feature pull requests. The Release PR workflow regenerates it from git history.

Preview locally:

```bash
mise install --locked
mise exec -- git cliff --bumped-version
mise exec -- git cliff --tag vX.Y.Z
```

## Credentials

The repository secret `SECUREFIX_CLIENT_PRIVATE_KEY` is the only release credential in this repository. Publishing runs on `civitaspo/securefix-server`. See [securefix.md](securefix.md).
