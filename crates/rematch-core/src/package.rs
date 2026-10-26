//! Skill package model (DESIGN §4 layout): SKILL.md frontmatter parse/serialize with
//! unknown keys preserved, the `rematch:` block, and the mechanical split of skills over
//! 8 KB into a router + `units/` + `facts/` (SPEC "Split first, if large").

use std::path::Path;
use std::sync::LazyLock;

use anyhow::{anyhow, Result};
use chrono::NaiveDate;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_yaml::{Mapping, Value};

use crate::types::{Edit, Family, Status};

/// Skills larger than this get the mechanical split.
pub const SPLIT_THRESHOLD: usize = 8 * 1024;
/// Byte budget for the laws copied into the router.
pub const ROUTER_LAWS_BUDGET: usize = 4 * 1024;
/// A single law longer than this stays in its unit only.
pub const MAX_LAW_BYTES: usize = 500;
/// Tables with more data rows than this move to the ledger notes.
pub const LONG_TABLE_ROWS: usize = 12;
pub const LEDGER_NOTES: &str = "units/ledger_notes.md";

/// `rematch:` frontmatter block (DESIGN §4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RematchBlock {
    pub id: String,
    pub status: Status,
    pub branch: String,
    pub family: Family,
    pub version: u32,
    pub predicate_hash: String,
}

/// A markdown document with optional YAML frontmatter (`SKILL.md`, units, facts).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SkillDoc {
    pub frontmatter: Mapping,
    pub body: String,
}

impl SkillDoc {
    pub fn parse(text: &str) -> Result<SkillDoc> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let first_nl = text.find('\n');
        let first = first_nl.map_or(text, |i| &text[..i]).trim_end_matches('\r');
        if first != "---" {
            return Ok(SkillDoc { frontmatter: Mapping::new(), body: text.to_string() });
        }
        let rest = &text[first_nl.unwrap() + 1..];
        let mut offset = 0;
        for line in rest.split_inclusive('\n') {
            let l = line.trim_end_matches(['\n', '\r']);
            if l == "---" || l == "..." {
                let yaml = &rest[..offset];
                let fm = parse_frontmatter(yaml)?;
                return Ok(SkillDoc { frontmatter: fm, body: rest[offset + line.len()..].to_string() });
            }
            offset += line.len();
        }
        Err(anyhow!("frontmatter: missing closing ---"))
    }

    pub fn render(&self) -> String {
        if self.frontmatter.is_empty() {
            return self.body.clone();
        }
        let yaml = serde_yaml::to_string(&self.frontmatter).unwrap_or_default();
        format!("---\n{yaml}---\n{}", self.body)
    }

    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.frontmatter.get(key).and_then(Value::as_str)
    }

    pub fn set(&mut self, key: &str, value: impl Into<Value>) {
        self.frontmatter.insert(Value::String(key.to_string()), value.into());
    }

    pub fn rematch(&self) -> Option<RematchBlock> {
        self.frontmatter
            .get("rematch")
            .and_then(|v| serde_yaml::from_value(v.clone()).ok())
    }

    /// Merge the rematch block: our keys overwrite, unknown keys inside an existing
    /// `rematch:` map survive.
    pub fn set_rematch(&mut self, b: &RematchBlock) {
        let mut merged = match self.frontmatter.get("rematch") {
            Some(Value::Mapping(m)) => m.clone(),
            _ => Mapping::new(),
        };
        if let Ok(Value::Mapping(ours)) = serde_yaml::to_value(b) {
            for (k, v) in ours {
                merged.insert(k, v);
            }
        }
        self.frontmatter.insert(Value::String("rematch".into()), Value::Mapping(merged));
    }
}

