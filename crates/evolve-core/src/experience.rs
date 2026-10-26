//! EXPERIENCE.md (SPEC "Learning from the loop"): after every keep or retirement the
//! reflector appends a record beside the entry — situation, lesson, evidence, and what
//! the counters showed. The editor reads matching ones on later proposals; the system
//! never rewrites them (records are append-only).

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::ledger::{enum_str, parse_enum};
use crate::library::load_entry;
use crate::paths::EvolvePaths;
use crate::predicate::matches_repo;
use crate::types::{Counters, LessonTag, Moment};

pub const EXPERIENCE_MD: &str = "EXPERIENCE.md";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Experience {
    pub entry_id: Option<String>,
    pub name: Option<String>,
    pub written_at: String,
    pub tag: LessonTag,
    pub situation: String,
    pub lesson: String,
    pub evidence: Vec<Moment>,
    /// The counters line as written (`matched 4 · recurrence 0 · …`).
    pub counters: String,
    pub dir: PathBuf,
}

/// Lines of free text that would collide with record structure are demoted.
fn escape(text: &str) -> String {
    text.trim()
        .lines()
        .map(|l| if l.starts_with("## ") || l.starts_with("### ") { format!("####{}", l.trim_start_matches('#')) } else { l.to_string() })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn counters_line(c: &Counters) -> String {
    let mut s = format!(
        "matched {} · recurrence {} · harm {} · tokens {} · turns {}",
        c.matched, c.recurrence, c.harm, c.tokens, c.turns
    );
    for (f, fc) in &c.per_family {
        s.push_str(&format!("\n- {}: matched {}, recurrence {}, harm {}", f.0, fc.matched, fc.recurrence, fc.harm));
    }
    s
}

/// Append one record to `<dir>/EXPERIENCE.md`.
pub fn write_experience(
    dir: &Path,
    situation: &str,
    lesson: &str,
    tag: LessonTag,
    evidence: &[Moment],
    counters: &Counters,
) -> Result<()> {
    let at = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    write_experience_at(dir, &at, situation, lesson, tag, evidence, counters)
}

pub fn write_experience_at(
    dir: &Path,
    at: &str,
    situation: &str,
    lesson: &str,
    tag: LessonTag,
    evidence: &[Moment],
    counters: &Counters,
) -> Result<()> {
    use std::io::Write;
    let path = dir.join(EXPERIENCE_MD);
    let mut rec = String::new();
    if !path.exists() {
        rec.push_str("# Experience\n\nAppend-only records written after keep/retire decisions.\n");
    }
    rec.push_str(&format!("\n## {at} · {}\n\n### Situation\n\n{}\n\n### Lesson\n\n{}\n\n### Evidence\n\n", enum_str(&tag), escape(situation), escape(lesson)));
    for m in evidence {
        rec.push_str(&format!("- `{}`#{}\n", m.session_id, m.seq));
    }
    rec.push_str(&format!("\n### Counters\n\n{}\n", counters_line(counters)));
    std::fs::create_dir_all(dir)?;
    std::fs::OpenOptions::new().create(true).append(true).open(&path)?.write_all(rec.as_bytes())?;
    Ok(())
}

/// Parse every record of an EXPERIENCE.md.
pub fn parse_experience(text: &str, dir: &Path) -> Vec<Experience> {
    let mut out = Vec::new();
    let mut cur: Option<(Experience, String)> = None;
    let flush = |cur: &mut Option<(Experience, String)>, out: &mut Vec<Experience>| {
        if let Some((e, _)) = cur.take() {
            out.push(e);
        }
    };
    for line in text.lines() {
        if let Some(h) = line.strip_prefix("## ") {
            if let Some((at, tag)) = h.split_once(" · ") {
                if let Ok(tag) = parse_enum::<LessonTag>(tag.trim()) {
                    flush(&mut cur, &mut out);
                    cur = Some((
                        Experience {
                            entry_id: None,
                            name: None,
                            written_at: at.trim().into(),
                            tag,
                            situation: String::new(),
                            lesson: String::new(),
                            evidence: vec![],
                            counters: String::new(),
                            dir: dir.to_path_buf(),
                        },
                        String::new(),
                    ));
                    continue;
                }
            }
        }
        let Some((e, section)) = cur.as_mut() else { continue };
        if let Some(s) = line.strip_prefix("### ") {
            *section = s.trim().to_string();
            continue;
        }
        let push = |field: &mut String| {
            if !field.is_empty() || !line.trim().is_empty() {
                if !field.is_empty() {
                    field.push('\n');
                }
                field.push_str(line);
            }
        };
        match section.as_str() {
            "Situation" => push(&mut e.situation),
            "Lesson" => push(&mut e.lesson),
            "Counters" => push(&mut e.counters),
            "Evidence" => {
                if let Some(rest) = line.strip_prefix("- `") {
                    if let Some((sid, seq)) = rest.split_once("`#") {
                        if let Ok(seq) = seq.trim().parse() {
                            e.evidence.push(Moment { session_id: sid.into(), seq });
                        }
                    }
                }
            }
            _ => {}
        }
    }
    flush(&mut cur, &mut out);
    for e in &mut out {
        for f in [&mut e.situation, &mut e.lesson, &mut e.counters] {
            *f = f.trim_end().to_string();
        }
    }
    out
}

/// What the editor is about to work on.
#[derive(Debug, Clone, Default)]
pub struct ExperienceQuery {
    /// Repo (else cwd) of the session; `None` matches every repo glob.
    pub repo_or_cwd: Option<String>,
    pub stems: Vec<String>,
}

fn entry_dirs(paths: &EvolvePaths) -> Vec<PathBuf> {
    let children = |d: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(d)
            .map(|r| r.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.is_dir()).collect())
            .unwrap_or_default()
    };
    let mut dirs = Vec::new();
    for parent in [&paths.staging, &paths.archive] {
        for sub in children(parent) {
            dirs.extend(children(&sub));
        }
    }
    dirs.extend(children(&paths.drafts));
    dirs.extend(children(&paths.skills_dir()));
    dirs.sort();
    dirs
}

