//! Model family: `<provider>/<model stem>`, the stem being the model id with version
//! numbers, size/context suffixes, dates and thinking-level selectors removed.
//! Entries record the family they were created and kept under (SPEC §Models).

use crate::types::{Family, ModelRef};

/// `family_of("anthropic", "claude-opus-5-5")` → `anthropic/claude-opus`.
///
/// Rules, applied to the lowercased id:
/// - a `:<level>` suffix (`claude-fable-5-1:high`) is dropped;
/// - a `<provider>/` prefix inside the id is used as the provider when `provider` is empty
///   and dropped otherwise;
/// - `-`/`_`-separated tokens that are versions (`5`, `6.1`, `v2`), dates (`20250514`),
///   sizes or context windows (`1m`, `200k`, `70b`, `8x7b`) or `latest` are removed.
///
/// If every token would be removed the whole id is kept as the stem.
pub fn family_of(provider: &str, model_id: &str) -> Family {
    let mut id = model_id.trim().to_ascii_lowercase();
    if let Some(i) = id.find(':') {
        id.truncate(i);
    }
    let mut provider = provider.trim().to_ascii_lowercase();
    if let Some(i) = id.rfind('/') {
        if provider.is_empty() {
            provider = id[..i].to_string();
        }
        id = id[i + 1..].to_string();
    }
    if provider.is_empty() {
        provider = "unknown".to_string();
    }
    let kept: Vec<&str> = id
        .split(['-', '_'])
        .filter(|t| !t.is_empty() && !is_version_token(t))
        .collect();
    let stem = if kept.is_empty() {
        id.clone()
    } else {
        kept.join("-")
    };
    Family(format!("{provider}/{stem}"))
}

pub fn family_of_ref(model: &ModelRef) -> Family {
    family_of(&model.provider, &model.id)
}

/// `"anthropic/claude-opus-5-5"` (a `--model` selector) → `anthropic/claude-opus`.
pub fn family_of_selector(selector: &str) -> Family {
    match selector.split_once('/') {
        Some((provider, id)) => family_of(provider, id),
        None => family_of("", selector),
    }
}

fn is_version_token(t: &str) -> bool {
    if t == "latest" {
        return true;
    }
    let b = t.as_bytes();
    // `5`, `6.1`, `20250514`
    if b.iter().all(|c| c.is_ascii_digit() || *c == b'.') && b[0].is_ascii_digit() {
        return true;
    }
    // `v2`, `v1.5`
    if b[0] == b'v' && b.len() > 1 && b[1..].iter().all(|c| c.is_ascii_digit() || *c == b'.') {
        return true;
    }
    // `1m`, `200k`, `70b`, `8x7b`, `1.5b`
    let (num, unit) = b.split_at(b.len() - 1);
    if matches!(unit[0], b'k' | b'm' | b'b' | b't')
        && !num.is_empty()
        && num[0].is_ascii_digit()
        && num
            .iter()
            .all(|c| c.is_ascii_digit() || *c == b'.' || *c == b'x')
    {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(p: &str, m: &str) -> String {
        family_of(p, m).0
    }

    #[test]
    fn strips_versions_sizes_and_levels() {
        assert_eq!(f("anthropic", "claude-opus-5-5"), "anthropic/claude-opus");
        assert_eq!(
            f("anthropic", "claude-fable-5-1:high"),
            "anthropic/claude-fable"
        );
        assert_eq!(
            f("github-copilot", "gemini-4-argon"),
            "github-copilot/gemini-argon"
        );
        assert_eq!(
            f("github-copilot", "gpt-6.1-sol-1m"),
            "github-copilot/gpt-sol"
        );
        assert_eq!(f("openai-codex", "gpt-6-luna"), "openai-codex/gpt-luna");
        assert_eq!(
            f("anthropic", "claude-3-5-sonnet-20241022"),
            "anthropic/claude-sonnet"
        );
        assert_eq!(f("x", "llama-3.1-70b-instruct"), "x/llama-instruct");
        assert_eq!(f("x", "mixtral-8x7b-v0.1"), "x/mixtral");
        assert_eq!(f("x", "model-latest"), "x/model");
    }

    #[test]
    fn same_family_across_versions_differs_across_providers() {
        assert_eq!(
            family_of("anthropic", "claude-opus-5-5"),
            family_of("anthropic", "claude-opus-4-1")
        );
        assert_ne!(
            family_of("anthropic", "claude-opus-5-5"),
            family_of("anthropic", "claude-fable-5-1")
        );
        assert_ne!(
            family_of("anthropic", "claude-opus-5-5"),
            family_of("bedrock", "claude-opus-5-5")
        );
    }

    #[test]
    fn provider_prefix_and_degenerate_ids() {
        assert_eq!(
            f("", "github-copilot/gpt-6.1-sol-1m"),
            "github-copilot/gpt-sol"
        );
        assert_eq!(
            f("anthropic", "anthropic/claude-opus-5-5"),
            "anthropic/claude-opus"
        );
        assert_eq!(
            family_of_selector("anthropic/claude-opus-5-5").0,
            "anthropic/claude-opus"
        );
        // Keeps tokens that merely contain digits (`4o`, `o3`) and never yields an empty stem.
        assert_eq!(f("openai", "gpt-4o"), "openai/gpt-4o");
        assert_eq!(f("openai", "o3"), "openai/o3");
        assert_eq!(f("x", "2024"), "x/2024");
        assert_eq!(f("", "solo"), "unknown/solo");
        assert_eq!(f("Anthropic", "Claude-Opus-5-5"), "anthropic/claude-opus");
    }
}
