//! Work domain of a session and of a trigger predicate (DESIGN §6 "Domain"): a dotted
//! lowercase path (`web`, `web.frontend`, `quant.options`) decided without a model, in
//! precedence order:
//!
//! 1. `REMATCH_ROOT/policy.json` `domains: [{match: {repo_remote?, subpath?}, domain}]`,
//!    first match wins (what the user declared, `rematch policy domain set`);
//! 2. else the session's branch `branches/<branch>/domain.yml` `domain:`;
//! 3. else inference from the session's extensions and stems with the table in
//!    `improver/domain_rules.toml` ([`DomainRules`]).
//!
//! A predicate's domain gates matching ([`domain_matches`]): when the predicate and the
//! session both carry one, the session's must equal it or extend it; absent on either side
//! the domain is not tested.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};
use glob::{MatchOptions, Pattern};
use serde::{Deserialize, Serialize};

use crate::features::porter_stem;
use crate::paths::RematchPaths;
use crate::predicate::{matches_subpath, normalize_ext};
use crate::types::SessionRecord;

/// `REMATCH_ROOT/policy.json` (also read by `scope::export` and `import --history`).
pub const POLICY_FILE: &str = "policy.json";
/// The inference table under `REMATCH_ROOT/improver/`.
pub const DOMAIN_RULES_FILE: &str = "domain_rules.toml";
/// The shipped inference table (used when the improver copy is missing).
pub const DEFAULT_DOMAIN_RULES_TOML: &str = include_str!("../../../improver/domain_rules.toml");
/// Kept entries per top-level domain across every branch (DESIGN §8) unless
/// `policy.json` `caps.kept_per_domain` says otherwise.
pub const DEFAULT_KEPT_PER_DOMAIN: usize = 8;

/// Where a `policy.json` domain declaration applies. An absent field matches anything.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainMatch {
    /// Normalized remote (`identity::normalize_remote`) or a glob over it (`github.com/acme/*`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_remote: Option<String>,
    /// Glob over the toplevel-relative cwd (also matches descendants).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subpath: Option<String>,
}

/// One `policy.json` `domains` item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainDecl {
    #[serde(rename = "match", default)]
    pub when: DomainMatch,
    pub domain: String,
}

/// `policy.json` `caps`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyCaps {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kept_per_domain: Option<usize>,
}

/// The parts of `policy.json` this module reads (other keys belong to other readers).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
struct PolicyPart {
    #[serde(default)]
    domains: Vec<DomainDecl>,
    #[serde(default)]
    caps: PolicyCaps,
}

/// `[[ext]]` row: any of `exts` scores for `domain`'s top level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtRule {
    pub domain: String,
    pub exts: Vec<String>,
}

/// `[[stem]]` row: any of `stems` scores for `domain`'s top level and, on the winner,
/// gives its second level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StemRule {
    pub domain: String,
    pub stems: Vec<String>,
}

/// `improver/domain_rules.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainRules {
    pub ext_weight: u32,
    pub stem_weight: u32,
    pub min_score: u32,
    pub min_margin: u32,
    #[serde(default)]
    pub ext: Vec<ExtRule>,
    #[serde(default)]
    pub stem: Vec<StemRule>,
}

impl Default for DomainRules {
    fn default() -> Self {
        DomainRules::parse(DEFAULT_DOMAIN_RULES_TOML).expect("shipped domain_rules.toml parses")
    }
}

impl DomainRules {
    pub fn parse(text: &str) -> Result<DomainRules> {
        let mut r: DomainRules = toml::from_str(text).context("domain_rules.toml")?;
        for x in &mut r.ext {
            x.domain = normalize_domain(&x.domain).with_context(|| format!("domain_rules.toml: bad domain {:?}", x.domain))?;
            x.exts = x.exts.iter().map(|e| normalize_ext(e)).filter(|e| !e.is_empty()).collect();
        }
        for x in &mut r.stem {
            x.domain = normalize_domain(&x.domain).with_context(|| format!("domain_rules.toml: bad domain {:?}", x.domain))?;
            x.stems = x.stems.iter().map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()).collect();
        }
        Ok(r)
    }

    /// `improver/domain_rules.toml`, or the shipped table when the file does not exist.
    pub fn load(path: &Path) -> Result<DomainRules> {
        match std::fs::read_to_string(path) {
            Ok(t) => Self::parse(&t).with_context(|| path.display().to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DomainRules::default()),
            Err(e) => Err(e).with_context(|| path.display().to_string()),
        }
    }
}