/// EXPERIENCE records of entries whose predicate matches the query: invariant lessons
/// ignore the repo glob; stems must overlap ≥ 1 when both sides have stems. Ordered by
/// stem overlap (desc), then newest first.
pub fn read_matching(paths: &EvolvePaths, q: &ExperienceQuery) -> Result<Vec<Experience>> {
    let mut scored: Vec<(usize, Experience)> = Vec::new();
    for dir in entry_dirs(paths) {
        let Ok(text) = std::fs::read_to_string(dir.join(EXPERIENCE_MD)) else { continue };
        let entry = load_entry(&dir).ok();
        for mut x in parse_experience(&text, &dir) {
            let (overlap, ok) = match &entry {
                Some(e) => {
                    let repo_ok = x.tag == LessonTag::Invariant
                        || q.repo_or_cwd.as_deref().is_none_or(|r| matches_repo(&e.predicate, r));
                    let overlap = e.predicate.stems.iter().filter(|s| q.stems.contains(s)).count();
                    let stems_ok = e.predicate.stems.is_empty() || q.stems.is_empty() || overlap >= 1;
                    (overlap, repo_ok && stems_ok)
                }
                None => (0, true),
            };
            if !ok {
                continue;
            }
            x.entry_id = entry.as_ref().map(|e| e.id.clone());
            x.name = entry.as_ref().map(|e| e.name.clone());
            scored.push((overlap, x));
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.written_at.cmp(&a.1.written_at)));
    Ok(scored.into_iter().map(|(_, x)| x).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::tests::entry;
    use crate::types::{Family, FamilyCounters, Status};

    fn counters() -> Counters {
        let mut c = Counters { matched: 4, tokens: 900, turns: 12, ..Default::default() };
        c.per_family.insert(Family("a/x".into()), FamilyCounters { matched: 4, recurrence: 0, harm: 0 });
        c
    }

    #[test]
    fn append_and_parse_roundtrip() {
        let t = tempfile::tempdir().unwrap();
        let ev = vec![Moment { session_id: "s1".into(), seq: 7 }];
        write_experience_at(t.path(), "2026-10-01T00:00:00Z", "Ledger edits\n## not a header", "Run the migration test.", LessonTag::Invariant, &ev, &counters()).unwrap();
        write_experience_at(t.path(), "2026-10-02T00:00:00Z", "Second", "Retired: harm twice.", LessonTag::Domain, &[], &Counters::default()).unwrap();
        let text = std::fs::read_to_string(t.path().join(EXPERIENCE_MD)).unwrap();
        assert_eq!(text.matches("# Experience").count(), 1);
        let recs = parse_experience(&text, t.path());
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].situation, "Ledger edits\n#### not a header");
        assert_eq!(recs[0].lesson, "Run the migration test.");
        assert_eq!(recs[0].evidence, ev);
        assert_eq!(recs[0].tag, LessonTag::Invariant);
        assert!(recs[0].counters.starts_with("matched 4 · recurrence 0 · harm 0"));
        assert!(recs[0].counters.contains("- a/x: matched 4"));
        assert_eq!(recs[1].tag, LessonTag::Domain);
        assert!(recs[1].evidence.is_empty());
    }

    #[test]
    fn read_matching_filters_by_predicate() {
        let t = tempfile::tempdir().unwrap();
        let paths = EvolvePaths::with_roots(t.path().join("ev"), t.path().join("agent"));
        let mk = |id: &str, dir: PathBuf, glob: &str, stems: &[&str]| {
            let mut e = entry(id, id, "repo", Status::Kept);
            e.predicate.repo_glob = glob.into();
            e.predicate.stems = stems.iter().map(|s| s.to_string()).collect();
            e.path = dir.clone();
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("evolve.json"), serde_json::to_string(&e).unwrap()).unwrap();
            dir
        };
        let a = mk("a", paths.skills_dir().join("a"), "/w/repo", &["ledger", "migrat"]);
        let b = mk("b", paths.archive.join("retired/b"), "/w/other", &["ledger"]);
        let c = mk("c", paths.staging.join("repo/c"), "**", &["anim"]);
        write_experience_at(&a, "2026-10-01T00:00:00Z", "a", "la", LessonTag::Domain, &[], &Counters::default()).unwrap();
        write_experience_at(&b, "2026-10-02T00:00:00Z", "b", "lb", LessonTag::Domain, &[], &Counters::default()).unwrap();
        write_experience_at(&b, "2026-10-03T00:00:00Z", "b2", "lb2", LessonTag::Invariant, &[], &Counters::default()).unwrap();
        write_experience_at(&c, "2026-10-04T00:00:00Z", "c", "lc", LessonTag::Domain, &[], &Counters::default()).unwrap();

        let q = ExperienceQuery { repo_or_cwd: Some("/w/repo".into()), stems: vec!["ledger".into(), "migrat".into()] };
        let got: Vec<String> = read_matching(&paths, &q).unwrap().into_iter().map(|x| x.situation).collect();
        // a: repo + 2 stems; b2: invariant ignores repo, 1 stem; b (domain, other repo) and c (no stem overlap) excluded
        assert_eq!(got, vec!["a", "b2"]);
        let all = read_matching(&paths, &ExperienceQuery::default()).unwrap();
        assert_eq!(all.len(), 4);
        assert_eq!(all[0].name.as_deref(), Some("c"), "ties: newest first");
    }
}
