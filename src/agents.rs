//! What the binary knows about Herdr agent kinds: the kinds `agent start`
//! accepts, the per-kind resume arguments from Herdr's session-state page
//! (herdr.dev, "Native agent session restore", 0.9.1), and how a profile's
//! model and effort become command-line flags.

// Derived from herdr-projects v0.2.11 (https://github.com/eliasstravik/herdr-projects).
// Copyright (c) 2026 Elias Stravik. MIT License; see NOTICE.

use crate::config::Profile;

pub const KINDS: [&str; 24] = [
    "pi",
    "claude",
    "codex",
    "gemini",
    "cursor",
    "devin",
    "agy",
    "cline",
    "omp",
    "mastracode",
    "opencode",
    "copilot",
    "kimi",
    "kiro",
    "droid",
    "amp",
    "grok",
    "hermes",
    "kilo",
    "qodercli",
    "qwen",
    "letta",
    "maki",
    "muse",
];

pub fn is_kind(kind: &str) -> bool {
    KINDS.contains(&kind)
}

/// Kinds whose CLI takes a reasoning-effort flag. Any other kind's profile
/// may not set `effort`.
pub fn has_effort_flag(kind: &str) -> bool {
    matches!(kind, "claude" | "codex")
}

/// The arguments that resume a native session `id` for `kind`, or `None` for
/// a kind whose resume command Herdr does not document.
pub fn resume_args(kind: &str, id: &str) -> Option<Vec<String>> {
    if id.is_empty() || id.starts_with('-') {
        return None;
    }
    let args: Vec<String> = match kind {
        "claude" | "cursor" | "grok" | "devin" | "droid" | "qodercli" | "qwen" | "hermes" => {
            vec!["--resume".into(), id.into()]
        }
        "codex" => vec!["resume".into(), id.into()],
        "omp" | "copilot" => vec![format!("--resume={id}")],
        "pi" | "opencode" | "kimi" | "kilo" => vec!["--session".into(), id.into()],
        "agy" | "letta" => vec!["--conversation".into(), id.into()],
        "mastracode" => vec!["--thread".into(), id.into()],
        _ => return None,
    };
    Some(args)
}

/// The agent CLI arguments for a profile: the model flag, the effort flag,
/// then the user's own `args`. Herdr has no model or effort option of its
/// own; `agent start` passes everything after `--` to the agent CLI.
pub fn profile_args(profile: &Profile) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(model) = &profile.model {
        match profile.kind.as_str() {
            "codex" => args.extend(["-m".to_string(), model.clone()]),
            _ => args.extend(["--model".to_string(), model.clone()]),
        }
    }
    if let Some(effort) = &profile.effort {
        match profile.kind.as_str() {
            "codex" => args.extend(["-c".to_string(), format!("model_reasoning_effort={effort}")]),
            "claude" => args.extend(["--effort".to_string(), effort.clone()]),
            // Refused when the config is loaded.
            _ => {}
        }
    }
    args.extend(profile.args.iter().cloned());
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(kind: &str, model: Option<&str>, effort: Option<&str>, args: &[&str]) -> Profile {
        Profile {
            kind: kind.into(),
            model: model.map(Into::into),
            effort: effort.map(Into::into),
            args: args.iter().map(|a| a.to_string()).collect(),
            description: String::new(),
        }
    }

    #[test]
    fn profiles_become_per_kind_flags() {
        assert_eq!(
            profile_args(&profile(
                "claude",
                Some("opus"),
                Some("high"),
                &["--permission-mode", "auto"]
            )),
            [
                "--model",
                "opus",
                "--effort",
                "high",
                "--permission-mode",
                "auto"
            ]
        );
        assert_eq!(
            profile_args(&profile(
                "codex",
                Some("gpt-6-sol"),
                Some("xhigh"),
                &["-s", "workspace-write"]
            )),
            [
                "-m",
                "gpt-6-sol",
                "-c",
                "model_reasoning_effort=xhigh",
                "-s",
                "workspace-write"
            ]
        );
        assert_eq!(
            profile_args(&profile("cursor", Some("gpt-5-high"), None, &[])),
            ["--model", "gpt-5-high"]
        );
        assert!(profile_args(&profile("claude", None, None, &[])).is_empty());
        assert!(
            has_effort_flag("claude") && has_effort_flag("codex") && !has_effort_flag("cursor")
        );
    }

    #[test]
    fn resume_arguments_follow_herdrs_table() {
        assert_eq!(resume_args("claude", "abc").unwrap(), ["--resume", "abc"]);
        assert_eq!(resume_args("codex", "abc").unwrap(), ["resume", "abc"]);
        assert_eq!(
            resume_args("opencode", "abc").unwrap(),
            ["--session", "abc"]
        );
        assert_eq!(resume_args("copilot", "abc").unwrap(), ["--resume=abc"]);
        assert_eq!(resume_args("gemini", "abc"), None);
        assert_eq!(resume_args("claude", ""), None);
        assert_eq!(resume_args("claude", "--dangerous"), None);
        assert!(is_kind("claude") && !is_kind("chatgpt"));
    }
}
