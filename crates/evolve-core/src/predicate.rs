//! Trigger predicates (DESIGN §6): derivation on stage, load-time match against
//! repo/cwd, count-time match against a finished session's exts and stems.

use std::path::Path;

use glob::{MatchOptions, Pattern};
use sha2::{Digest, Sha256};

use crate::types::{Predicate, Sha256Hex};

pub const MAX_EXTS: usize = 8;
pub const MAX_STEMS: usize = 10;
/// Repo glob used when the originating session had no repo.
pub const ANY_REPO: &str = "**";

/// Lowercase, strip leading dots and whitespace: `.RS` → `rs`.
pub fn normalize_ext(e: &str) -> String {
    e.trim().trim_start_matches('.').to_ascii_lowercase()
}

fn normalize_stem(s: &str) -> String {
    s.trim().to_lowercase()
}

fn dedup_capped(items: impl IntoIterator<Item = String>, cap: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for i in items {
        if i.is_empty() || out.contains(&i) {
            continue;
        }
        if out.len() == cap {
            break;
        }
        out.push(i);
    }
    out
}

/// Build the predicate written at stage time. `ranked_stems` come already ranked
/// (tf-idf, `features` module); order is preserved and the first 10 kept.
pub fn derive(repo: Option<&str>, exts: &[String], ranked_stems: &[String]) -> Predicate {
    let repo_glob = match repo.map(|r| r.trim_end_matches('/')).filter(|r| !r.is_empty()) {
        Some(r) => Pattern::escape(r),
        None => ANY_REPO.to_string(),
    };
    Predicate {
        repo_glob,
        file_exts: dedup_capped(exts.iter().map(|e| normalize_ext(e)), MAX_EXTS),
        stems: dedup_capped(ranked_stems.iter().map(|s| normalize_stem(s)), MAX_STEMS),
    }
}

