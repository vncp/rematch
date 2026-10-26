//! Text features shared by predicates, redirect heuristics and outcomes: tokenization,
//! English stop words, Porter stemming, Jaccard similarity, tf-idf ranking, file
//! extensions and read-selector stripping. Everything here is deterministic and pure.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;

/// Token rule shared with the extension: `text.toLowerCase().match(/[a-z0-9]+/g)`.
pub const TOKEN_PATTERN: &str = "[a-z0-9]+";

/// Inputs considered for session stems (DESIGN §6: first 5 `input` texts).
pub const STEM_INPUTS: usize = 5;

/// Maximum stems stored on a predicate (DESIGN §6).
pub const MAX_PREDICATE_STEMS: usize = 10;

/// Lowercased ASCII alphanumeric runs, in order, duplicates kept. Equivalent to
/// [`TOKEN_PATTERN`] applied to the lowercased text.
pub fn tokenize(text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in lower.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            cur.push(c);
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

pub fn token_set(text: &str) -> BTreeSet<String> {
    tokenize(text).into_iter().collect()
}

/// |A ∩ B| / |A ∪ B| over the distinct elements; 0.0 when both are empty.
pub fn jaccard<S: AsRef<str>>(a: &[S], b: &[S]) -> f64 {
    let a: HashSet<&str> = a.iter().map(AsRef::as_ref).collect();
    let b: HashSet<&str> = b.iter().map(AsRef::as_ref).collect();
    let union = a.union(&b).count();
    if union == 0 {
        return 0.0;
    }
    a.intersection(&b).count() as f64 / union as f64
}

/// English stop words (NLTK list plus contraction fragments and chat filler).
pub const STOP_WORDS: &[&str] = &[
    "a",
    "about",
    "above",
    "actually",
    "after",
    "again",
    "against",
    "ain",
    "all",
    "also",
    "am",
    "an",
    "and",
    "any",
    "are",
    "aren",
    "as",
    "at",
    "be",
    "because",
    "been",
    "before",
    "being",
    "below",
    "between",
    "both",
    "but",
    "by",
    "can",
    "could",
    "couldn",
    "d",
    "did",
    "didn",
    "do",
    "does",
    "doesn",
    "doing",
    "don",
    "down",
    "during",
    "each",
    "few",
    "for",
    "from",
    "further",
    "get",
    "got",
    "had",
    "hadn",
    "has",
    "hasn",
    "have",
    "haven",
    "having",
    "he",
    "her",
    "here",
    "hers",
    "herself",
    "him",
    "himself",
    "his",
    "how",
    "i",
    "if",
    "in",
    "into",
    "is",
    "isn",
    "it",
    "its",
    "itself",
    "just",
    "let",
    "ll",
    "m",
    "ma",
    "me",
    "might",
    "mightn",
    "more",
    "most",
    "must",
    "mustn",
    "my",
    "myself",
    "needn",
    "no",
    "nor",
    "not",
    "now",
    "o",
    "of",
    "off",
    "ok",
    "okay",
    "on",
    "once",
    "only",
    "or",
    "other",
    "our",
    "ours",
    "ourselves",
    "out",
    "over",
    "own",
    "please",
    "pls",
    "re",
    "s",
    "same",
    "shan",
    "she",
    "should",
    "shouldn",
    "so",
    "some",
    "such",
    "t",
    "than",
    "thank",
    "thanks",
    "that",
    "the",
    "their",
    "theirs",
    "them",
    "themselves",
    "then",
    "there",
    "these",
    "they",
    "this",
    "those",
    "through",
    "to",
    "too",
    "under",
    "until",
    "up",
    "ve",
    "very",
    "was",
    "wasn",
    "we",
    "were",
    "weren",
    "what",
    "when",
    "where",
    "which",
    "while",
    "who",
    "whom",
    "why",
    "will",
    "with",
    "won",
    "would",
    "wouldn",
    "y",
    "yeah",
    "yes",
    "you",
    "your",
    "yours",
    "yourself",
    "yourselves",
];

pub fn is_stop_word(token: &str) -> bool {
    STOP_WORDS.binary_search(&token).is_ok()
}

/// Tokens worth stemming: not a stop word, ≥ 3 chars, ≤ 30 chars, not purely numeric and
/// not an identifier-like hex run (hashes, uuids, session ids).
pub fn is_content_token(token: &str) -> bool {
    let n = token.len();
    if !(3..=30).contains(&n) || is_stop_word(token) {
        return false;
    }
    if token.bytes().all(|c| c.is_ascii_digit()) {
        return false;
    }
    let hexish = token.bytes().all(|c| c.is_ascii_hexdigit());
    let has_digit = token.bytes().any(|c| c.is_ascii_digit());
    !(hexish && has_digit && n >= 7)
}

/// Porter-stemmed content tokens of `text`, in order, duplicates kept (term frequencies).
pub fn stems_of_text(text: &str) -> Vec<String> {
    tokenize(text)
        .into_iter()
        .filter(|t| is_content_token(t))
        .map(|t| porter_stem(&t))
        .collect()
}

/// Session stems for count-time matching: distinct stems (first-seen order) of the first
/// [`STEM_INPUTS`] inputs plus the basenames (without extension) of touched files.
pub fn stems_of_session(inputs: &[String], files: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut push = |s: String| {
        if seen.insert(s.clone()) {
            out.push(s);
        }
    };
    for input in inputs.iter().take(STEM_INPUTS) {
        stems_of_text(input).into_iter().for_each(&mut push);
    }
    for f in files {
        let p = Path::new(strip_selector(f));
        let base = p.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
        stems_of_text(base).into_iter().for_each(&mut push);
    }
    out
}

/// Distinct terms of `doc_terms` ranked by tf-idf against `corpus` (each corpus document a
/// bag of terms). tf = occurrences in the doc; idf = ln((N + 1) / (df + 1)) + 1 with N the
/// corpus size and df the documents containing the term. Ties break by first occurrence
/// in the doc. Callers truncate (predicates keep [`MAX_PREDICATE_STEMS`]).
pub fn rank_stems(doc_terms: &[String], corpus: &[Vec<String>]) -> Vec<String> {
    let mut tf: BTreeMap<&str, (usize, usize)> = BTreeMap::new(); // term → (count, first index)
    for (i, t) in doc_terms.iter().enumerate() {
        tf.entry(t.as_str()).or_insert((0, i)).0 += 1;
    }
    let mut df: BTreeMap<&str, usize> = BTreeMap::new();
    for doc in corpus {
        let distinct: HashSet<&str> = doc.iter().map(String::as_str).collect();
        for t in distinct {
            if let Some(d) = tf.get_key_value(t).map(|(k, _)| *k) {
                *df.entry(d).or_insert(0) += 1;
            }
        }
    }
    let n = corpus.len() as f64;
    let mut scored: Vec<(&str, f64, usize)> = tf
        .iter()
        .map(|(t, (count, first))| {
            let d = *df.get(t).unwrap_or(&0) as f64;
            let idf = ((n + 1.0) / (d + 1.0)).ln() + 1.0;
            (*t, *count as f64 * idf, *first)
        })
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.2.cmp(&b.2)));
    scored.into_iter().map(|(t, _, _)| t.to_string()).collect()
}

