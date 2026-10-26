//! `retrieve`: harness-driven unit retrieval for the `context` hook (D22, DESIGN §10).
//!
//! Candidates are the entries the session loads for `--cwd` (`library::list_for_match`:
//! predicate hits plus every kept/pinned entry), restricted to `--entries` when given (the
//! extension passes none: the cwd's match is the bound), that are packages (`units/` or `facts/`).
//! Each unit (`units/**/*.md`, `facts/**/*.md`) is scored by the number of distinct query
//! stems found among its heading + body stems (frontmatter excluded). Units with overlap
//! ≥ `--min-overlap` are sorted by overlap desc, then shorter first, then path, and taken
//! greedily while their text fits `--budget` bytes. Reads files only (one ledger read per
//! candidate entry, none per unit).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Args;
use rematch_core::features::{is_content_token, porter_stem, tokenize};
use rematch_core::library;
use rematch_core::package::SkillDoc;
use rematch_core::types::{Status, DEFAULT_RETRIEVE_BUDGET_BYTES, DEFAULT_RETRIEVE_MIN_OVERLAP};
use serde::Serialize;
use serde_json::Value;

use crate::app::AppCtx;

#[derive(Args, Debug, Clone)]
pub struct RetrieveArgs {
    /// Request text: the last user message and recent tool-result paths.
    #[arg(long)]
    pub query: String,
    /// Byte budget for the returned unit texts.
    #[arg(long, default_value_t = DEFAULT_RETRIEVE_BUDGET_BYTES)]
    pub budget: u32,
    /// Distinct query stems a unit must share to be returned.
    #[arg(long, default_value_t = DEFAULT_RETRIEVE_MIN_OVERLAP)]
    pub min_overlap: u32,
    /// Comma-separated entry ids the caller loaded; units of other entries are never returned.
    #[arg(long, value_delimiter = ',')]
    pub entries: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Unit {
    pub entry_id: String,
    pub name: String,
    /// Relative to the entry dir (`units/x.md`, `facts/y.md`).
    pub path: String,
    pub title: String,
    pub text: String,
    pub overlap: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Retrieval {
    pub units: Vec<Unit>,
    pub used_bytes: usize,
    pub query_stems: Vec<String>,
}

/// Distinct stems of `text` (`features::stems_of_text` semantics, each distinct token
/// stemmed once).
fn stem_set(text: &str) -> HashSet<String> {
    let tokens: HashSet<String> = tokenize(text).into_iter().filter(|t| is_content_token(t)).collect();
    tokens.iter().map(|t| porter_stem(t)).collect()
}

/// Counts the distinct query stems in a text with `stems_of_text` semantics, stemming each
/// distinct token once across all units (token → index of its stem among the query stems).
struct Overlap<'q> {
    query: &'q [String],
    memo: HashMap<String, Option<usize>>,
    token: String,
    seen: Vec<bool>,
}

impl<'q> Overlap<'q> {
    fn new(query: &'q [String]) -> Self {
        Overlap { query, memo: HashMap::new(), token: String::new(), seen: vec![false; query.len()] }
    }

    fn end_token(&mut self) {
        if self.token.is_empty() {
            return;
        }
        let hit = match self.memo.get(self.token.as_str()) {
            Some(h) => *h,
            None => {
                let h = if is_content_token(&self.token) {
                    let stem = porter_stem(&self.token);
                    self.query.iter().position(|q| *q == stem)
                } else {
                    None
                };
                self.memo.insert(self.token.clone(), h);
                h
            }
        };
        if let Some(i) = hit {
            self.seen[i] = true;
        }
        self.token.clear();
    }

    /// Tokens as in `features::tokenize`: lowercased text, ASCII alphanumeric runs.
    fn count(&mut self, text: &str) -> u32 {
        self.seen.iter_mut().for_each(|s| *s = false);
        for c in text.chars() {
            if c.is_ascii() {
                if c.is_ascii_alphanumeric() {
                    self.token.push(c.to_ascii_lowercase());
                } else {
                    self.end_token();
                }
            } else {
                for l in c.to_lowercase() {
                    if l.is_ascii_lowercase() || l.is_ascii_digit() {
                        self.token.push(l);
                    } else {
                        self.end_token();
                    }
                }
            }
        }
        self.end_token();
        self.seen.iter().filter(|s| **s).count() as u32
    }
}

fn md_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for ent in rd.flatten() {
        let p = ent.path();
        if p.is_dir() {
            md_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "md") {
            out.push(p);
        }
    }
}

/// Body without frontmatter, and its first heading (else the file stem).
fn title_and_body(text: &str, file: &Path) -> (String, String) {
    let body = SkillDoc::parse(text).map(|d| d.body).unwrap_or_else(|_| text.to_string());
    let title = body
        .lines()
        .find_map(|l| l.trim_start().strip_prefix('#').map(|h| h.trim_start_matches('#').trim().to_string()))
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| file.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default());
    (title, body)
}