/// Strict YAML first; on failure (real skills carry unquoted `: ` inside plain
/// descriptions, which omp tolerates) fall back to per-top-level-key parsing where a
/// key whose chunk is not valid YAML keeps its raw text as a string.
fn parse_frontmatter(yaml: &str) -> Result<Mapping> {
    if yaml.trim().is_empty() {
        return Ok(Mapping::new());
    }
    if let Ok(m) = serde_yaml::from_str::<Mapping>(yaml) {
        return Ok(m);
    }
    let mut chunks: Vec<(String, Vec<&str>)> = Vec::new();
    for line in yaml.lines() {
        let top = !line.starts_with([' ', '\t', '-', '#']) && !line.trim().is_empty();
        match line.split_once(':') {
            Some((k, _)) if top && !k.is_empty() && !k.contains(' ') => {
                chunks.push((k.to_string(), vec![line]))
            }
            _ => match chunks.last_mut() {
                Some((_, lines)) => lines.push(line),
                None if line.trim().is_empty() || line.trim_start().starts_with('#') => {}
                None => return Err(anyhow!("frontmatter: unparseable line {line:?}")),
            },
        }
    }
    let mut m = Mapping::new();
    for (k, lines) in chunks {
        let chunk = lines.join("\n");
        let v = match serde_yaml::from_str::<Mapping>(&chunk) {
            Ok(mut one) => one.remove(k.as_str()).unwrap_or(Value::Null),
            Err(_) => {
                let first = lines[0].split_once(':').map_or("", |(_, v)| v);
                let raw: Vec<&str> = std::iter::once(first).chain(lines[1..].iter().copied()).map(str::trim).collect();
                let raw = raw.join(" ");
                let raw = raw.trim();
                let unq = raw
                    .strip_prefix('"')
                    .and_then(|r| r.strip_suffix('"'))
                    .or_else(|| raw.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')))
                    .unwrap_or(raw);
                Value::String(unq.to_string())
            }
        };
        m.insert(Value::String(k), v);
    }
    Ok(m)
}

/// A directory is a package when it has SKILL.md plus `units/` or `facts/`.
pub fn is_package(dir: &Path) -> bool {
    dir.join("SKILL.md").is_file() && (dir.join("units").is_dir() || dir.join("facts").is_dir())
}

/// Always-on bytes of a SKILL.md: the body without frontmatter.
pub fn router_bytes(skill_md: &str) -> usize {
    SkillDoc::parse(skill_md).map(|d| d.body.len()).unwrap_or(skill_md.len())
}

/// A trigger line: `Use when …`, `Use this skill whenever …`, `- **Use for …`.
pub static TRIGGER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*(?:[-*]\s+)?(?:\*\*)?use (?:this skill |it )?(?:when|whenever|for|to|if|on)\b").unwrap()
});

/// Imperative bullets carrying MUST/NEVER/ALWAYS (any case for must/never).
pub static LAW_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:[-*+]|\d+[.)])\s+.*\b(?:MUST|NEVER|ALWAYS|[Mm]ust|[Nn]ever)\b").unwrap()
});

/// Measured values: numbers carrying a unit or a percent sign.
pub static MEASURED_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\d+(?:\.\d+)?\s?(?:%|(?:pp|bps|bp|ms|us|µs|ns|s|sec|seconds|min|mins|h|hrs|hours|days?|KB|MB|GB|TB|kB|tokens|sessions|rows|x)\b)",
    )
    .unwrap()
});

static REJECTED_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:reject(?:ed)?|killed|dead[- ]ends?|graveyard|negative results?|did not work|failed (?:hypothes[ie]s|attempts?|ideas?))\b",
    )
    .unwrap()
});

static FACTS_HEADING_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:results?|measured|measurements?|benchmarks?|metrics?|facts?|receipts?|numbers)\b").unwrap()
});

/// Kebab slug from heading text, leading numbering dropped, ≤ 48 chars.
pub fn slug(text: &str) -> String {
    let mut s = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            s.push(c.to_ascii_lowercase());
        } else if !s.ends_with('-') && !s.is_empty() {
            s.push('-');
        }
    }
    let s = s.trim_matches('-');
    let s = s.trim_start_matches(|c: char| c.is_ascii_digit() || c == '-');
    let mut out = String::new();
    for part in s.split('-').filter(|p| !p.is_empty()) {
        if !out.is_empty() && out.len() + 1 + part.len() > 48 {
            break;
        }
        if !out.is_empty() {
            out.push('-');
        }
        out.push_str(&part[..part.len().min(48)]);
    }
    if out.is_empty() {
        "section".into()
    } else {
        out
    }
}

struct Section {
    heading: String,
    lines: Vec<String>,
}