/// Order-insensitive identity of a predicate (rate limits, sibling routing).
pub fn predicate_hash(p: &Predicate) -> Sha256Hex {
    let mut exts: Vec<String> = p.file_exts.iter().map(|e| normalize_ext(e)).collect();
    exts.sort();
    exts.dedup();
    let mut stems: Vec<String> = p.stems.iter().map(|s| normalize_stem(s)).collect();
    stems.sort();
    stems.dedup();
    let canon = serde_json::json!({
        "repo_glob": p.repo_glob.trim_end_matches('/'),
        "file_exts": exts,
        "stems": stems,
    });
    hex(&Sha256::digest(canon.to_string().as_bytes()))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Load-time match: the repo glob matches `repo_or_cwd` or one of its ancestors
/// (a repo-path glob therefore matches any cwd inside that repo). `**` or an empty
/// glob matches everything.
pub fn matches_repo(p: &Predicate, repo_or_cwd: &str) -> bool {
    let glob = p.repo_glob.trim();
    if glob.is_empty() || glob == ANY_REPO {
        return true;
    }
    let Ok(pat) = Pattern::new(glob.trim_end_matches('/')) else {
        return false;
    };
    let opts = MatchOptions {
        case_sensitive: true,
        require_literal_separator: false,
        require_literal_leading_dot: false,
    };
    let target = repo_or_cwd.trim_end_matches('/');
    let target = if target.is_empty() { "/" } else { target };
    Path::new(target)
        .ancestors()
        .any(|a| pat.matches_with(&a.to_string_lossy(), opts))
}

/// Count-time content match (DESIGN §6), excluding the glob:
/// (`file_exts` empty OR overlap ≥ 1) AND (`stems` empty OR overlap ≥ min(2, |stems|)).
pub fn count_hit(p: &Predicate, exts: &[String], stems: &[String]) -> bool {
    let ext_ok = p.file_exts.is_empty() || {
        let have: Vec<String> = exts.iter().map(|e| normalize_ext(e)).collect();
        p.file_exts.iter().any(|e| have.contains(&normalize_ext(e)))
    };
    if !ext_ok {
        return false;
    }
    if p.stems.is_empty() {
        return true;
    }
    let have: Vec<String> = stems.iter().map(|s| normalize_stem(s)).collect();
    let mine = dedup_capped(p.stems.iter().map(|s| normalize_stem(s)), usize::MAX);
    let overlap = mine.iter().filter(|s| have.contains(s)).count();
    overlap >= mine.len().min(2)
}

/// Full count-time predicate hit: glob AND content.
pub fn count_match(p: &Predicate, repo_or_cwd: &str, exts: &[String], stems: &[String]) -> bool {
    matches_repo(p, repo_or_cwd) && count_hit(p, exts, stems)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn pred(glob: &str, exts: &[&str], stems: &[&str]) -> Predicate {
        Predicate { repo_glob: glob.into(), file_exts: s(exts), stems: s(stems) }
    }

    #[test]
    fn derive_caps_normalizes_and_escapes() {
        let exts = s(&[".RS", "rs", "toml", "a", "b", "c", "d", "e", "f", "g"]);
        let stems: Vec<String> = (0..14).map(|i| format!("Stem{i}")).collect();
        let p = derive(Some("/w/repo[1]/"), &exts, &stems);
        assert_eq!(p.repo_glob, "/w/repo[[]1[]]");
        assert_eq!(p.file_exts, s(&["rs", "toml", "a", "b", "c", "d", "e", "f"]));
        assert_eq!(p.stems.len(), 10);
        assert_eq!(p.stems[0], "stem0");
        assert!(matches_repo(&p, "/w/repo[1]"));
        assert!(!matches_repo(&p, "/w/repo1"));
        assert_eq!(derive(None, &[], &[]).repo_glob, "**");
    }

    #[test]
    fn hash_is_order_insensitive_and_content_sensitive() {
        let a = pred("/r", &["rs", "md"], &["ledger", "count"]);
        let b = pred("/r/", &[".md", "RS"], &["count", "ledger"]);
        assert_eq!(predicate_hash(&a), predicate_hash(&b));
        let c = pred("/r", &["rs", "md"], &["ledger"]);
        assert_ne!(predicate_hash(&a), predicate_hash(&c));
        assert_eq!(predicate_hash(&a).len(), 64);
    }

    #[test]
    fn repo_glob_matches_path_or_ancestor() {
        let p = pred("/w/repo", &[], &[]);
        assert!(matches_repo(&p, "/w/repo"));
        assert!(matches_repo(&p, "/w/repo/crates/x"));
        assert!(!matches_repo(&p, "/w/repo2"));
        assert!(!matches_repo(&p, "/w"));
        let g = pred("/w/*/svc", &[], &[]);
        assert!(matches_repo(&g, "/w/a/svc"));
        assert!(!matches_repo(&g, "/w/a/api"));
        let deep = pred("/w/**/svc", &[], &[]);
        assert!(matches_repo(&deep, "/w/a/b/svc/src"));
        assert!(matches_repo(&pred("**", &[], &[]), "/anything"));
        assert!(matches_repo(&pred("", &[], &[]), "/anything"));
    }

    #[test]
    fn count_hit_ext_rule() {
        let p = pred("**", &["rs"], &[]);
        assert!(count_hit(&p, &s(&["md", ".RS"]), &[]));
        assert!(!count_hit(&p, &s(&["md"]), &[]));
        assert!(!count_hit(&p, &[], &[]));
        assert!(count_hit(&pred("**", &[], &[]), &[], &[]));
    }

    #[test]
    fn count_hit_stem_threshold_is_min_two() {
        let one = pred("**", &[], &["ledger"]);
        assert!(count_hit(&one, &[], &s(&["ledger"])));
        assert!(!count_hit(&one, &[], &s(&["count"])));
        let many = pred("**", &[], &["ledger", "count", "decid"]);
        assert!(!count_hit(&many, &[], &s(&["ledger", "other"])));
        assert!(count_hit(&many, &[], &s(&["ledger", "decid"])));
        // both conjuncts required
        let both = pred("**", &["rs"], &["ledger", "count"]);
        assert!(!count_hit(&both, &s(&["md"]), &s(&["ledger", "count"])));
        assert!(count_hit(&both, &s(&["rs"]), &s(&["ledger", "count"])));
    }

    #[test]
    fn count_match_requires_glob() {
        let p = pred("/w/repo", &[], &[]);
        assert!(count_match(&p, "/w/repo/sub", &[], &[]));
        assert!(!count_match(&p, "/elsewhere", &[], &[]));
    }
}