/// Everything [`domain_of`] decides with: declarations, branch files, inference table, cap.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DomainPolicy {
    /// `policy.json` `domains`, in file order.
    pub declared: Vec<DomainDecl>,
    /// `branches/<branch>/domain.yml` `domain:` by branch.
    pub branch_domains: BTreeMap<String, String>,
    pub rules: DomainRules,
    pub caps: PolicyCaps,
}

impl DomainPolicy {
    /// Reads `policy.json`, every `branches/*/domain.yml` and `improver/domain_rules.toml`
    /// (file reads only). Missing files are empty (the shipped table for the rules).
    pub fn load(paths: &RematchPaths) -> Result<DomainPolicy> {
        let p = paths.root.join(POLICY_FILE);
        let part: PolicyPart = match std::fs::read_to_string(&p) {
            Ok(t) => serde_json::from_str(&t).with_context(|| format!("parse {}", p.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => PolicyPart::default(),
            Err(e) => return Err(e).with_context(|| p.display().to_string()),
        };
        let mut declared = Vec::with_capacity(part.domains.len());
        for d in part.domains {
            let domain = normalize_domain(&d.domain).with_context(|| format!("{}: bad domain {:?}", p.display(), d.domain))?;
            declared.push(DomainDecl { when: d.when, domain });
        }
        Ok(DomainPolicy {
            declared,
            branch_domains: branch_domains(&paths.branches)?,
            rules: DomainRules::load(&paths.improver.join(DOMAIN_RULES_FILE))?,
            caps: part.caps,
        })
    }

    pub fn kept_per_domain(&self) -> usize {
        self.caps.kept_per_domain.unwrap_or(DEFAULT_KEPT_PER_DOMAIN)
    }

    /// The first declaration matching the place, if any.
    pub fn declared_for(&self, repo_remote: Option<&str>, subpath: Option<&str>) -> Option<&DomainDecl> {
        self.declared.iter().find(|d| decl_matches(&d.when, repo_remote, subpath))
    }
}

/// `domain:` of every `branches/<branch>/domain.yml` that names one.
fn branch_domains(dir: &Path) -> Result<BTreeMap<String, String>> {
    #[derive(Deserialize)]
    struct DomainYml {
        #[serde(default)]
        domain: Option<String>,
    }
    let mut out = BTreeMap::new();
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e).with_context(|| dir.display().to_string()),
    };
    for e in rd {
        let e = e?;
        let file = e.path().join("domain.yml");
        let Ok(text) = std::fs::read_to_string(&file) else { continue };
        let y: DomainYml = serde_yaml::from_str(&text).with_context(|| format!("parse {}", file.display()))?;
        if let Some(d) = y.domain.as_deref().and_then(normalize_domain) {
            out.insert(e.file_name().to_string_lossy().into_owned(), d);
        }
    }
    Ok(out)
}

/// A remote pattern against a remote: equal, or the pattern is a glob that matches it.
fn remote_matches(pattern: &str, remote: &str) -> bool {
    let pattern = pattern.trim();
    pattern == remote
        || Pattern::new(pattern).is_ok_and(|p| {
            p.matches_with(remote, MatchOptions { case_sensitive: true, require_literal_separator: false, require_literal_leading_dot: false })
        })
}

fn decl_matches(m: &DomainMatch, repo_remote: Option<&str>, subpath: Option<&str>) -> bool {
    let remote_ok = match m.repo_remote.as_deref() {
        None => true,
        Some(want) => repo_remote.is_some_and(|have| remote_matches(want, have)),
    };
    remote_ok
        && match m.subpath.as_deref() {
            None => true,
            Some(g) => matches_subpath(Some(g), subpath.unwrap_or("")),
        }
}

/// Dotted lowercase form of `s` (`Web.FrontEnd` → `web.frontend`); `None` when empty or
/// a segment is empty or holds anything but `[a-z0-9_-]`.
pub fn normalize_domain(s: &str) -> Option<String> {
    let d = s.trim().to_lowercase();
    let ok = !d.is_empty()
        && d.split('.').all(|seg| !seg.is_empty() && seg.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-'));
    ok.then_some(d)
}

/// First segment: `quant.options` → `quant`.
pub fn top_domain(d: &str) -> &str {
    d.split('.').next().unwrap_or(d)
}

/// The predicate gate: when both sides carry a domain, the session's equals the
/// predicate's or extends it at a dot (`quant` matches `quant.options`, not `quantx`);
/// absent on either side ⇒ true.
pub fn domain_matches(predicate: Option<&str>, session: Option<&str>) -> bool {
    match (predicate, session) {
        (Some(p), Some(s)) => s == p || s.strip_prefix(p).is_some_and(|rest| rest.starts_with('.')),
        _ => true,
    }
}

/// Which rule decided a domain ([`decide`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DomainSource {
    /// `policy.json` `domains`.
    Policy,
    /// `branches/<branch>/domain.yml`.
    DomainYml,
    /// `improver/domain_rules.toml` over exts and stems.
    Inferred,
}