/// Distinct lowercased extensions (no dot) of `paths`, sorted. Read selectors are stripped
/// first and `scheme://` URLs contribute only if they carry an extension.
pub fn exts_of<S: AsRef<str>>(paths: &[S]) -> Vec<String> {
    let set: BTreeSet<String> = paths
        .iter()
        .filter_map(|p| {
            Path::new(strip_selector(p.as_ref()))
                .extension()
                .and_then(|e| e.to_str())
                .filter(|e| !e.is_empty() && e.bytes().all(|c| c.is_ascii_alphanumeric()))
                .map(str::to_ascii_lowercase)
        })
        .collect();
    set.into_iter().collect()
}

/// Strips omp `read` selectors from the end of a path: `foo.rs:50-80`, `foo.rs:raw`,
/// `foo.rs:raw:1-4`, `foo.rs:-5`, `foo.rs:5-16,960-973`, `foo.rs:50+150`, `x.svg:img`,
/// `f.json?q=.a`. Only the last path component is inspected.
pub fn strip_selector(path: &str) -> &str {
    let path = path.trim();
    let base_start = path.rfind('/').map_or(0, |i| i + 1);
    let mut end = path.len();
    if let Some(q) = path[base_start..].find('?') {
        end = base_start + q;
    }
    loop {
        let cur = &path[base_start..end];
        let Some(colon) = cur.rfind(':') else { break };
        let seg = &cur[colon + 1..];
        let is_selector = matches!(seg, "raw" | "img" | "conflicts")
            || (!seg.is_empty()
                && seg.bytes().any(|c| c.is_ascii_digit())
                && seg
                    .bytes()
                    .all(|c| c.is_ascii_digit() || matches!(c, b'-' | b'+' | b',')));
        if !is_selector || colon == 0 {
            break;
        }
        end = base_start + colon;
    }
    &path[..end]
}

