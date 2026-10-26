//! Near-duplicate routing (SPEC "Caps and rates"; DESIGN §8): simhash over normalized
//! text with Hamming ≤ 6, or an identical script body ⇒ the candidate routes to the
//! existing entry with evidence appended.

use std::path::Path;

use anyhow::Result;

use crate::package::SkillDoc;

pub const NEAR_DUP_BITS: u32 = 6;

/// Lowercase, keep alphanumerics, collapse everything else to single spaces.
pub fn normalize_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = true;
    for c in s.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
            space = false;
        } else if !space {
            out.push(' ');
            space = true;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // splitmix finalizer for well-spread bits
    h = (h ^ (h >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h = (h ^ (h >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^ (h >> 31)
}

/// 64-bit simhash over word unigrams and bigrams of already-normalized text.
pub fn simhash64(normalized: &str) -> u64 {
    let words: Vec<&str> = normalized.split_whitespace().collect();
    let mut acc = [0i64; 64];
    let mut add = |feature: &str| {
        let h = fnv1a(feature.as_bytes());
        for (i, a) in acc.iter_mut().enumerate() {
            if h >> i & 1 == 1 {
                *a += 1;
            } else {
                *a -= 1;
            }
        }
    };
    for w in &words {
        add(w);
    }
    for pair in words.windows(2) {
        add(&format!("{} {}", pair[0], pair[1]));
    }
    acc.iter()
        .enumerate()
        .fold(0u64, |h, (i, a)| if *a > 0 { h | (1 << i) } else { h })
}

pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

pub fn near_duplicate(a: &str, b: &str) -> bool {
    let (na, nb) = (normalize_text(a), normalize_text(b));
    if na.is_empty() || nb.is_empty() {
        return false;
    }
    hamming(simhash64(&na), simhash64(&nb)) <= NEAR_DUP_BITS
}

/// Script identity ignoring line endings, trailing whitespace and blank lines.
pub fn identical_script(a: &str, b: &str) -> bool {
    let norm = |s: &str| -> Vec<String> {
        s.lines().map(|l| l.trim_end().to_string()).filter(|l| !l.is_empty()).collect()
    };
    let (na, nb) = (norm(a), norm(b));
    !na.is_empty() && na == nb
}

/// Comparable content of an entry dir: SKILL.md body + units + facts (sorted by path),
/// and script bodies.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EntryContent {
    pub text: String,
    pub scripts: Vec<String>,
}

pub fn entry_content(dir: &Path) -> Result<EntryContent> {
    let mut c = EntryContent::default();
    if let Ok(md) = std::fs::read_to_string(dir.join("SKILL.md")) {
        c.text.push_str(&SkillDoc::parse(&md).map(|d| d.body).unwrap_or(md));
    }
    for sub in ["units", "facts"] {
        for p in sorted_files(&dir.join(sub))? {
            let t = std::fs::read_to_string(&p)?;
            c.text.push('\n');
            c.text.push_str(&SkillDoc::parse(&t).map(|d| d.body).unwrap_or(t));
        }
    }
    for p in sorted_files(&dir.join("scripts"))? {
        c.scripts.push(std::fs::read_to_string(&p)?);
    }
    Ok(c)
}

fn sorted_files(dir: &Path) -> Result<Vec<std::path::PathBuf>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut v: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file())
        .collect();
    v.sort();
    Ok(v)
}

/// First existing entry the candidate duplicates (identical script first, then text).
pub fn route_duplicate<'a, K>(candidate: &EntryContent, existing: &'a [(K, EntryContent)]) -> Option<&'a K> {
    existing
        .iter()
        .find(|(_, e)| {
            candidate.scripts.iter().any(|s| e.scripts.iter().any(|t| identical_script(s, t)))
        })
        .or_else(|| existing.iter().find(|(_, e)| near_duplicate(&candidate.text, &e.text)))
        .map(|(k, _)| k)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "Use when editing the ledger schema. Always run the migration test before \
        committing. Never drop a column in place; add a new migration that copies data, then \
        verify the row counts match. Keep JSON columns for nested data and index only the \
        columns that filters use. Record the schema version after every migration step.";

    #[test]
    fn normalize() {
        assert_eq!(normalize_text("  Hello, WORLD!\n\n`x_y`  "), "hello world x y");
        assert_eq!(normalize_text("---"), "");
    }

    #[test]
    fn hamming_counts_bits() {
        assert_eq!(hamming(0, 0), 0);
        assert_eq!(hamming(0b1011, 0b0001), 2);
        assert_eq!(hamming(0, u64::MAX), 64);
    }

    #[test]
    fn formatting_changes_are_duplicates() {
        let reformatted = BASE.replace(". ", ".\n\n- ").to_uppercase();
        assert_eq!(simhash64(&normalize_text(BASE)), simhash64(&normalize_text(&reformatted)));
        assert!(near_duplicate(BASE, &reformatted));
    }

    #[test]
    fn small_edit_is_near_and_different_text_is_far() {
        let edited = BASE.replace("row counts", "record counts");
        let d = hamming(simhash64(&normalize_text(BASE)), simhash64(&normalize_text(&edited)));
        assert!(d <= NEAR_DUP_BITS, "distance {d}");
        let other = "Use when writing animation retargeting scripts for Blender rigs. Bake \
            the action before export and verify bone roll on the hips; compare frame 0 \
            against the reference pose and keep root motion on the armature object.";
        let d2 = hamming(simhash64(&normalize_text(BASE)), simhash64(&normalize_text(other)));
        assert!(d2 > NEAR_DUP_BITS, "distance {d2}");
        assert!(!near_duplicate(BASE, other));
        assert!(!near_duplicate("", ""));
    }

    #[test]
    fn scripts_identical_modulo_whitespace() {
        assert!(identical_script("#!/bin/sh\necho hi  \n\n", "#!/bin/sh\r\necho hi\n"));
        assert!(!identical_script("echo hi\n", "echo bye\n"));
        assert!(!identical_script("\n\n", "\n"));
    }

    #[test]
    fn route_prefers_identical_script_then_text() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path();
        std::fs::create_dir_all(d.join("scripts")).unwrap();
        std::fs::create_dir_all(d.join("units")).unwrap();
        std::fs::write(d.join("SKILL.md"), format!("---\nname: a\n---\n{BASE}")).unwrap();
        std::fs::write(d.join("units/u.md"), "extra unit").unwrap();
        std::fs::write(d.join("scripts/check.sh"), "cargo test -p x\n").unwrap();
        let c = entry_content(d).unwrap();
        assert!(c.text.contains("extra unit") && !c.text.contains("name: a"));
        assert_eq!(c.scripts, vec!["cargo test -p x\n".to_string()]);

        let by_text = EntryContent { text: BASE.to_string(), scripts: vec![] };
        let by_script = EntryContent { text: "unrelated words entirely".into(), scripts: vec!["cargo test -p x".into()] };
        let existing = vec![("text", by_text), ("script", by_script)];
        assert_eq!(route_duplicate(&c, &existing), Some(&"script"));
        let no_script = EntryContent { text: c.text.clone(), scripts: vec![] };
        assert_eq!(route_duplicate(&no_script, &existing), Some(&"text"));
        let fresh = EntryContent { text: "Blender rig retargeting procedure for exports".into(), scripts: vec![] };
        assert_eq!(route_duplicate(&fresh, &existing), None);
    }
}
