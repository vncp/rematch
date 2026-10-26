//! Trigger predicates (DESIGN §6): derivation on stage, load-time match against
//! repo/cwd, count-time match against a finished session's exts and stems.

use std::path::{Path, PathBuf};

use glob::{MatchOptions, Pattern};
use sha2::{Digest, Sha256};

use crate::types::{Predicate, SessionRecord, Sha256Hex};

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
/// (tf-idf, `features` module); order is preserved and the first 10 kept. When `repo` is a
/// git worktree on this machine, `repo_remote` is its identity (`identity::repo_remote_of`,
/// file reads only) and `subpath` = `**` (the whole repo, like the glob).
pub fn derive(repo: Option<&str>, exts: &[String], ranked_stems: &[String]) -> Predicate {
    let repo = repo.map(|r| r.trim_end_matches('/')).filter(|r| !r.is_empty());
    let repo_glob = match repo {
        Some(r) => Pattern::escape(r),
        None => ANY_REPO.to_string(),
    };
    let repo_remote = repo.and_then(|r| crate::identity::repo_remote_of(Path::new(r)));
    Predicate {
        repo_glob,
        file_exts: dedup_capped(exts.iter().map(|e| normalize_ext(e)), MAX_EXTS),
        stems: dedup_capped(ranked_stems.iter().map(|s| normalize_stem(s)), MAX_STEMS),
        subpath: repo_remote.as_ref().map(|_| ANY_REPO.to_string()),
        repo_remote,
    }
}

/// A path as a glob matching exactly it (what [`derive`] writes as `repo_glob`).
pub fn escape_glob(path: &str) -> String {
    Pattern::escape(path)
}

/// The literal path a derived repo glob names (`predicate::derive` escapes the toplevel with
/// `Pattern::escape`); `None` for a real glob (`**`, `*`, `?`, `[…]` classes).
pub fn literal_path(glob: &str) -> Option<PathBuf> {
    let g = glob.trim();
    if g.is_empty() || g == ANY_REPO {
        return None;
    }
    let mut out = String::with_capacity(g.len());
    let mut rest = g;
    while let Some(c) = rest.chars().next() {
        if c == '[' {
            // Only the one-char classes `Pattern::escape` writes: `[[]`, `[]]`, `[*]`, `[?]`.
            let esc = rest.get(..3).filter(|s| matches!(*s, "[[]" | "[]]" | "[*]" | "[?]"))?;
            out.push(esc.as_bytes()[1] as char);
            rest = &rest[3..];
            continue;
        }
        if matches!(c, '*' | '?' | ']') {
            return None;
        }
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    Some(PathBuf::from(out))
}

/// Order-insensitive identity of a predicate (rate limits, sibling routing). `repo_remote`
/// and `subpath` are left out: they restate the glob's repo, and a hash that changed when
/// `lake migrate` filled them would split one trigger into two.
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
    // Symlinked roots (`/tmp` → `/private/tmp` on macOS): a glob derived from the git toplevel
    // (realpath) must still match a cwd given in its symlinked spelling, and vice versa.
    let mut candidates = vec![PathBuf::from(target)];
    if let Ok(real) = std::fs::canonicalize(target) {
        if real != candidates[0] {
            candidates.push(real);
        }
    }
    candidates
        .iter()
        .flat_map(|c| c.ancestors())
        .any(|a| pat.matches_with(&a.to_string_lossy(), opts))
}

/// Where a session runs (or a `match` is asked for): the repo toplevel (else cwd) path for
/// the glob, and the repo identity when known.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Place {
    pub path: String,
    pub remote: Option<String>,
    /// cwd relative to the toplevel (`""` at the top).
    pub subpath: Option<String>,
}

impl Place {
    /// A path with no identity: the glob decides.
    pub fn path(path: impl Into<String>) -> Place {
        Place { path: path.into(), remote: None, subpath: None }
    }

    /// Where a recorded session ran: its repo (else cwd), remote and subpath.
    pub fn of_session(s: &SessionRecord) -> Place {
        Place {
            path: s.repo.clone().unwrap_or_else(|| s.cwd.clone()),
            remote: s.repo_remote.clone(),
            subpath: s.subpath.clone(),
        }
    }

    /// The place of `cwd` in the worktree `repo` (else of `cwd` alone), its identity read
    /// from disk (`identity::session_place`, file reads only).
    pub fn locate(repo: Option<&Path>, cwd: &Path) -> Place {
        let repo = repo.map(|r| r.to_string_lossy().into_owned());
        let cwd_s = cwd.to_string_lossy();
        let (remote, subpath) = crate::identity::session_place(repo.as_deref(), &cwd_s);
        Place { path: repo.unwrap_or_else(|| cwd_s.into_owned()), remote, subpath }
    }
}

/// `subpath` glob over a toplevel-relative path: `None`, empty or `**` match everything;
/// otherwise the glob matches the path or one of its ancestors.
pub fn matches_subpath(glob: Option<&str>, subpath: &str) -> bool {
    let glob = glob.map(|g| g.trim().trim_matches('/')).unwrap_or("");
    if glob.is_empty() || glob == ANY_REPO {
        return true;
    }
    let Ok(pat) = Pattern::new(glob) else {
        return false;
    };
    let opts = MatchOptions { case_sensitive: true, require_literal_separator: false, require_literal_leading_dot: false };
    Path::new(subpath.trim_matches('/')).ancestors().any(|a| pat.matches_with(&a.to_string_lossy(), opts))
}