impl DomainSource {
    pub fn name(self) -> &'static str {
        match self {
            DomainSource::Policy => "policy.json",
            DomainSource::DomainYml => "domain.yml",
            DomainSource::Inferred => "inferred",
        }
    }
}

/// Domain of a session or place and the rule that decided it (module doc for the
/// precedence). `branch` is the library branch (`library::branch_for`); `exts` and `stems`
/// the session's (empty at load time, which leaves only the declared and branch domains).
pub fn decide(
    repo_remote: Option<&str>,
    subpath: Option<&str>,
    branch: Option<&str>,
    exts: &[String],
    stems: &[String],
    policy: &DomainPolicy,
) -> Option<(String, DomainSource)> {
    if let Some(d) = policy.declared_for(repo_remote, subpath) {
        return Some((d.domain.clone(), DomainSource::Policy));
    }
    if let Some(d) = branch.and_then(|b| policy.branch_domains.get(b)) {
        return Some((d.clone(), DomainSource::DomainYml));
    }
    infer(exts, stems, &policy.rules).map(|d| (d, DomainSource::Inferred))
}

/// [`decide`] without the source.
pub fn domain_of(
    repo_remote: Option<&str>,
    subpath: Option<&str>,
    branch: Option<&str>,
    exts: &[String],
    stems: &[String],
    policy: &DomainPolicy,
) -> Option<String> {
    decide(repo_remote, subpath, branch, exts, stems, policy).map(|(d, _)| d)
}

/// [`decide`] for a recorded session (its branch from its repo, else cwd).
pub fn session_decision(s: &SessionRecord, policy: &DomainPolicy) -> Option<(String, DomainSource)> {
    let branch = crate::library::branch_for(s.repo.as_deref().map(Path::new), Path::new(&s.cwd));
    decide(s.repo_remote.as_deref(), s.subpath.as_deref(), Some(&branch), &s.exts, &s.stems, policy)
}

/// [`domain_of`] for a recorded session.
pub fn session_domain(s: &SessionRecord, policy: &DomainPolicy) -> Option<String> {
    session_decision(s, policy).map(|(d, _)| d)
}

/// Load-time domain of a place (`match`, `skills sync`): the declared one only
/// (`policy.json`, else the branch's `domain.yml`); the session's extensions and stems are
/// not known yet. An unreadable policy leaves the place without one (hot path; `doctor`
/// reports the file).
pub fn place_domain(paths: &RematchPaths, place: &crate::predicate::Place, branch: &str) -> Option<String> {
    let policy = DomainPolicy::load(paths).ok()?;
    domain_of(place.remote.as_deref(), place.subpath.as_deref(), Some(branch), &[], &[], &policy)
}

/// Inference from the table (module doc of `domain_rules.toml` for the scoring).
pub fn infer(exts: &[String], stems: &[String], rules: &DomainRules) -> Option<String> {
    let exts: BTreeSet<String> = exts.iter().map(|e| normalize_ext(e)).collect();
    let stems: BTreeSet<String> = stems.iter().map(|s| s.trim().to_lowercase()).collect();
    let stem_hits = |rule: &StemRule| {
        rule.stems.iter().filter(|w| stems.contains(w.as_str()) || stems.contains(&porter_stem(w))).count() as u32
    };
    let mut score: BTreeMap<&str, u32> = BTreeMap::new();
    for r in &rules.ext {
        let hits = r.exts.iter().filter(|e| exts.contains(e.as_str())).count() as u32;
        if hits > 0 {
            *score.entry(top_domain(&r.domain)).or_default() += hits * rules.ext_weight;
        }
    }
    for r in &rules.stem {
        let hits = stem_hits(r);
        if hits > 0 {
            *score.entry(top_domain(&r.domain)).or_default() += hits * rules.stem_weight;
        }
    }
    let mut ranked: Vec<(&str, u32)> = score.into_iter().collect();
    ranked.sort_by_key(|r| std::cmp::Reverse(r.1));
    let (top, best) = *ranked.first()?;
    let runner_up = ranked.get(1).map_or(0, |r| r.1);
    if best < rules.min_score || best - runner_up < rules.min_margin {
        return None;
    }
    // Second level: the stem rule under the winner with the most hits (table order on ties).
    let mut sub: Option<(&str, u32)> = None;
    for r in rules.stem.iter().filter(|r| top_domain(&r.domain) == top && r.domain.contains('.')) {
        let hits = stem_hits(r);
        if hits > 0 && sub.is_none_or(|(_, h)| hits > h) {
            sub = Some((&r.domain, hits));
        }
    }
    Some(sub.map_or(top, |(d, _)| d).to_string())
}