/// First `max` chars of `s` (char-boundary safe).
pub fn head(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => s[..i].to_string(),
        None => s.to_string(),
    }
}

// ---- Porter (1980) stemmer, after Martin Porter's reference C implementation ----

/// Porter stem of a lowercase ASCII word. Words of ≤ 2 letters and tokens containing
/// anything but `a-z` are returned unchanged.
pub fn porter_stem(word: &str) -> String {
    if word.len() <= 2 || !word.bytes().all(|c| c.is_ascii_lowercase()) {
        return word.to_string();
    }
    let mut s = Stemmer {
        b: word.as_bytes().to_vec(),
        k: word.len() as isize - 1,
        j: 0,
    };
    s.step1ab();
    if s.k > 0 {
        s.step1c();
        s.step2();
        s.step3();
        s.step4();
        s.step5();
    }
    s.b.truncate((s.k + 1) as usize);
    String::from_utf8(s.b).expect("ascii")
}

struct Stemmer {
    b: Vec<u8>,
    k: isize,
    j: isize,
}

impl Stemmer {
    fn at(&self, i: isize) -> u8 {
        self.b[i as usize]
    }

    fn cons(&self, i: isize) -> bool {
        match self.at(i) {
            b'a' | b'e' | b'i' | b'o' | b'u' => false,
            b'y' => i == 0 || !self.cons(i - 1),
            _ => true,
        }
    }

    /// Number of VC sequences in b[0..=j].
    fn m(&self) -> usize {
        let mut n = 0;
        let mut i = 0;
        loop {
            if i > self.j {
                return n;
            }
            if !self.cons(i) {
                break;
            }
            i += 1;
        }
        i += 1;
        loop {
            loop {
                if i > self.j {
                    return n;
                }
                if self.cons(i) {
                    break;
                }
                i += 1;
            }
            i += 1;
            n += 1;
            loop {
                if i > self.j {
                    return n;
                }
                if !self.cons(i) {
                    break;
                }
                i += 1;
            }
            i += 1;
        }
    }

    fn vowel_in_stem(&self) -> bool {
        (0..=self.j).any(|i| !self.cons(i))
    }

    fn doublec(&self, j: isize) -> bool {
        j >= 1 && self.at(j) == self.at(j - 1) && self.cons(j)
    }

    fn cvc(&self, i: isize) -> bool {
        if i < 2 || !self.cons(i) || self.cons(i - 1) || !self.cons(i - 2) {
            return false;
        }
        !matches!(self.at(i), b'w' | b'x' | b'y')
    }

