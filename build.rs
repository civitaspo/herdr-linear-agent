// Derived from herdr-projects v0.2.11 (https://github.com/eliasstravik/herdr-projects).
// Copyright (c) 2026 Elias Stravik. MIT License; see NOTICE.

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

// No rerun-if-changed lines on purpose: cargo then reruns this script whenever
// any file in the package changes, so a rebuilt binary always gets a new build
// id and `ticker start` replaces a ticker that runs an older build.
//
// The release version comes from `.release-version`, which the Release PR
// workflow writes; Cargo.toml's version is not bumped by releases.
fn main() {
    let release = std::fs::read_to_string(".release-version").expect(".release-version is missing");
    println!("cargo:rustc-env=HLA_RELEASE_VERSION={}", release.trim());
    let hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "nogit".to_string());
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("cargo:rustc-env=HLA_BUILD_ID={hash}.{secs}");
}
