//! Session outcome records (SPEC §"Session outcome records"): parameter normalization,
//! the exact `signature` ("same thing"), MinHash over normalized tokens ("close thing",
//! a suggestion never a gate), construction, and `self_reported` provenance marking.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::features::{stems_of_text, tokenize};
use crate::types::{
    Evidence, EvidenceKind, Facet, Family, Link, Outcome, OutcomeKind, OutcomeResult, Provenance,
    Sha256Hex, Subject, UnixMs,
};

/// MinHash signature length.
pub const MINHASH_K: usize = 64;

/// Normalizes outcome params so equal intent hashes equally: keys trimmed, lowercased,
/// runs of whitespace/`-` collapsed to `_`; strings trimmed, lowercased, inner whitespace
/// collapsed; integral floats become integers; nulls dropped; objects normalized
/// recursively (sorted keys); array order is kept (order can be meaningful).
pub fn normalize_params(params: &BTreeMap<String, Value>) -> BTreeMap<String, Value> {
    params
        .iter()
        .filter(|(_, v)| !v.is_null())
        .map(|(k, v)| (normalize_key(k), normalize_value(v)))
        .collect()
}

fn normalize_key(k: &str) -> String {
    let mut out = String::new();
    let mut sep = false;
    for c in k.trim().chars() {
        if c.is_whitespace() || c == '-' || c == '_' {
            sep = true;
        } else {
            if sep && !out.is_empty() {
                out.push('_');
            }
            sep = false;
            out.extend(c.to_lowercase());
        }
    }
    out
}