    /// Sets j on success only (as the reference implementation does).
    fn ends(&mut self, s: &str) -> bool {
        let l = s.len() as isize;
        if l > self.k + 1 {
            return false;
        }
        let start = (self.k + 1 - l) as usize;
        if &self.b[start..=(self.k as usize)] != s.as_bytes() {
            return false;
        }
        self.j = self.k - l;
        true
    }

    fn setto(&mut self, s: &str) {
        let start = (self.j + 1) as usize;
        self.b.truncate(start);
        self.b.extend_from_slice(s.as_bytes());
        self.k = self.j + s.len() as isize;
    }

    fn r(&mut self, s: &str) {
        if self.m() > 0 {
            self.setto(s);
        }
    }

    fn step1ab(&mut self) {
        if self.at(self.k) == b's' {
            if self.ends("sses") {
                self.k -= 2;
            } else if self.ends("ies") {
                self.setto("i");
            } else if self.at(self.k - 1) != b's' {
                self.k -= 1;
            }
        }
        if self.ends("eed") {
            if self.m() > 0 {
                self.k -= 1;
            }
        } else if (self.ends("ed") || self.ends("ing")) && self.vowel_in_stem() {
            self.k = self.j;
            if self.ends("at") {
                self.setto("ate");
            } else if self.ends("bl") {
                self.setto("ble");
            } else if self.ends("iz") {
                self.setto("ize");
            } else if self.doublec(self.k) {
                self.k -= 1;
                if matches!(self.at(self.k), b'l' | b's' | b'z') {
                    self.k += 1;
                }
            } else if self.m() == 1 && self.cvc(self.k) {
                self.setto("e");
            }
        }
    }

    fn step1c(&mut self) {
        if self.ends("y") && self.vowel_in_stem() {
            let k = self.k as usize;
            self.b[k] = b'i';
        }
    }

    fn replace_first(&mut self, rules: &[(&str, &str)]) {
        for (suffix, repl) in rules {
            if self.ends(suffix) {
                self.r(repl);
                return;
            }
        }
    }

    fn step2(&mut self) {
        if self.k < 1 {
            return;
        }
        let rules: &[(&str, &str)] = match self.at(self.k - 1) {
            b'a' => &[("ational", "ate"), ("tional", "tion")],
            b'c' => &[("enci", "ence"), ("anci", "ance")],
            b'e' => &[("izer", "ize")],
            b'l' => &[
                ("bli", "ble"),
                ("alli", "al"),
                ("entli", "ent"),
                ("eli", "e"),
                ("ousli", "ous"),
            ],
            b'o' => &[("ization", "ize"), ("ation", "ate"), ("ator", "ate")],
            b's' => &[
                ("alism", "al"),
                ("iveness", "ive"),
                ("fulness", "ful"),
                ("ousness", "ous"),
            ],
            b't' => &[("aliti", "al"), ("iviti", "ive"), ("biliti", "ble")],
            b'g' => &[("logi", "log")],
            _ => &[],
        };
        self.replace_first(rules);
    }

    fn step3(&mut self) {
        let rules: &[(&str, &str)] = match self.at(self.k) {
            b'e' => &[("icate", "ic"), ("ative", ""), ("alize", "al")],
            b'i' => &[("iciti", "ic")],
            b'l' => &[("ical", "ic"), ("ful", "")],
            b's' => &[("ness", "")],
            _ => &[],
        };
        self.replace_first(rules);
    }

