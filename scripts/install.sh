#!/bin/sh
# Puts the herdr-linear-agent binary at target/release/herdr-linear-agent.
#
# Herdr runs this as the plugin's build step. It downloads the prebuilt binary
# of the release named by `.release-version` for this machine (macOS or
# Linux), checks it against its published SHA-256, and falls back to
# `cargo build --release --locked` when there is no such binary or it cannot
# be verified.
#
#   HERDR_LINEAR_AGENT_BUILD=source   always build from source
#
# Derived from herdr-projects v0.2.11 (https://github.com/eliasstravik/herdr-projects).
# Copyright (c) 2026 Elias Stravik. MIT License; see NOTICE.
set -u

cd "$(dirname "$0")/.." || exit 1

say() { printf 'herdr-linear-agent install: %s\n' "$*" >&2; }

build_from_source() {
  [ -n "${tmp:-}" ] && rm -rf "$tmp"
  say "$1"
  say "building from source instead: cargo build --release --locked (this takes a minute or two)"
  if ! command -v cargo >/dev/null 2>&1; then
    say "cargo is not installed. Install Rust (https://rustup.rs), then install the plugin again."
    exit 1
  fi
  exec cargo build --release --locked
}

if [ "${HERDR_LINEAR_AGENT_BUILD:-}" = source ]; then
  build_from_source "HERDR_LINEAR_AGENT_BUILD=source is set"
fi

version=$(tr -d '[:space:]' < .release-version 2>/dev/null)
[ -n "$version" ] || build_from_source ".release-version is missing"
tag="v$version"

case "$(uname -s)" in
  Darwin) os=apple-darwin ;;
  Linux) os=unknown-linux-musl ;;
  *) build_from_source "there is no prebuilt binary for $(uname -s)" ;;
esac
case "$(uname -m)" in
  arm64 | aarch64) arch=aarch64 ;;
  x86_64 | amd64) arch=x86_64 ;;
  *) build_from_source "there is no prebuilt binary for $(uname -m)" ;;
esac
asset="herdr-linear-agent-$arch-$os"

# A prebuilt binary matches only the release commit itself. A checkout with
# local changes, or one on a commit after the release, builds what it has.
if [ -d .git ] || [ -f .git ]; then
  if [ -n "$(git status --porcelain --untracked-files=no 2>/dev/null)" ]; then
    build_from_source "this checkout has uncommitted changes"
  fi
  head=$(git rev-parse HEAD 2>/dev/null)
  release=$(git rev-parse -q --verify "refs/tags/$tag^{commit}" 2>/dev/null)
  if [ -z "$release" ]; then
    # `^{}` is the commit an annotated tag points at; a lightweight tag has none.
    release=$(GIT_TERMINAL_PROMPT=0 git ls-remote origin "refs/tags/$tag" "refs/tags/$tag^{}" 2>/dev/null |
      awk '{ sha = $1 } $2 ~ /\^\{\}$/ { peeled = $1 } END { print (peeled != "" ? peeled : sha) }')
  fi
  if [ -z "$release" ]; then
    build_from_source "could not find the $tag tag here or on origin"
  elif [ "$head" != "$release" ]; then
    build_from_source "this checkout is not the $tag release commit"
  fi
fi

base="https://github.com/civitaspo/herdr-linear-agent/releases/download/$tag"
command -v curl >/dev/null 2>&1 || build_from_source "curl is not installed"
if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | cut -d ' ' -f 1; }
elif command -v shasum >/dev/null 2>&1; then
  sha256() { shasum -a 256 "$1" | cut -d ' ' -f 1; }
else
  build_from_source "neither sha256sum nor shasum is installed to check the download"
fi

mkdir -p target/release
tmp=$(mktemp -d "target/release/.download.XXXXXX") || build_from_source "could not create a download folder"
trap 'rm -rf "$tmp"' EXIT

say "downloading $asset $tag"
curl -fsSL --retry 2 --connect-timeout 10 -o "$tmp/$asset.sha256" "$base/$asset.sha256" ||
  build_from_source "the $tag release has no $asset"
expected=$(cut -d ' ' -f 1 "$tmp/$asset.sha256")
curl -fsSL --retry 2 --connect-timeout 10 -o "$tmp/$asset" "$base/$asset" ||
  build_from_source "could not download $base/$asset"
actual=$(sha256 "$tmp/$asset")
[ "$actual" = "$expected" ] ||
  build_from_source "the downloaded $asset does not match its published SHA-256 (got $actual, expected $expected)"

chmod 755 "$tmp/$asset"
reported=$("$tmp/$asset" --version 2>/dev/null)
case "$reported" in
  "herdr-linear-agent $version" | "herdr-linear-agent $version+"*) ;;
  *) build_from_source "the downloaded binary did not run or is not $version (it said: ${reported:-nothing})" ;;
esac

# Rename, never copy over: macOS kills a binary rewritten in place.
mv -f "$tmp/$asset" target/release/herdr-linear-agent || build_from_source "could not move the binary into target/release"
say "installed the prebuilt $asset $tag"