/// The domain every given domain lies in (absent ones ignored): the shared one when all are
/// equal, else their shared top level; `None` when they span top levels or none is known.
pub fn common_domain<'a>(domains: impl IntoIterator<Item = Option<&'a str>>) -> Option<String> {
    let known: BTreeSet<&str> = domains.into_iter().flatten().collect();
    let first = *known.iter().next()?;
    if known.len() == 1 {
        return Some(first.to_string());
    }
    let top = top_domain(first);
    known.iter().all(|d| top_domain(d) == top).then(|| top.to_string())
}

/// The domain most reference sessions share: a strict majority of the sessions that have
/// one on the full domain, else on the top level; `None` when neither has one.
pub fn majority_domain<'a>(domains: impl IntoIterator<Item = Option<&'a str>>) -> Option<String> {
    let known: Vec<&str> = domains.into_iter().flatten().collect();
    let strict = |keys: Vec<&str>| -> Option<String> {
        let mut n: BTreeMap<&str, usize> = BTreeMap::new();
        for k in &keys {
            *n.entry(k).or_default() += 1;
        }
        n.into_iter().find(|(_, c)| *c * 2 > keys.len()).map(|(k, _)| k.to_string())
    };
    strict(known.clone()).or_else(|| strict(known.iter().map(|d| top_domain(d)).collect()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn policy() -> DomainPolicy {
        DomainPolicy::default()
    }

    #[test]
    fn shipped_table_parses_and_is_the_default() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../improver").join(DOMAIN_RULES_FILE);
        assert_eq!(DomainRules::load(&repo).unwrap(), DomainRules::default());
        let r = DomainRules::default();
        assert_eq!((r.ext_weight, r.stem_weight, r.min_score, r.min_margin), (2, 1, 2, 1));
        assert_eq!(r.ext.len(), 5);
        assert_eq!(r.stem.len(), 4);
    }

    #[test]
    fn precedence_policy_then_domain_yml_then_inference() {
        let mut p = policy();
        let web = s(&["tsx", "css"]);
        assert_eq!(domain_of(Some("github.com/acme/app"), Some(""), Some("app"), &web, &[], &p).as_deref(), Some("web"));
        p.branch_domains.insert("app".into(), "quant.options".into());
        assert_eq!(domain_of(Some("github.com/acme/app"), Some(""), Some("app"), &web, &[], &p).as_deref(), Some("quant.options"));
        p.declared.push(DomainDecl {
            when: DomainMatch { repo_remote: Some("github.com/acme/*".into()), subpath: None },
            domain: "mobile".into(),
        });
        assert_eq!(domain_of(Some("github.com/acme/app"), Some(""), Some("app"), &web, &[], &p).as_deref(), Some("mobile"));
        // a declaration elsewhere does not apply; the branch file still does
        assert_eq!(domain_of(Some("github.com/other/app"), Some(""), Some("app"), &web, &[], &p).as_deref(), Some("quant.options"));
        // first match wins; subpath narrows
        p.declared.insert(0, DomainDecl {
            when: DomainMatch { repo_remote: Some("github.com/acme/app".into()), subpath: Some("ios".into()) },
            domain: "mobile.ios".into(),
        });
        assert_eq!(domain_of(Some("github.com/acme/app"), Some("ios/Sources"), None, &[], &[], &p).as_deref(), Some("mobile.ios"));
        assert_eq!(domain_of(Some("github.com/acme/app"), Some("web"), None, &[], &[], &p).as_deref(), Some("mobile"));
        // no remote: declarations naming one never match
        assert_eq!(domain_of(None, None, None, &web, &[], &p).as_deref(), Some("web"));
    }

    #[test]
    fn inference_scores_and_margins() {
        let r = DomainRules::default();
        assert_eq!(infer(&s(&["tsx"]), &[], &r).as_deref(), Some("web"), "one ext = 2");
        assert_eq!(infer(&[], &s(&["backtest"]), &r), None, "one stem = 1 < 2");
        assert_eq!(infer(&[], &s(&["backtest", "sharp"]), &r).as_deref(), Some("quant"), "porter form of sharpe");
        assert_eq!(infer(&s(&["tsx", "swift"]), &[], &r), None, "tie: no margin");
        assert_eq!(infer(&s(&["tsx", "swift"]), &s(&["ios"]), &r).as_deref(), Some("mobile"), "margin 1");
        assert_eq!(infer(&s(&["tsx", "css"]), &s(&["compon", "layout"]), &r).as_deref(), Some("web.frontend"));
        assert_eq!(infer(&s(&["blend"]), &s(&["anim", "rig"]), &r).as_deref(), Some("3d.animation"));
        assert_eq!(infer(&s(&["rs", "md"]), &[], &r).as_deref(), Some("backend"));
        assert_eq!(infer(&s(&["md", "toml"]), &s(&["ledger"]), &r), None, "nothing scores");
        // a stem rule's second level never applies to another winner
        assert_eq!(infer(&s(&["swift", "kt"]), &s(&["compon"]), &r).as_deref(), Some("mobile"));
    }

    #[test]
    fn gate_match_mismatch_prefix_absent() {
        assert!(domain_matches(Some("web"), Some("web")));
        assert!(!domain_matches(Some("web"), Some("mobile")));
        assert!(domain_matches(Some("quant"), Some("quant.options")));
        assert!(!domain_matches(Some("quant.options"), Some("quant")), "a narrower trigger does not widen");
        assert!(!domain_matches(Some("quant"), Some("quantx")));
        assert!(domain_matches(None, Some("web")));
        assert!(domain_matches(Some("web"), None));
        assert!(domain_matches(None, None));
    }

    #[test]
    fn majority_and_normalization() {
        assert_eq!(majority_domain([Some("web"), Some("web"), Some("mobile"), None]).as_deref(), Some("web"));
        assert_eq!(majority_domain([Some("web"), Some("mobile")]), None);
        assert_eq!(majority_domain([Some("web.frontend"), Some("web"), Some("web.a11y")]).as_deref(), Some("web"));
        assert_eq!(majority_domain([None, None]), None);
        assert_eq!(common_domain([Some("web"), None, Some("web")]).as_deref(), Some("web"));
        assert_eq!(common_domain([Some("web.frontend"), Some("web.a11y")]).as_deref(), Some("web"));
        assert_eq!(common_domain([Some("web"), Some("web"), Some("mobile")]), None, "never narrows a cross-domain lesson");
        assert_eq!(common_domain([None, None]), None);
        assert_eq!(normalize_domain(" Web.FrontEnd ").as_deref(), Some("web.frontend"));
        assert_eq!(normalize_domain("web..x"), None);
        assert_eq!(normalize_domain("a/b"), None);
        assert_eq!(top_domain("quant.options"), "quant");
    }

    #[test]
    fn load_reads_policy_branch_files_and_rules() {
        let t = tempfile::tempdir().unwrap();
        let paths = RematchPaths::with_roots(t.path().join("r"), t.path().join("a"));
        paths.ensure_dirs().unwrap();
        assert_eq!(DomainPolicy::load(&paths).unwrap().kept_per_domain(), DEFAULT_KEPT_PER_DOMAIN);
        std::fs::write(
            paths.root.join(POLICY_FILE),
            r#"{"environment":"work","domains":[{"match":{"repo_remote":"github.com/acme/web"},"domain":"Web"}],"caps":{"kept_per_domain":3}}"#,
        )
        .unwrap();
        std::fs::create_dir_all(paths.branches.join("opts")).unwrap();
        std::fs::write(paths.branches.join("opts/domain.yml"), "domain: quant.options\nfacets: [a, b]\n").unwrap();
        std::fs::create_dir_all(paths.branches.join("bare")).unwrap();
        std::fs::write(paths.branches.join("bare/domain.yml"), "facets: [a]\n").unwrap();
        let p = DomainPolicy::load(&paths).unwrap();
        assert_eq!(p.declared[0].domain, "web");
        assert_eq!(p.kept_per_domain(), 3);
        assert_eq!(p.branch_domains.into_iter().collect::<Vec<_>>(), vec![("opts".to_string(), "quant.options".to_string())]);
    }
}