/// Rank and cut the units of `candidates` (`(entry_id, name, entry dir)`).
pub fn rank(candidates: &[(String, String, PathBuf)], query: &str, budget: usize, min_overlap: u32) -> Retrieval {
    let mut query_stems: Vec<String> = stem_set(query).into_iter().collect();
    query_stems.sort();
    let mut scored: Vec<Unit> = Vec::new();
    if !query_stems.is_empty() {
        let mut overlap = Overlap::new(&query_stems);
        for (id, name, dir) in candidates {
            let mut files = Vec::new();
            for sub in ["units", "facts"] {
                md_files(&dir.join(sub), &mut files);
            }
            for f in files {
                let Ok(text) = std::fs::read_to_string(&f) else { continue };
                let (title, body) = title_and_body(&text, &f);
                let n = overlap.count(&body);
                if n < min_overlap.max(1) {
                    continue;
                }
                let path = f.strip_prefix(dir).unwrap_or(&f).to_string_lossy().replace('\\', "/");
                scored.push(Unit { entry_id: id.clone(), name: name.clone(), path, title, text, overlap: n });
            }
        }
    }
    scored.sort_by(|a, b| {
        b.overlap
            .cmp(&a.overlap)
            .then(a.text.len().cmp(&b.text.len()))
            .then_with(|| (&a.name, &a.path).cmp(&(&b.name, &b.path)))
    });
    let mut used = 0usize;
    let mut units = Vec::new();
    for u in scored {
        if used + u.text.len() <= budget {
            used += u.text.len();
            units.push(u);
        }
    }
    Retrieval { units, used_bytes: used, query_stems }
}

/// Loaded package entries for `cwd`: `(id, name, dir)`.
pub fn candidates(ctx: &AppCtx, cwd: &Path, only: &[String]) -> Result<Vec<(String, String, PathBuf)>> {
    let repo = rematch_core::import::repo_of(cwd);
    let m = library::list_for_match(&ctx.ledger, &ctx.paths, repo.as_deref(), cwd)?;
    let mut out = Vec::new();
    for me in m.entries {
        let loaded = me.predicate_hit || matches!(me.status, Status::Kept | Status::Pinned);
        if !loaded || (!only.is_empty() && !only.contains(&me.id)) {
            continue;
        }
        let Some(e) = ctx.ledger.get_entry(&me.id)? else { continue };
        if e.path.join("units").is_dir() || e.path.join("facts").is_dir() {
            out.push((e.id, e.name, e.path));
        }
    }
    Ok(out)
}

pub fn cmd_retrieve(ctx: &AppCtx, a: RetrieveArgs) -> Result<Value> {
    let only: Vec<String> = a.entries.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    let c = candidates(ctx, &ctx.cwd, &only)?;
    Ok(serde_json::to_value(rank(&c, &a.query, a.budget as usize, a.min_overlap))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(p: &Path, s: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }

    #[test]
    fn overlap_counts_like_stems_of_text() {
        let query = "Fix the LEDGER reconciliation; migrations failing in 01a175cc parser.rs (Ünïcode café)";
        let mut q: Vec<String> = stem_set(query).into_iter().collect();
        q.sort();
        let mut o = Overlap::new(&q);
        for text in [
            "The ledger's reconciliations: MIGRATION-failed, parsers.",
            "café ÜNÏCODE ledger ledger ledger",
            "nothing relevant here at all",
            "",
            "Parser_failing01a175cc fix-fixes",
        ] {
            let want = stem_set(text).iter().filter(|s| q.contains(s)).count() as u32;
            assert_eq!(o.count(text), want, "{text}");
        }
    }

    #[test]
    fn ranks_by_overlap_then_length_and_cuts_at_budget() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("pkg");
        write(&d.join("SKILL.md"), "---\nname: pkg\n---\nrouter mentions ledger reconciliation migration\n");
        write(&d.join("units/ledger.md"), "# Ledger reconciliation\n\nRun the reconciliation migration before the ledger tests.\n");
        write(&d.join("units/short.md"), "# Ledger\n\nReconciliation first.\n");
        write(&d.join("units/other.md"), "# Plotting\n\nCharts use the dark theme.\n");
        write(&d.join("facts/rates.md"), "---\nas_of: 2026-10-01\n---\n# Ledger rates\n\nThe ledger reconciliation runs nightly at 02:00.\n");
        let c = vec![("e1".to_string(), "pkg".to_string(), d.clone())];

        let r = rank(&c, "please fix the ledger reconciliation migration", 10_000, 2);
        let got: Vec<(&str, u32)> = r.units.iter().map(|u| (u.path.as_str(), u.overlap)).collect();
        assert_eq!(got, vec![("units/ledger.md", 3), ("units/short.md", 2), ("facts/rates.md", 2)], "{r:?}");
        assert_eq!(r.units[0].title, "Ledger reconciliation");
        assert!(r.units[2].text.contains("as_of"), "unit text keeps its frontmatter");
        assert_eq!(r.used_bytes, r.units.iter().map(|u| u.text.len()).sum::<usize>());
        assert!(r.query_stems.contains(&"ledger".to_string()));

        // the budget skips a unit that does not fit and keeps a smaller one after it
        let first = r.units[0].text.len();
        let r2 = rank(&c, "please fix the ledger reconciliation migration", first + r.units[1].text.len(), 2);
        assert_eq!(r2.units.iter().map(|u| u.path.as_str()).collect::<Vec<_>>(), vec!["units/ledger.md", "units/short.md"]);
        assert!(r2.used_bytes <= first + r.units[1].text.len());

        // min overlap and empty queries
        assert!(rank(&c, "ledger", 10_000, 2).units.is_empty());
        assert_eq!(rank(&c, "ledger", 10_000, 1).units.len(), 3);
        assert!(rank(&c, "the and of", 10_000, 1).units.is_empty());
    }
}
