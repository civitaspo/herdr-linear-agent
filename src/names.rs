//! Herdr agent names for a run's coordinator and workers.

use crate::files::sha256_hex;

/// `<key>-coordinator`, for example `data-123-coordinator`.
pub fn coordinator(key: &str, issue_id: &str) -> String {
    named(key, issue_id, "coordinator")
}

/// `<key>-<id>`, for example `data-123-w2`.
pub fn worker(key: &str, issue_id: &str, id: &str) -> String {
    named(key, issue_id, id)
}

/// Herdr's rule for agent names: `[a-z][a-z0-9_-]{0,31}`.
fn is_valid(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && name.len() <= 32
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// The lower-case key and the suffix, or, when that is not a valid name, a
/// stand-in for the key made from the issue UUID.
fn named(key: &str, issue_id: &str, suffix: &str) -> String {
    let plain = format!("{}-{suffix}", key.to_ascii_lowercase());
    if is_valid(&plain) {
        return plain;
    }
    let hash = sha256_hex(issue_id.as_bytes());
    format!("i{}-{suffix}", &hash[..8])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_use_the_lower_case_key() {
        assert_eq!(coordinator("DATA-123", "u-1"), "data-123-coordinator");
        assert_eq!(worker("DATA-123", "u-1", "w2"), "data-123-w2");
    }

    #[test]
    fn a_key_that_cannot_be_a_name_becomes_a_hash_of_the_issue() {
        // sha256("issue-uuid") = 6d0c47b2...
        assert_eq!(
            coordinator("VERYLONGTEAMKEY-123456", "issue-uuid"),
            "i6d0c47b2-coordinator"
        );
        assert_eq!(worker("9LIVES-1", "issue-uuid", "w1"), "i6d0c47b2-w1");
        assert!(is_valid(&coordinator(
            "VERYLONGTEAMKEY-123456",
            "issue-uuid"
        )));
    }

    #[test]
    fn validity_follows_herdrs_pattern() {
        for good in ["a", "data-1-w1", "x_y", &"a".repeat(32)] {
            assert!(is_valid(good), "{good}");
        }
        for bad in ["", "Hla-x", "1abc", "-a", "a.b", &"a".repeat(33)] {
            assert!(!is_valid(bad), "{bad}");
        }
    }
}
