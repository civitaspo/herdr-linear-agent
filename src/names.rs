//! Herdr agent names. `agent start` names must be unique among live agents and
//! match `[a-z][a-z0-9_-]{0,31}`, so the issue key is lower-cased and, when a
//! name would not fit, replaced by `i` and eight hex digits of the issue
//! UUID's SHA-256.

// Derived from herdr-projects v0.2.11 (https://github.com/eliasstravik/herdr-projects).
// Copyright (c) 2026 Elias Stravik. MIT License; see NOTICE.

pub const MAX_AGENT_NAME: usize = 32;

fn named(issue_key: &str, issue_id: &str, suffix: &str) -> String {
    let name = format!("{}-{suffix}", issue_key.to_ascii_lowercase());
    if is_valid(&name) {
        return name;
    }
    let hash = crate::files::sha256_hex(issue_id.as_bytes());
    format!("i{}-{suffix}", &hash[..8])
}

/// `<issue-key>-coordinator`, for example `data-123-coordinator`.
pub fn coordinator(issue_key: &str, issue_id: &str) -> String {
    named(issue_key, issue_id, "coordinator")
}

/// `<issue-key>-<worker id>`, for example `data-123-w1`.
pub fn worker(issue_key: &str, issue_id: &str, id: &str) -> String {
    named(issue_key, issue_id, id)
}

pub fn is_valid(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-'))
        && name.len() <= MAX_AGENT_NAME
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_lower_case_and_fall_back_to_a_hash() {
        assert_eq!(coordinator("DATA-123", "u"), "data-123-coordinator");
        assert_eq!(worker("DATA-123", "u", "w2"), "data-123-w2");
        let long = coordinator(
            "VERYLONGTEAMKEY-123456",
            "0d1f6f4e-0000-4000-8000-000000000001",
        );
        assert!(
            long.starts_with('i') && long.ends_with("-coordinator"),
            "{long}"
        );
        assert!(is_valid(&long), "{long}");
        // A key starting with a digit is not a valid name start either.
        assert!(is_valid(&worker("1X-1", "u", "w1")));
        assert!(!is_valid("Hla-x"));
        assert!(!is_valid(""));
    }
}
