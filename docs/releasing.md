# Releasing

This repository uses [csm-ci-actions](https://github.com/civitaspo/csm-ci-actions/blob/main/docs/release-pr.md) hosted by [civitaspo/csm-ci-server](https://github.com/civitaspo/csm-ci-server).

## Flow

1. Squash-merge reviewed changes to `main` through `/merge`.
2. **csm-ci-release-pr** runs git-cliff and asks the server for signed commits on `release/next`. It changes only `.release-version` and `CHANGELOG.md`. The caller sets `update-root-version: false`; releases retain the fixed versions in `Cargo.toml`, `Cargo.lock`, and `herdr-plugin.toml`.
3. Review `release-pr: chore(release): vX.Y.Z`, request `/approve` for its current head, and wait for full CI, policy, and approval before requesting `/merge`. Release PRs are never automerged.
4. **csm-ci-release** requests publication. The server verifies the merged release PR and creates an annotated tag at its squash merge. It builds all four binaries without credentials, executes each on its native runner to verify its version, and checks its architecture and static musl linkage on Linux.
5. The publisher generates individual checksums and `SHA256SUMS`, attests all nine assets, uploads them to a draft, and publishes the immutable GitHub Release only after the complete asset set is present.

The server uses `release.build_type: cargo-raw`, binary `herdr-linear-agent`, and exactly these targets:

| Target | Binary asset |
| --- | --- |
| `aarch64-apple-darwin` | `herdr-linear-agent-aarch64-apple-darwin` |
| `x86_64-apple-darwin` | `herdr-linear-agent-x86_64-apple-darwin` |
| `x86_64-unknown-linux-musl` | `herdr-linear-agent-x86_64-unknown-linux-musl` |
| `aarch64-unknown-linux-musl` | `herdr-linear-agent-aarch64-unknown-linux-musl` |

Each binary has a `<binary asset>.sha256` sidecar. `scripts/install.sh` retains its existing download names and verifies that sidecar before installing; its source-build fallback remains available. The aggregate `SHA256SUMS` and attestations add verification without changing the installer contract. Publication no longer relies on uploading assets after an immutable release is published.

## Versions and changelog

`.release-version` is the release version reported by the binary. `build.rs` appends its existing git-hash and timestamp build metadata. Cargo and plugin manifest versions are deliberately independent and remain fixed.

While major is zero, git-cliff bumps a breaking change or `feat:` to the next minor version; other releasable commits bump the patch version. `cliff.toml` controls that policy. Preparation skips release commits to avoid duplicate PRs before the new tag exists. Do not edit `CHANGELOG.md` in feature PRs.

Preview the next version or request a real explicit release PR:

```bash
mise exec -- git cliff --bumped-version
gh workflow run csm-ci-release-pr.yml -R civitaspo/herdr-linear-agent -f version=X.Y.Z
```

The explicit version must be semantic and omit the `v` prefix. Published tags and releases are permanent. Existing immutable releases with missing binaries cannot be repaired by adding assets; publish a new reviewed release instead.

## Credentials and recovery

Only the Client App key lives here; see [securefix.md](securefix.md). Strong credentials stay in the server's `csm-ci` environment. The managed `release-tags` ruleset restricts `v*` creation to the Server App, while general tag integrity remains active.

If preparation or publication fails, inspect the client request and server run before retrying. Require green full CI on the final PR head. Do not force-update a published tag or bypass checks. Before adopting an existing tag, verify that it identifies the intended squash merge.