/// Load- and count-time place test (DESIGN §6): when both the predicate and the place carry
/// a remote, the remotes are equal and the `subpath` glob matches (a worktree or a clone at
/// another path matches its repo's entries); otherwise the repo glob decides ([`matches_repo`]).
pub fn matches_place(p: &Predicate, place: &Place) -> bool {
    match (p.repo_remote.as_deref(), place.remote.as_deref()) {
        (Some(want), Some(have)) => {
            want == have && matches_subpath(p.subpath.as_deref(), place.subpath.as_deref().unwrap_or(""))
        }
        _ => matches_repo(p, &place.path),
    }
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

/// Full count-time predicate hit: place ([`matches_place`]) AND content.
pub fn count_match(p: &Predicate, place: &Place, exts: &[String], stems: &[String]) -> bool {
    matches_place(p, place) && count_hit(p, exts, stems)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn pred(glob: &str, exts: &[&str], stems: &[&str]) -> Predicate {
        Predicate { repo_glob: glob.into(), file_exts: s(exts), stems: s(stems), ..Default::default() }
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
    fn literal_path_unescapes_derived_globs_only() {
        let p = escape_glob("/w/repo[1]*?");
        assert_eq!(literal_path(&p), Some(PathBuf::from("/w/repo[1]*?")));
        assert_eq!(literal_path("/w/plain"), Some(PathBuf::from("/w/plain")));
        assert_eq!(literal_path("**"), None);
        assert_eq!(literal_path("/w/*/svc"), None);
        assert_eq!(literal_path("/w/[ab]"), None);
        assert_eq!(literal_path(""), None);
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
        assert!(count_match(&p, &Place::path("/w/repo/sub"), &[], &[]));
        assert!(!count_match(&p, &Place::path("/elsewhere"), &[], &[]));
    }

    #[test]
    fn remote_and_subpath_match_worktrees_and_fall_back_to_the_glob() {
        // An entry derived in /w/main, a session in a worktree at /w/wt of the same remote.
        let mut p = pred("/w/main", &[], &[]);
        p.repo_remote = Some("github.com/org/repo".into());
        p.subpath = Some("**".into());
        let wt = Place { path: "/w/wt".into(), remote: Some("github.com/org/repo".into()), subpath: Some("crates/x".into()) };
        let at = |sub: &str| Place { subpath: Some(sub.into()), ..wt.clone() };
        assert!(matches_place(&p, &wt), "same remote, other toplevel");
        assert!(!matches_repo(&p, "/w/wt"), "the glob alone would miss it");
        let fork = Place { remote: Some("github.com/fork/repo".into()), ..wt.clone() };
        assert!(!matches_place(&p, &fork), "a fork is another remote");
        // Fallbacks: either side without a remote uses the glob.
        assert!(matches_place(&p, &Place::path("/w/main/src")));
        assert!(!matches_place(&p, &Place::path("/w/wt")));
        let mut glob_only = p.clone();
        glob_only.repo_remote = None;
        assert!(matches_place(&glob_only, &Place { path: "/w/main".into(), ..wt.clone() }));
        assert!(!matches_place(&glob_only, &wt));
        // Monorepo subpath: the glob over the toplevel-relative cwd, ancestors included.
        p.subpath = Some("crates/payments".into());
        assert!(matches_place(&p, &at("crates/payments/src")));
        assert!(!matches_place(&p, &at("crates/ledger")));
        assert!(!matches_place(&p, &at("")));
        p.subpath = Some("crates/*".into());
        assert!(matches_place(&p, &at("crates/ledger/src")));
        assert!(matches_subpath(None, "a/b") && matches_subpath(Some(""), "a") && matches_subpath(Some("**"), ""));
        // Only the place changes the count-time verdict; content rules are unchanged.
        let mut c = pred("/w/main", &["rs"], &[]);
        c.repo_remote = Some("github.com/org/repo".into());
        assert!(count_match(&c, &wt, &s(&["rs"]), &[]));
        assert!(!count_match(&c, &wt, &s(&["md"]), &[]));
    }

    #[test]
    fn derive_fills_the_remote_of_a_local_repo() {
        let t = tempfile::tempdir().unwrap();
        let ok = std::process::Command::new("git").args(["init", "-q"]).current_dir(t.path()).status().unwrap();
        assert!(ok.success());
        std::fs::write(t.path().join(".git/config"), "[remote \"origin\"]\n\turl = git@github.com:Org/Repo.git\n").unwrap();
        let p = derive(Some(&t.path().to_string_lossy()), &[], &[]);
        assert_eq!((p.repo_remote.as_deref(), p.subpath.as_deref()), (Some("github.com/org/repo"), Some("**")));
        assert!(matches_repo(&p, &t.path().to_string_lossy()), "the glob is kept");
        let none = derive(Some("/w/not-a-repo"), &[], &[]);
        assert_eq!((none.repo_remote, none.subpath), (None, None));
    }
}