    fn step4(&mut self) {
        if self.k < 1 {
            return;
        }
        let suffixes: &[&str] = match self.at(self.k - 1) {
            b'a' => &["al"],
            b'c' => &["ance", "ence"],
            b'e' => &["er"],
            b'i' => &["ic"],
            b'l' => &["able", "ible"],
            b'n' => &["ant", "ement", "ment", "ent"],
            b'o' => {
                // `-ion` only after s/t; otherwise `-ou`. An empty list means "matched".
                let ion = self.ends("ion") && self.j >= 0 && matches!(self.at(self.j), b's' | b't');
                if !(ion || self.ends("ou")) {
                    return;
                }
                &[]
            }
            b's' => &["ism"],
            b't' => &["ate", "iti"],
            b'u' => &["ous"],
            b'v' => &["ive"],
            b'z' => &["ize"],
            _ => return,
        };
        if !suffixes.is_empty() && !suffixes.iter().any(|s| self.ends(s)) {
            return;
        }
        if self.m() > 1 {
            self.k = self.j;
        }
    }

    fn step5(&mut self) {
        self.j = self.k;
        if self.at(self.k) == b'e' {
            let a = self.m();
            if a > 1 || (a == 1 && !self.cvc(self.k - 1)) {
                self.k -= 1;
            }
        }
        if self.at(self.k) == b'l' && self.doublec(self.k) && self.m() > 1 {
            self.k -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn stop_words_sorted_for_binary_search() {
        let mut sorted = STOP_WORDS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted, STOP_WORDS);
        assert!(is_stop_word("the") && is_stop_word("didn") && !is_stop_word("test"));
    }

    #[test]
    fn tokenize_matches_extension_rule() {
        assert_eq!(
            tokenize("Don't run `cargo-test` on v2.1!"),
            v(&["don", "t", "run", "cargo", "test", "on", "v2", "1"])
        );
        assert_eq!(tokenize("Été naïve"), v(&["t", "na", "ve"]));
        assert!(tokenize("  --  ").is_empty());
    }

    #[test]
    fn porter_reference_vocabulary() {
        let cases = [
            ("caresses", "caress"),
            ("ponies", "poni"),
            ("ties", "ti"),
            ("caress", "caress"),
            ("cats", "cat"),
            ("feed", "feed"),
            ("agreed", "agre"),
            ("plastered", "plaster"),
            ("bled", "bled"),
            ("motoring", "motor"),
            ("sing", "sing"),
            ("conflated", "conflat"),
            ("troubled", "troubl"),
            ("sized", "size"),
            ("hopping", "hop"),
            ("tanned", "tan"),
            ("falling", "fall"),
            ("hissing", "hiss"),
            ("fizzed", "fizz"),
            ("failing", "fail"),
            ("filing", "file"),
            ("happy", "happi"),
            ("sky", "sky"),
            ("relational", "relat"),
            ("conditional", "condit"),
            ("rational", "ration"),
            ("valenci", "valenc"),
            ("digitizer", "digit"),
            ("conformabli", "conform"),
            ("radicalli", "radic"),
            ("differentli", "differ"),
            ("vileli", "vile"),
            ("analogousli", "analog"),
            ("vietnamization", "vietnam"),
            ("predication", "predic"),
            ("operator", "oper"),
            ("feudalism", "feudal"),
            ("decisiveness", "decis"),
            ("hopefulness", "hope"),
            ("callousness", "callous"),
            ("formaliti", "formal"),
            ("sensitiviti", "sensit"),
            ("sensibiliti", "sensibl"),
            ("triplicate", "triplic"),
            ("formative", "form"),
            ("formalize", "formal"),
            ("electriciti", "electr"),
            ("electrical", "electr"),
            ("hopeful", "hope"),
            ("goodness", "good"),
            ("revival", "reviv"),
            ("allowance", "allow"),
            ("inference", "infer"),
            ("airliner", "airlin"),
            ("gyroscopic", "gyroscop"),
            ("adjustable", "adjust"),
            ("defensible", "defens"),
            ("irritant", "irrit"),
            ("replacement", "replac"),
            ("adjustment", "adjust"),
            ("dependent", "depend"),
            ("adoption", "adopt"),
            ("homologou", "homolog"),
            ("communism", "commun"),
            ("activate", "activ"),
            ("angulariti", "angular"),
            ("homologous", "homolog"),
            ("effective", "effect"),
            ("bowdlerize", "bowdler"),
            ("probate", "probat"),
            ("rate", "rate"),
            ("cease", "ceas"),
            ("controll", "control"),
            ("roll", "roll"),
            ("generalizations", "gener"),
            ("running", "run"),
            ("tests", "test"),
            ("testing", "test"),
            ("is", "is"),
            ("v2", "v2"),
        ];
        for (w, want) in cases {
            assert_eq!(porter_stem(w), want, "stem({w})");
        }
    }

    #[test]
    fn jaccard_boundaries() {
        assert_eq!(jaccard::<&str>(&[], &[]), 0.0);
        assert_eq!(jaccard(&["a", "b"], &["a", "b", "a"]), 1.0);
        assert!((jaccard(&["a", "b", "c"], &["b", "c", "d"]) - 0.5).abs() < 1e-12);
        assert_eq!(jaccard(&["a"], &["b"]), 0.0);
    }

    #[test]
    fn content_tokens_drop_noise() {
        assert_eq!(
            stems_of_text("Please fix the failing tests in 01a175cc 2024 parser.rs ok"),
            v(&["fix", "fail", "test", "parser"])
        );
        assert!(!is_content_token("deadbeef1"));
        assert!(
            is_content_token("deadbeef"),
            "pure-letter hex words are real words"
        );
    }

    #[test]
    fn session_stems_use_first_five_inputs_and_basenames() {
        let inputs = v(&["alpha", "beta", "gamma", "delta", "epsilon", "zeta"]);
        let files = v(&["/r/src/order_book.rs:10-20", "skill://quant-options"]);
        let stems = stems_of_session(&inputs, &files);
        assert!(!stems.contains(&"zeta".to_string()));
        assert!(stems.contains(&"order".to_string()) && stems.contains(&"book".to_string()));
        assert_eq!(stems.iter().filter(|s| *s == "alpha").count(), 1);
    }

    #[test]
    fn tf_idf_prefers_rare_and_frequent_terms() {
        let doc = v(&["parquet", "index", "parquet", "test"]);
        let corpus = vec![
            v(&["test", "index"]),
            v(&["test"]),
            v(&["test", "index", "foo"]),
        ];
        assert_eq!(rank_stems(&doc, &corpus), v(&["parquet", "index", "test"]));
        // Ties break by first occurrence, empty corpus is fine.
        assert_eq!(rank_stems(&v(&["b", "a"]), &[]), v(&["b", "a"]));
    }

    #[test]
    fn selectors_and_exts() {
        assert_eq!(strip_selector("src/a.rs:50-80"), "src/a.rs");
        assert_eq!(strip_selector("a.jsonl:raw:1-40"), "a.jsonl");
        assert_eq!(strip_selector("a.rs:5-16,960-973"), "a.rs");
        assert_eq!(strip_selector("a.rs:-5"), "a.rs");
        assert_eq!(strip_selector("a.rs:50+150"), "a.rs");
        assert_eq!(strip_selector("x.svg:img"), "x.svg");
        assert_eq!(strip_selector("f.json?q=.a"), "f.json");
        assert_eq!(strip_selector("skill://name:raw"), "skill://name");
        assert_eq!(strip_selector("host:8080/x.md"), "host:8080/x.md");
        assert_eq!(strip_selector("C:"), "C:");
        assert_eq!(
            exts_of(&[
                "a.RS",
                "b.rs:1-3",
                "Makefile",
                "c.tar.gz",
                "skill://x",
                "omp://ext.md"
            ]),
            v(&["gz", "md", "rs"])
        );
    }

    #[test]
    fn head_is_char_safe() {
        assert_eq!(head("héllo", 2), "hé");
        assert_eq!(head("hi", 10), "hi");
    }
}
