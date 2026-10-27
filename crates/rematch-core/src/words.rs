//! Bellwether's `words` tokenizer (a `text:<column>,tokenizer=words` key, its B5): a value
//! is case-folded code point by code point (Unicode full case folding, `unicase`), then
//! split at Unicode word boundaries (UAX #29, `unicode-segmentation`), both at the versions
//! Bellwether pins. The lake's `text:tool_cmd` residual and the no-lake scans of `create
//! --ran` use it, so `cd x && cargo test --workspace` has the words `cd`, `x`, `cargo`,
//! `test`, `workspace` everywhere.

use unicode_segmentation::UnicodeSegmentation;

/// `value` case-folded (`ß` → `ss`, `K` → `k`), never normalized.
pub fn fold(value: &str) -> String {
    unicase::UniCase::unicode(value).to_folded_case()
}

/// The words of `value` after folding, in order, repeats included.
pub fn words(value: &str) -> Vec<String> {
    fold(value).unicode_words().map(str::to_string).collect()
}

/// The distinct words of every argument, in first-seen order (`["cargo test", "Cargo"]` →
/// `["cargo", "test"]`): what `--has`/`--ran` require, one word each.
pub fn tokens<S: AsRef<str>>(args: &[S]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for a in args {
        for w in words(a.as_ref()) {
            if !out.contains(&w) {
                out.push(w);
            }
        }
    }
    out
}

/// One of `value`'s words equals `token` (both folded): Bellwether's `has_token`.
pub fn has_token(value: &str, token: &str) -> bool {
    let t = fold(token);
    fold(value).unicode_words().any(|w| w == t)
}

/// Every token is one of `value`'s words.
pub fn has_all(value: &str, tokens: &[String]) -> bool {
    let folded = fold(value);
    let have: Vec<&str> = folded.unicode_words().collect();
    !tokens.is_empty() && tokens.iter().all(|t| have.contains(&fold(t).as_str()))
}

/// One of `value`'s words starts with `prefix` (both folded): Bellwether's `has_prefix`.
pub fn has_prefix(value: &str, prefix: &str) -> bool {
    let p = fold(prefix);
    fold(value).unicode_words().any(|w| w.starts_with(&p))
}

/// `value` contains `needle` (both folded): Bellwether's `contains`.
pub fn contains(value: &str, needle: &str) -> bool {
    fold(value).contains(&fold(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_split_into_folded_words() {
        assert_eq!(words("cd x && cargo test --workspace"), ["cd", "x", "cargo", "test", "workspace"]);
        assert_eq!(words("STRASSE Straße"), ["strasse", "strasse"]);
        assert_eq!(tokens(&["cargo test", "Cargo", ""]), ["cargo", "test"]);
        assert!(tokens::<&str>(&["&&", "--"]).is_empty());
    }

    #[test]
    fn the_four_matches() {
        let cmd = "cd x && cargo test --workspace";
        assert!(has_token(cmd, "CARGO") && !has_token(cmd, "carg"));
        assert!(has_all(cmd, &["cargo".into(), "test".into()]) && !has_all(cmd, &["cargo".into(), "build".into()]));
        assert!(!has_all(cmd, &[]));
        assert!(has_prefix(cmd, "work") && !has_prefix(cmd, "orks"));
        assert!(contains(cmd, "O TE") && !contains(cmd, "cargo  test"));
    }
}