/// Split body lines into a preamble and H2 sections, respecting code fences.
fn sections(body: &str, level: &str) -> (Vec<String>, Vec<Section>) {
    let mut pre = Vec::new();
    let mut secs: Vec<Section> = Vec::new();
    let mut fence = false;
    for line in body.lines() {
        if line.trim_start().starts_with("```") || line.trim_start().starts_with("~~~") {
            fence = !fence;
        }
        if !fence && line.starts_with(level) && !line[level.len()..].starts_with('#') {
            secs.push(Section { heading: line[level.len()..].trim().to_string(), lines: vec![line.to_string()] });
            continue;
        }
        match secs.last_mut() {
            Some(s) => s.lines.push(line.to_string()),
            None => pre.push(line.to_string()),
        }
    }
    (pre, secs)
}

fn is_table_line(l: &str) -> bool {
    l.trim_start().starts_with('|')
}

fn is_table_sep(l: &str) -> bool {
    let t = l.trim();
    t.starts_with('|') && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

fn meaningful(lines: &[String]) -> bool {
    lines.iter().skip(1).any(|l| {
        let t = l.trim();
        !t.is_empty() && t != "---" && t != "***"
    })
}

/// Move long tables and rejected-hypothesis H3 subsections of a section to `notes`.
fn extract_ledger_parts(sec: &Section, notes: &mut Vec<String>) -> Vec<String> {
    let mut kept: Vec<String> = Vec::new();
    let mut i = 0;
    let lines = &sec.lines;
    while i < lines.len() {
        let l = &lines[i];
        if let Some(h3) = l.strip_prefix("### ") {
            if REJECTED_RE.is_match(h3) {
                let end = lines[i + 1..]
                    .iter()
                    .position(|x| x.starts_with("### "))
                    .map_or(lines.len(), |p| i + 1 + p);
                notes.push(format!("## {} › {}\n", sec.heading, h3.trim()));
                notes.extend(lines[i + 1..end].iter().cloned());
                notes.push(String::new());
                kept.push(format!("_Rejected hypotheses `{}` moved to {LEDGER_NOTES} (ledger)._", h3.trim()));
                i = end;
                continue;
            }
        }
        if is_table_line(l) {
            let end = lines[i..].iter().position(|x| !is_table_line(x)).map_or(lines.len(), |p| i + p);
            let rows = lines[i..end].iter().filter(|x| !is_table_sep(x)).count().saturating_sub(1);
            if rows > LONG_TABLE_ROWS {
                notes.push(format!("## {} (table, {rows} rows)\n", sec.heading));
                notes.extend(lines[i..end].iter().cloned());
                notes.push(String::new());
                kept.push(format!("_Table ({rows} rows) moved to {LEDGER_NOTES} (ledger)._"));
            } else {
                kept.extend(lines[i..end].iter().cloned());
            }
            i = end;
            continue;
        }
        kept.push(l.clone());
        i += 1;
    }
    kept
}

fn is_facts_section(heading: &str, lines: &[String]) -> bool {
    let text: String = lines.iter().skip(1).map(|l| l.as_str()).collect::<Vec<_>>().join("\n");
    let n = MEASURED_RE.find_iter(&text).count();
    if n == 0 {
        return false;
    }
    if FACTS_HEADING_RE.is_match(heading) && n >= 3 {
        return true;
    }
    // Dense in measurements and light on laws: a section of measured values.
    let laws = lines.iter().filter(|l| LAW_RE.is_match(l)).count();
    let per_kb = n * 1024 / text.len().max(1);
    per_kb >= 6 && laws == 0
}

/// Mechanical split (no model) for a SKILL.md over [`SPLIT_THRESHOLD`]. Returns the
/// edits relative to the entry dir, or nothing when the skill is small enough.
pub fn mechanical_split(skill_md: &str, name: &str) -> Result<Vec<Edit>> {
    mechanical_split_on(skill_md, name, chrono::Utc::now().date_naive())
}

pub fn mechanical_split_on(skill_md: &str, name: &str, today: NaiveDate) -> Result<Vec<Edit>> {
    if skill_md.len() <= SPLIT_THRESHOLD {
        return Ok(Vec::new());
    }
    let doc = SkillDoc::parse(skill_md)?;
    let as_of = today.format("%Y-%m-%d").to_string();
    let (pre, secs) = sections(&doc.body, "## ");

    // Title and trigger line.
    let title = pre
        .iter()
        .find_map(|l| l.strip_prefix("# ").map(|t| t.trim().to_string()))
        .unwrap_or_else(|| name.to_string());
    let trigger_idx = pre.iter().position(|l| TRIGGER_RE.is_match(l));
    let trigger = match trigger_idx {
        Some(i) => pre[i].trim().to_string(),
        None => match doc.body.lines().find(|l| TRIGGER_RE.is_match(l)) {
            Some(l) => l.trim().to_string(),
            None => format!(
                "Use when the task involves: {}",
                doc.get_str("description").unwrap_or(name).trim()
            ),
        },
    };

    let mut edits = Vec::new();
    let mut index: Vec<String> = Vec::new();
    let mut laws: Vec<String> = Vec::new();
    let mut laws_bytes = 0usize;
    let mut notes: Vec<String> = Vec::new();
    let mut used: Vec<String> = vec!["ledger-notes".into()];
    let mut unique = |s: String| {
        let mut cand = s.clone();
        let mut n = 2;
        while used.contains(&cand) {
            cand = format!("{s}-{n}");
            n += 1;
        }
        used.push(cand.clone());
        cand
    };

    // Preamble remainder (anything but the H1 and the trigger line) → overview unit.
    let overview: Vec<String> = pre
        .iter()
        .enumerate()
        .filter(|(i, l)| Some(*i) != trigger_idx && !l.starts_with("# "))
        .map(|(_, l)| l.clone())
        .collect();
    if overview.iter().any(|l| !l.trim().is_empty() && l.trim() != "---") {
        let path = format!("units/{}.md", unique("overview".into()));
        edits.push(Edit::Write {
            path: path.clone(),
            content: format!("# Overview\n\n{}\n", overview.join("\n").trim()),
        });
        index.push(format!("- [Overview]({path})"));
    }

    for sec in &secs {
        if REJECTED_RE.is_match(&sec.heading) {
            notes.push(format!("## {}\n", sec.heading));
            notes.extend(sec.lines.iter().skip(1).cloned());
            notes.push(String::new());
            continue;
        }
        let kept = extract_ledger_parts(sec, &mut notes);
        if !meaningful(&kept) {
            continue;
        }
        let s = unique(slug(&sec.heading));
        let facts = is_facts_section(&sec.heading, &kept);
        let path = if facts { format!("facts/{s}.md") } else { format!("units/{s}.md") };
        let text = kept.join("\n");
        let text = text.trim_end_matches(|c: char| c.is_whitespace() || c == '-').to_string();
        let content = if facts {
            format!("---\nas_of: {as_of}\nsource: SKILL.md\n---\n{text}\n")
        } else {
            format!("{text}\n")
        };
        edits.push(Edit::Write { path: path.clone(), content });
        index.push(if facts {
            format!("- [{}]({path}) — measured values, as_of {as_of}", sec.heading)
        } else {
            format!("- [{}]({path})", sec.heading)
        });
        for l in &kept {
            if !LAW_RE.is_match(l) || is_table_line(l) {
                continue;
            }
            let item = l.trim().trim_start_matches(|c: char| matches!(c, '-' | '*' | '+') || c.is_ascii_digit());
            let item = item.trim_start_matches([')', '.']).trim();
            let law = format!("- {item} ({path})");
            if law.len() > MAX_LAW_BYTES || laws_bytes + law.len() + 1 > ROUTER_LAWS_BUDGET {
                continue;
            }
            laws_bytes += law.len() + 1;
            laws.push(law);
        }
    }

    if !notes.is_empty() {
        let body = notes.join("\n");
        edits.push(Edit::Write {
            path: LEDGER_NOTES.into(),
            content: format!(
                "---\nledger: true\nas_of: {as_of}\n---\n# Ledger notes\n\nLong tables and rejected hypotheses moved out of the router; the ledger ingests this unit.\n\n{}\n",
                body.trim()
            ),
        });
        index.push(format!("- [Ledger notes]({LEDGER_NOTES}) — long tables and rejected hypotheses, flagged for the ledger"));
    }

    let mut router = format!("\n# {title}\n\n{trigger}\n");
    if !laws.is_empty() {
        router.push_str("\n## Laws\n\n");
        router.push_str(&laws.join("\n"));
        router.push('\n');
    }
    router.push_str("\n## Units\n\nRead the unit that matches the step at hand (`skill://");
    router.push_str(name);
    router.push_str("/<path>`).\n\n");
    router.push_str(&index.join("\n"));
    router.push('\n');
    let router_doc = SkillDoc { frontmatter: doc.frontmatter.clone(), body: router };
    edits.insert(0, Edit::Write { path: "SKILL.md".into(), content: router_doc.render() });
    Ok(edits)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FORGE: &str = include_str!("../tests/fixtures/forge-portfolio-weighting/SKILL.md");

    fn day() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 25).unwrap()
    }

    #[test]
    fn frontmatter_roundtrip_preserves_unknown_keys_and_merges_rematch() {
        let src = "---\nname: x\ndescription: d\nglobs:\n- '*.rs'\nrematch:\n  custom: keep\n  status: draft\n---\n# Body\n";
        let mut d = SkillDoc::parse(src).unwrap();
        assert_eq!(d.get_str("name"), Some("x"));
        assert_eq!(d.body, "# Body\n");
        let b = RematchBlock {
            id: "e1".into(),
            status: Status::Provisional,
            branch: "repo".into(),
            family: Family("a/x".into()),
            version: 2,
            predicate_hash: "h".into(),
        };
        d.set_rematch(&b);
        let out = d.render();
        let back = SkillDoc::parse(&out).unwrap();
        assert_eq!(back.rematch().unwrap(), b);
        assert_eq!(back.frontmatter["rematch"]["custom"], Value::from("keep"));
        assert!(back.frontmatter.contains_key("globs"));
        let keys: Vec<&str> = back.frontmatter.keys().filter_map(Value::as_str).collect();
        assert_eq!(keys, vec!["name", "description", "globs", "rematch"]);
        assert_eq!(back.body, "# Body\n");
    }

    #[test]
    fn no_frontmatter_and_errors() {
        let d = SkillDoc::parse("# Just body\n").unwrap();
        assert!(d.frontmatter.is_empty());
        assert_eq!(d.render(), "# Just body\n");
        assert!(SkillDoc::parse("---\nname: x\n# never closed\n").is_err());
        let e = SkillDoc::parse("---\n---\nbody").unwrap();
        assert!(e.frontmatter.is_empty() && e.body == "body");
        assert_eq!(router_bytes("---\nname: x\n---\n12345"), 5);
    }

    #[test]
    fn slugs() {
        assert_eq!(slug("1. Non-Negotiable Estimation & Data Laws"), "non-negotiable-estimation-data-laws");
        let long = slug("4. Native Rust Rayon QF-WIX v1 Results (`VALIDATION_RECEIPT_20261024.json`, Built in `1.25s`)");
        assert_eq!(long, "native-rust-rayon-qf-wix-v1-results-validation");
        assert_eq!(slug("!!!"), "section");
    }

    #[test]
    fn small_skill_is_not_split() {
        assert!(mechanical_split_on("---\nname: a\n---\n# A\n", "a", day()).unwrap().is_empty());
    }

    fn get<'a>(edits: &'a [Edit], path: &str) -> Option<&'a str> {
        edits.iter().find_map(|e| match e {
            Edit::Write { path: p, content } if p == path => Some(content.as_str()),
            _ => None,
        })
    }

    #[test]
    fn splits_forge_portfolio_weighting() {
        assert!(FORGE.len() > 30_000);
        let edits = mechanical_split_on(FORGE, "forge-portfolio-weighting", day()).unwrap();
        let router = get(&edits, "SKILL.md").unwrap();
        let rdoc = SkillDoc::parse(router).unwrap();
        assert_eq!(rdoc.get_str("name"), Some("forge-portfolio-weighting"));
        assert!(rdoc.get_str("description").unwrap().starts_with("Walk-forward portfolio weighting"));
        assert!(rdoc.body.len() < SPLIT_THRESHOLD, "router {} bytes", rdoc.body.len());
        assert!(rdoc.body.contains("Use this skill whenever designing"));
        assert!(rdoc.body.contains("## Laws"));
        for l in rdoc.body.lines().skip_while(|l| *l != "## Laws").skip(2).take_while(|l| l.starts_with("- ")) {
            assert!(LAW_RE.is_match(l), "router law without MUST/NEVER/ALWAYS: {l}");
        }
        // every H2 section accounted for: a unit/fact or ledger notes
        let paths: Vec<&str> = edits
            .iter()
            .map(|e| match e {
                Edit::Write { path, .. } | Edit::Delete { path } => path.as_str(),
            })
            .collect();
        assert!(paths.iter().any(|p| p.starts_with("units/") && p.contains("laws")), "{paths:?}");
        // the 30+ row results table goes to the ledger notes
        let notes = get(&edits, LEDGER_NOTES).unwrap();
        assert!(notes.starts_with("---\nledger: true\nas_of: 2026-10-25\n---\n"));
        assert!(notes.contains("BW_MT_A24_C10"));
        assert!(notes.lines().filter(|l| l.starts_with('|')).count() > LONG_TABLE_ROWS);
        // no unit keeps a long table
        for e in &edits {
            if let Edit::Write { path, content } = e {
                if path != LEDGER_NOTES {
                    let mut run = 0;
                    let mut max = 0;
                    for l in content.lines() {
                        run = if l.trim_start().starts_with('|') { run + 1 } else { 0 };
                        max = max.max(run);
                    }
                    assert!(max <= LONG_TABLE_ROWS + 2, "{path} keeps a {max}-line table");
                }
                if path.starts_with("facts/") {
                    assert!(content.starts_with("---\nas_of: 2026-10-25\n"), "{path} lacks as_of");
                }
                assert!(router.contains(path.as_str()) || path == "SKILL.md", "{path} not indexed");
            }
        }
        // nothing lost: every non-blank body line of the original appears somewhere
        let all: String = edits
            .iter()
            .filter_map(|e| match e {
                Edit::Write { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let orig = SkillDoc::parse(FORGE).unwrap();
        for l in orig.body.lines() {
            let t = l.trim();
            if t.is_empty() || t == "---" {
                continue;
            }
            assert!(all.contains(t), "lost line: {t}");
        }
        // the split output is itself a package
        let tmp = tempfile::tempdir().unwrap();
        for e in &edits {
            if let Edit::Write { path, content } = e {
                let p = tmp.path().join(path);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(p, content).unwrap();
            }
        }
        assert!(is_package(tmp.path()));
    }

    #[test]
    fn rejected_sections_and_measured_sections_route() {
        let mut md = String::from("---\nname: s\ndescription: d\n---\n# S\n\nUse when testing.\n\n## Procedure\n\n- You MUST run tests first.\n- Then commit.\n\n## Rejected hypotheses\n\n- caching helped (it did not)\n\n## Benchmarks\n\n- p50 12 ms, p99 40 ms, cache hit 93%\n");
        md.push_str(&"filler line\n".repeat(800));
        let edits = mechanical_split_on(&md, "s", day()).unwrap();
        let router = get(&edits, "SKILL.md").unwrap();
        assert!(router.contains("- You MUST run tests first. (units/procedure.md)"));
        assert!(get(&edits, LEDGER_NOTES).unwrap().contains("caching helped"));
        assert!(get(&edits, "facts/benchmarks.md").unwrap().starts_with("---\nas_of: 2026-10-25"));
        assert!(get(&edits, "units/rejected-hypotheses.md").is_none());
    }

    #[test]
    fn lenient_frontmatter_keeps_raw_values_and_renders_valid_yaml() {
        let src = "---\nname: x\ndescription: Design (v1: Kelly) bounds\nglobs:\n  - '*.rs'\n---\nbody\n";
        let d = SkillDoc::parse(src).unwrap();
        assert_eq!(d.get_str("description"), Some("Design (v1: Kelly) bounds"));
        assert_eq!(d.frontmatter["globs"][0], Value::from("*.rs"));
        let out = d.render();
        let strict: Mapping = serde_yaml::from_str(out.split("---\n").nth(1).unwrap()).unwrap();
        assert_eq!(strict["description"], Value::from("Design (v1: Kelly) bounds"));
    }
}
