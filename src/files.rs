//! Small file helpers shared by every record: atomic writes, JSON records,
//! timestamps and hashes.

// Derived from herdr-projects v0.2.11 (https://github.com/eliasstravik/herdr-projects).
// Copyright (c) 2026 Elias Stravik. MIT License; see NOTICE.

use std::fs::File;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};

/// Writes through a temporary file in the same directory plus a rename. It
/// never creates parent directories: callers create the folders they own.
pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let dir = path.parent().context("path has no parent")?;
    let name = path.file_name().context("path has no file name")?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id()
    ));
    let result = (|| -> Result<()> {
        let mut file = File::create(&tmp)?;
        file.write_all(contents)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.with_context(|| format!("could not write {}", path.display()))
}

pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    write_atomic(path, text.as_bytes())
}

/// The current time as an RFC 3339 string, rounded to the second.
pub fn now() -> String {
    jiff::Timestamp::now()
        .round(jiff::Unit::Second)
        .map(|t| t.to_string())
        .unwrap_or_default()
}

/// Seconds from `timestamp` to `now`; 0 when `timestamp` does not parse.
pub fn seconds_since(timestamp: &str, now: jiff::Timestamp) -> i64 {
    timestamp
        .parse::<jiff::Timestamp>()
        .map(|then| now.as_second() - then.as_second())
        .unwrap_or(0)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Lower-case ASCII letters and digits joined by single hyphens, at most 40
/// characters: safe in a branch name and a file name.
pub fn slugify(text: &str) -> String {
    let mut slug = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    let mut slug: String = slug.chars().take(40).collect();
    while slug.ends_with('-') {
        slug.pop();
    }
    slug
}

/// Single-quotes `value` for a POSIX shell when it contains anything but
/// characters that are safe unquoted.
pub fn shell_quote(value: &str) -> String {
    let safe = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+:@%=,".contains(c));
    if safe {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', r"'\''"))
    }
}

/// Reads a command's text argument: `-` is standard input, anything else a file.
pub fn read_text_arg(path: &str) -> Result<String> {
    if path == "-" {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)
            .context("could not read standard input")?;
        Ok(text)
    } else {
        std::fs::read_to_string(path).with_context(|| format!("could not read {path}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_quotes_and_atomic_writes() {
        assert_eq!(slugify("Fix the $(login) bug!"), "fix-the-login-bug");
        assert_eq!(slugify("???"), "");
        assert_eq!(slugify(&"a".repeat(60)).len(), 40);
        assert_eq!(shell_quote("/bin/hla"), "/bin/hla");
        assert_eq!(shell_quote("/my dir/it's"), r"'/my dir/it'\''s'");

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.json");
        write_json(&path, &serde_json::json!({"a": 1})).unwrap();
        assert_eq!(read_json::<serde_json::Value>(&path).unwrap()["a"], 1);
        let leftovers = std::fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(leftovers, 1);
    }

    #[test]
    fn seconds_since_parses_or_is_zero() {
        let now: jiff::Timestamp = "2026-09-25T12:00:00Z".parse().unwrap();
        assert_eq!(seconds_since("2026-09-25T11:59:00Z", now), 60);
        assert_eq!(seconds_since("garbage", now), 0);
    }
}