fn normalize_value(v: &Value) -> Value {
    match v {
        Value::String(s) => Value::String(
            s.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase(),
        ),
        Value::Number(n) => {
            if n.is_f64() {
                let f = n.as_f64().unwrap_or(0.0);
                if f.fract() == 0.0 && f.abs() < 9.0e15 {
                    return Value::from(f as i64);
                }
            }
            Value::Number(n.clone())
        }
        Value::Array(a) => Value::Array(a.iter().map(normalize_value).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (normalize_key(k), normalize_value(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// JSON with object keys sorted recursively and no whitespace.
pub fn canonical_json(v: &Value) -> String {
    fn sorted(v: &Value) -> Value {
        match v {
            Value::Object(o) => {
                let m: BTreeMap<&String, Value> = o.iter().map(|(k, v)| (k, sorted(v))).collect();
                Value::Object(m.into_iter().map(|(k, v)| (k.clone(), v)).collect())
            }
            Value::Array(a) => Value::Array(a.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    serde_json::to_string(&sorted(v)).expect("serializable")
}

pub fn sha256_hex(bytes: &[u8]) -> Sha256Hex {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// `sha256(canonical_json(normalize_params(params)))`: the exact "same thing" key.
pub fn signature(params: &BTreeMap<String, Value>) -> Sha256Hex {
    let normalized = normalize_params(params);
    let v = Value::Object(normalized.into_iter().collect());
    sha256_hex(canonical_json(&v).as_bytes())
}

fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// MinHash signature of the distinct `tokens` with `k` seeded hash functions. An empty
/// token set yields all `u64::MAX` (similar to nothing).
pub fn minhash<S: AsRef<str>>(tokens: &[S], k: usize) -> Vec<u64> {
    let base: BTreeSet<u64> = tokens.iter().map(|t| fnv1a(t.as_ref())).collect();
    (0..k as u64)
        .map(|i| {
            let seed = splitmix64(i.wrapping_add(0x5eed));
            base.iter()
                .map(|h| splitmix64(h ^ seed))
                .min()
                .unwrap_or(u64::MAX)
        })
        .collect()
}

/// Fraction of agreeing positions (estimates Jaccard). 0.0 for mismatched lengths or an
/// empty side.
pub fn minhash_similarity(a: &[u64], b: &[u64]) -> f64 {
    if a.is_empty()
        || a.len() != b.len()
        || a.iter().all(|x| *x == u64::MAX)
        || b.iter().all(|x| *x == u64::MAX)
    {
        return 0.0;
    }
    a.iter().zip(b).filter(|(x, y)| x == y).count() as f64 / a.len() as f64
}

/// Normalized token set used for MinHash: domain path segments, stems of the claim, and
/// `key=value` tokens of normalized params and facets.
pub fn outcome_tokens(o: &Outcome) -> Vec<String> {
    let mut set = BTreeSet::new();
    set.extend(tokenize(&o.subject.domain));
    set.extend(stems_of_text(&o.claim));
    for (k, v) in normalize_params(&o.params) {
        let v = match v {
            Value::String(s) => s,
            other => other.to_string(),
        };
        set.insert(format!("{k}={v}"));
    }
    for f in &o.facets {
        set.insert(format!(
            "{}={}",
            normalize_key(&f.key),
            f.value.trim().to_lowercase()
        ));
    }
    set.into_iter().collect()
}

/// Host name: `$OMP_EVOLVE_MACHINE` when set (tests, renamed hosts), else `gethostname`.
pub fn machine_name() -> String {
    if let Ok(m) = std::env::var("OMP_EVOLVE_MACHINE") {
        if !m.trim().is_empty() {
            return m.trim().to_string();
        }
    }
    let mut buf = [0u8; 256];
    // SAFETY: buf is valid for buf.len() bytes; gethostname NUL-terminates on success
    // (we also bound the scan by the buffer length).
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc == 0 {
        let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
        let name = String::from_utf8_lossy(&buf[..end]).trim().to_string();
        if !name.is_empty() {
            return name;
        }
    }
    "unknown".to_string()
}

/// Caller-provided part of an outcome; [`new_outcome`] fills id, ts, machine, signature.
#[derive(Debug, Clone)]
pub struct NewOutcome {
    pub session_id: String,
    pub model_family: Family,
    pub repo: String,
    pub branch: String,
    pub kind: OutcomeKind,
    pub subject: Subject,
    pub claim: String,
    pub params: BTreeMap<String, Value>,
    pub facets: Vec<Facet>,
    pub evidence: Vec<Evidence>,
    pub result: OutcomeResult,
    pub links: Vec<Link>,
    pub provenance: Provenance,
}

/// Builds an outcome now on this machine: uuid v7 id, current unix ms, host name,
/// normalized params and their signature.
pub fn new_outcome(n: NewOutcome) -> Outcome {
    let ts = chrono::Utc::now().timestamp_millis();
    new_outcome_at(n, uuid::Uuid::now_v7().to_string(), ts, machine_name())
}

/// Deterministic variant (importer, tests): caller supplies id, ts and machine.
pub fn new_outcome_at(n: NewOutcome, id: String, ts: UnixMs, machine: String) -> Outcome {
    let params = normalize_params(&n.params);
    let signature = signature(&params);
    Outcome {
        id,
        session_id: n.session_id,
        ts,
        machine,
        model_family: n.model_family,
        repo: n.repo,
        branch: n.branch,
        kind: n.kind,
        subject: n.subject,
        claim: n.claim,
        signature,
        params,
        facets: n.facets,
        evidence: n.evidence,
        result: n.result,
        links: n.links,
        provenance: n.provenance,
    }
}

/// Marks fields as filled from prose by the reflector (deduplicated, sorted). A
/// self-reported field is evidence of what the agent believed, not of what happened.
pub fn mark_self_reported(o: &mut Outcome, fields: &[&str]) {
    let mut set: BTreeSet<String> = o.provenance.self_reported.drain(..).collect();
    set.extend(fields.iter().map(|f| f.to_string()));
    o.provenance.self_reported = set.into_iter().collect();
}

pub fn is_self_reported(o: &Outcome, field: &str) -> bool {
    o.provenance.self_reported.iter().any(|f| f == field)
}

/// Whether the outcome may count toward promoting an entry: its result is not
/// self-reported and it carries machine evidence (test, receipt or metric).
pub fn has_machine_evidence(o: &Outcome) -> bool {
    !is_self_reported(o, "result")
        && o.evidence.iter().any(|e| {
            matches!(
                e.kind,
                EvidenceKind::Test | EvidenceKind::Receipt | EvidenceKind::Metric
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ResultStatus;
    use serde_json::json;

    fn params(v: Value) -> BTreeMap<String, Value> {
        serde_json::from_value(v).unwrap()
    }

    fn sample() -> NewOutcome {
        NewOutcome {
            session_id: "s".into(),
            model_family: Family("anthropic/claude-opus".into()),
            repo: "/r".into(),
            branch: "r".into(),
            kind: OutcomeKind::Hypothesis,
            subject: Subject {
                domain: "quant.options".into(),
                entities: vec![],
            },
            claim: "Overlap depth predicts spreads".into(),
            params: params(json!({"Universe ": "SPY", "horizon": 5.0})),
            facets: vec![Facet {
                key: "mechanism".into(),
                value: "Overlap".into(),
            }],
            evidence: vec![],
            result: OutcomeResult {
                status: ResultStatus::Inconclusive,
                metrics: BTreeMap::new(),
                kill_criterion: None,
            },
            links: vec![],
            provenance: Provenance::default(),
        }
    }

    #[test]
    fn normalization_makes_equivalent_params_equal() {
        let a = params(
            json!({"Data Snapshot": "  2026-10  ", "horizon": 5.0, "x": null, "nested": {"B": 1, "a-b": "Q"}}),
        );
        let b = params(
            json!({"data_snapshot": "2026-10", "horizon": 5, "nested": {"a_b": "q", "b": 1.0}}),
        );
        assert_eq!(normalize_params(&a), normalize_params(&b));
        assert_eq!(signature(&a), signature(&b));
        let c = params(json!({"data_snapshot": "2026-11", "horizon": 5}));
        assert_ne!(signature(&a), signature(&c));
        // Array order is significant; non-integral floats survive.
        assert_ne!(
            signature(&params(json!({"u": ["a", "b"]}))),
            signature(&params(json!({"u": ["b", "a"]})))
        );
        assert_eq!(
            normalize_params(&params(json!({"r": 0.25})))["r"],
            json!(0.25)
        );
        assert_eq!(signature(&a).len(), 64);
    }

    #[test]
    fn canonical_json_sorts_nested_keys() {
        assert_eq!(
            canonical_json(&json!({"b": {"d": 1, "c": [ {"z":1,"y":2} ]}, "a": 0})),
            r#"{"a":0,"b":{"c":[{"y":2,"z":1}],"d":1}}"#
        );
    }

    #[test]
    fn minhash_estimates_overlap() {
        let a: Vec<String> = (0..100).map(|i| format!("t{i}")).collect();
        let b: Vec<String> = (0..100).map(|i| format!("t{}", i + 50)).collect(); // J = 50/150
        let (ma, mb) = (minhash(&a, MINHASH_K), minhash(&b, MINHASH_K));
        assert_eq!(ma.len(), MINHASH_K);
        assert_eq!(minhash_similarity(&ma, &ma), 1.0);
        let s = minhash_similarity(&ma, &mb);
        assert!((0.1..0.6).contains(&s), "estimate {s} near 0.33");
        let disjoint: Vec<String> = (0..100).map(|i| format!("u{i}")).collect();
        assert!(minhash_similarity(&ma, &minhash(&disjoint, MINHASH_K)) < 0.1);
        // Duplicates and order do not matter; empty sets are similar to nothing.
        let mut rev = a.clone();
        rev.reverse();
        rev.push("t1".into());
        assert_eq!(minhash(&rev, MINHASH_K), ma);
        let empty = minhash::<String>(&[], MINHASH_K);
        assert_eq!(minhash_similarity(&empty, &empty), 0.0);
        assert_eq!(minhash_similarity(&ma, &ma[..10]), 0.0);
    }

    #[test]
    fn builder_fills_identity_and_signature() {
        let o = new_outcome_at(sample(), "id1".into(), 42, "box".into());
        assert_eq!(
            (o.id.as_str(), o.ts, o.machine.as_str()),
            ("id1", 42, "box")
        );
        assert_eq!(o.params["universe"], json!("spy"));
        assert_eq!(o.params["horizon"], json!(5));
        assert_eq!(
            o.signature,
            signature(&params(json!({"universe": "SPY", "horizon": 5})))
        );
        let live = new_outcome(sample());
        assert!(live.ts > 1_700_000_000_000 && !live.machine.is_empty() && live.id.len() == 36);
        let toks = outcome_tokens(&o);
        assert!(toks.contains(&"universe=spy".to_string()));
        assert!(toks.contains(&"mechanism=overlap".to_string()));
        assert!(toks.contains(&"quant".to_string()) && toks.contains(&"predict".to_string()));
    }

    #[test]
    fn self_reported_blocks_promotion() {
        let mut o = new_outcome_at(sample(), "id".into(), 0, "m".into());
        assert!(!has_machine_evidence(&o), "no evidence");
        o.evidence.push(Evidence {
            kind: EvidenceKind::Redirect,
            reference: "s:1".into(),
            verdict: "x".into(),
        });
        assert!(
            !has_machine_evidence(&o),
            "redirects are not machine evidence"
        );
        o.evidence.push(Evidence {
            kind: EvidenceKind::Test,
            reference: "s:2".into(),
            verdict: "pass".into(),
        });
        assert!(has_machine_evidence(&o));
        mark_self_reported(&mut o, &["result", "claim"]);
        mark_self_reported(&mut o, &["claim"]);
        assert_eq!(
            o.provenance.self_reported,
            vec!["claim".to_string(), "result".to_string()]
        );
        assert!(is_self_reported(&o, "result") && !has_machine_evidence(&o));
    }
}
