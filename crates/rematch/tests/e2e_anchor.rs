//! End-to-end anchor replay (SPEC "Rollback and anchors"): before an entry replaces a kept
//! one, the kept entry's originating task is replayed once in batch mode (fake omp, a
//! verifier that passes or fails via `$ANCHOR_VERDICT`). All three call sites: `count`
//! keeping a sibling by its counters, the batch next-run rule keeping a pending entry that
//! takes a kept entry's slot, and `transfer promote` keeping a shared copy in a branch-kept
//! slot. Plus the manual `anchor replay <entry>` and the busy-task deferral.

mod common;

use std::path::Path;
use std::process::Command;

use common::*;
use rematch_core::family::family_of;
use rematch_core::ledger::{CounterRow, EntryFilter, Ledger};
use rematch_core::library::{set_status, stage};
use rematch_core::paths::RematchPaths;
use rematch_core::predicate::derive;
use rematch_core::types::{Certificate, Edit, Entry, Status, Verdict};
use serde_json::{json, Value};

const NAME: &str = "run-tests-first";
const ANCHOR_TASK: &str = "anchor-task";

/// Passes only when `$ANCHOR_VERDICT` is `pass`, the replacing entry's SKILL.md is the one
/// in the rollout's skills dir and the kept entry's extra file is absent (kept excluded).
const VERIFIER: &str = "test \"$ANCHOR_VERDICT\" = pass && grep -q sibling-marker ../skills/run-tests-first/SKILL.md && test ! -e ../skills/run-tests-first/units/old.md";

fn sh(dir: &Path, cmd: &str) {
    let st = Command::new("sh").arg("-c").arg(cmd).current_dir(dir).status().unwrap();
    assert!(st.success(), "{cmd}");
}

struct Fixture {
    env: Env,
    paths: RematchPaths,
    kept: String,
    sibling: String,
    omp_log: std::path::PathBuf,
}

impl Fixture {
    fn ledger(&self) -> Ledger {
        Ledger::open(&self.paths.ledger_file).unwrap()
    }

    fn get(&self, id: &str) -> Entry {
        self.ledger().get_entry(id).unwrap().unwrap()
    }

    fn cmd(&self, args: &[&str], verdict: &str) -> Command {
        let mut c = self.env.cmd(args);
        c.env("OMP_BIN", scripts().join("fake-omp-batch.sh"))
            .env("FAKE_OMP_LOG", &self.omp_log)
            .env("ANCHOR_VERDICT", verdict);
        c
    }

    fn run(&self, args: &[&str], verdict: &str) -> Value {
        let (v, code) = self.env.run_cmd(self.cmd(args, verdict));
        assert_eq!(code, 0, "{args:?}: {v}");
        assert_eq!(v["ok"], true, "{args:?}: {v}");
        v["data"].clone()
    }

    fn rollouts(&self) -> usize {
        std::fs::read_to_string(&self.omp_log).map(|s| s.lines().count()).unwrap_or(0)
    }

    fn certificates(&self, id: &str) -> Vec<Certificate> {
        let l = self.ledger();
        l.list_receipts(Some(id))
            .unwrap()
            .into_iter()
            .map(|r| serde_json::from_slice(&l.get_blob(&r.receipt.certificate).unwrap().unwrap()).unwrap())
            .collect()
    }

    fn worktrees(&self) -> usize {
        let out = Command::new("git").arg("-C").arg(&self.env.repo).args(["worktree", "list"]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).lines().count()
    }
}

fn skill(body_marker: &str) -> Vec<Edit> {
    vec![Edit::Write {
        path: "SKILL.md".into(),
        content: format!(
            "---\nname: {NAME}\ndescription: Use when committing in the payments crate.\n---\n\n# {NAME}\n\n## Laws\n- MUST run the payments tests before committing ({body_marker}).\n"
        ),
    }]
}

fn entry(env: &Env, origin: Value, extra: Value) -> Entry {
    let repo = env.repo.to_string_lossy().to_string();
    let mut v = json!({
        "id": uuid_like(),
        "kind": "skill",
        "name": NAME,
        "branch": "payments",
        "scope": "personal",
        "status": "provisional",
        "family": family_of("anthropic", "claude-opus-5-5"),
        "origin": origin,
        "predicate": derive(Some(&repo), &["rs".to_string()], &[], None),
        "trigger_line": "Use when committing in the payments crate.",
        "version": 1,
        "path": "",
        "created_at": 1,
        "updated_at": 1,
    });
    if let (Value::Object(m), Value::Object(x)) = (&mut v, extra) {
        m.extend(x);
    }
    serde_json::from_value(v).unwrap()
}

fn uuid_like() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(1);
    format!("0199a000-0000-7000-8000-{:012}", N.fetch_add(1, Ordering::SeqCst))
}

/// A real git repo, the anchor task (verifier optional), a kept entry originating from it
/// (with an extra `units/old.md`) and a provisional sibling (`sibling_of` = kept) carrying
/// `sibling-marker`; `sibling_of` is omitted (same slot instead) when `as_sibling` is false.
fn fixture(verifier: Option<&str>, as_sibling: bool) -> Fixture {
    let env = Env::new();
    std::fs::remove_dir_all(env.repo.join(".git")).unwrap();
    sh(
        &env.repo,
        "git init -q . && git config user.email t@e.st && git config user.name tester && \
         printf 'echo build\\n' > build.sh && git add . && git commit -qm init",
    );
    let repo = env.repo.to_string_lossy().to_string();
    let mut add = vec!["task", "add", ANCHOR_TASK, "--repo", repo.as_str(), "--prompt", "Make the check pass", "--max-time", "30s"];
    if let Some(v) = verifier {
        add.extend(["--verifier", v]);
    }
    env.run(&add);

    let paths = RematchPaths::with_roots(&env.root, &env.agent);
    let ledger = Ledger::open(&paths.ledger_file).unwrap();
    let k = entry(&env, json!({"kind": "task", "task_id": ANCHOR_TASK}), json!({}));
    let mut files = skill("kept");
    files.push(Edit::Write { path: "units/old.md".into(), content: "old unit\n".into() });
    let k = stage(&ledger, &paths, &k, &files).unwrap();
    let k = set_status(&ledger, &paths, &k.id, Status::Kept, None).unwrap();
    assert!(k.path.starts_with(paths.skills_dir()));

    let extra = if as_sibling { json!({"sibling_of": k.id, "version": 2}) } else { json!({"version": 2}) };
    let s = entry(&env, json!({"kind": "session", "session_id": "0199a000-0000-7000-8000-00000000aaaa"}), extra);
    let s = stage(&ledger, &paths, &s, &skill("sibling-marker")).unwrap();
    let omp_log = env.root.join("fake-omp.log");
    Fixture { kept: k.id, sibling: s.id, env, paths, omp_log }
}

/// Four clean matched sessions with the sibling loaded ⇒ counters decide Keep.
fn count_four(f: &Fixture, verdict: &str) -> Value {
    for n in 1..=4 {
        f.env.write_trace(&format!("0199b000-0000-7000-8000-00000000000{n}"), &[&f.sibling], false);
    }
    let c = f.run(&["count", "--editor", &script("editor-ok.sh")], verdict);
    assert_eq!(c["folded"].as_array().unwrap().len(), 4, "{c}");
    c["decisions"].as_array().unwrap().iter().find(|d| d["entry"] == f.sibling.as_str()).cloned().expect("sibling decided")
}

#[test]
fn anchor_count_keep_replays_once_and_passes() {
    let f = fixture(Some(VERIFIER), true);
    let d = count_four(&f, "pass");
    assert_eq!(d["decision"], "keep", "{d}");
    assert_eq!(d["anchor"]["kept"], f.kept.as_str(), "{d}");
    assert_eq!(d["anchor"]["replay"]["outcome"], "replayed", "{d}");
    assert_eq!(d["anchor"]["replay"]["pass"], true, "{d}");
    assert_eq!(d["anchor"]["replay"]["receipt"]["task"], ANCHOR_TASK);
    assert_eq!(f.rollouts(), 1, "exactly one anchor rollout");

    let s = f.get(&f.sibling);
    assert_eq!(s.status, Status::Kept);
    assert!(s.path.starts_with(f.paths.skills_dir()));
    assert_eq!(s.predecessor.as_deref(), Some(f.kept.as_str()));
    assert_eq!(f.get(&f.kept).status, Status::Dormant, "superseded, saved for rollback");
    let certs = f.certificates(&f.sibling);
    assert_eq!(certs.len(), 1, "{certs:?}");
    assert!(matches!(certs[0].verifier, Some(Verdict::Pass { .. })), "{certs:?}");
    assert_eq!(certs[0].canary.as_ref().map(|p| p.matched), Some(4));
    // The replay record is in the blob store; the workspace and worktree are gone.
    let blob = d["anchor"]["replay"]["receipt"]["blob"].as_str().unwrap();
    assert!(f.ledger().get_blob(blob).unwrap().is_some());
    assert!(!f.env.root.join("runs").join(ANCHOR_TASK).join("1").exists());
    assert_eq!(f.worktrees(), 1);
}

#[test]
fn anchor_count_keep_failing_replay_retires_sibling() {
    let f = fixture(Some(VERIFIER), true);
    let d = count_four(&f, "fail");
    assert_eq!(d["decision"], "retire", "{d}");
    assert_eq!(d["anchor"]["replay"]["pass"], false, "{d}");
    assert_eq!(f.rollouts(), 1);

    let s = f.get(&f.sibling);
    assert_eq!(s.status, Status::Retired);
    let note = s.status_note.unwrap_or_default();
    assert!(note.starts_with("anchor replay failed: task anchor-task"), "{note}");
    let k = f.get(&f.kept);
    assert_eq!(k.status, Status::Kept, "the kept entry stays");
    assert!(k.path.join("units/old.md").is_file());
    assert!(f.certificates(&f.sibling).is_empty(), "no receipt for a failed anchor");
    assert_eq!(f.worktrees(), 1);
}

#[test]
fn anchor_count_keep_without_verifier_uses_canary() {
    let f = fixture(None, true);
    let d = count_four(&f, "fail");
    assert_eq!(d["decision"], "keep", "{d}");
    assert_eq!(d["anchor"]["replay"]["outcome"], "no_verifier", "{d}");
    assert_eq!(f.rollouts(), 0, "no rollout without a verifier");
    assert_eq!(f.get(&f.sibling).status, Status::Kept);
    assert_eq!(f.get(&f.kept).status, Status::Dormant);
}

#[test]
fn anchor_count_keep_deferred_while_task_locked() {
    let f = fixture(Some(VERIFIER), true);
    // A live process (this test) holds the anchor task's run lock.
    let runs = f.env.root.join("runs").join(ANCHOR_TASK);
    std::fs::create_dir_all(&runs).unwrap();
    let lock = runs.join(".lock");
    std::fs::write(&lock, format!("{}\n", std::process::id())).unwrap();

    let d = count_four(&f, "pass");
    assert_eq!(d["decision"], "keep", "{d}");
    let err = d["error"].as_str().unwrap_or_default();
    assert!(err.contains("another run of task anchor-task is in progress; anchor replay deferred"), "{d}");
    assert_eq!(f.rollouts(), 0, "no rollout while the task is locked");
    let s = f.get(&f.sibling);
    assert_eq!(s.status, Status::Provisional, "the keep is deferred");
    assert!(s.status_note.is_none(), "{:?}", s.status_note);
    assert_eq!(f.get(&f.kept).status, Status::Kept);
    let retired = f.ledger().list_entries(&EntryFilter { status: Some(Status::Retired), ..Default::default() }).unwrap();
    assert!(retired.is_empty(), "nothing retired: {retired:?}");
    assert!(f.certificates(&f.sibling).is_empty());
    assert!(lock.is_file(), "the holder's lock is untouched");
    assert!(!runs.join("1").exists(), "no workspace materialized");
    assert_eq!(f.worktrees(), 1, "no worktree left");

    // Lock released: the next matched session decides again and the replay runs.
    std::fs::remove_file(&lock).unwrap();
    f.env.write_trace("0199b000-0000-7000-8000-000000000005", &[&f.sibling], false);
    let c = f.run(&["count", "--editor", &script("editor-ok.sh")], "pass");
    let d = c["decisions"].as_array().unwrap().iter().find(|d| d["entry"] == f.sibling.as_str()).cloned().expect("decided");
    assert_eq!(d["anchor"]["replay"]["pass"], true, "{d}");
    assert_eq!(f.rollouts(), 1);
    assert_eq!(f.get(&f.sibling).status, Status::Kept);
    assert_eq!(f.get(&f.kept).status, Status::Dormant);
}

/// `transfer promote` setup: the kept entry (branch `payments`) plus a clean canary of the
/// same name in branch `ops` ⇒ a `shared` copy that takes the kept entry's slot, gated by
/// the kept entry's task replay.
fn promote_fixture() -> Fixture {
    let f = fixture(Some("test \"$ANCHOR_VERDICT\" = pass"), false);
    let l = f.ledger();
    let ops = entry(&f.env, json!({"kind": "session", "session_id": "0199a000-0000-7000-8000-00000000bbbb"}), json!({"branch": "ops"}));
    let ops = stage(&l, &f.paths, &ops, &skill("ops")).unwrap();
    for i in 0..4 {
        l.add_counter(&CounterRow {
            entry_id: ops.id.clone(),
            session_id: format!("0199c000-0000-7000-8000-00000000000{i}"),
            family: family_of("anthropic", "claude-opus-5-5"),
            matched: 1,
            recurrence: 0,
            harm: 0,
            tokens: 0,
            turns: 0,
            ts: 1_000 + i,
        })
        .unwrap();
    }
    f
}

/// One `transfer promote` pass; its single promotion row (of the kept entry).
fn promote_row(f: &Fixture, verdict: &str) -> Value {
    let out = f.run(&["transfer", "promote"], verdict);
    let rows = out["promotions"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{out}");
    let row = rows[0].clone();
    assert_eq!(row["entry"], f.kept.as_str(), "{row}");
    assert_eq!(row["anchor"]["kept"], f.kept.as_str(), "{row}");
    assert_eq!(f.worktrees(), 1);
    row
}

fn promote(verdict: &str) -> (Fixture, Value) {
    let f = promote_fixture();
    let row = promote_row(&f, verdict);
    assert_eq!(f.rollouts(), 1, "one anchor rollout: {row}");
    (f, row)
}

#[test]
fn anchor_promote_shared_passing_replay_keeps_and_certifies() {
    let (f, row) = promote("pass");
    assert_eq!(row["anchor"]["replay"]["pass"], true, "{row}");
    assert!(row["blocked"].is_null(), "{row}");
    let shared = f.get(row["shared_id"].as_str().unwrap());
    assert_eq!(shared.status, Status::Kept);
    assert_eq!(shared.branch, "shared");
    assert_eq!(shared.predecessor.as_deref(), Some(f.kept.as_str()));
    assert_eq!(f.get(&f.kept).status, Status::Dormant);
    let certs = f.certificates(&shared.id);
    assert_eq!(certs.len(), 1, "{certs:?}");
    assert!(matches!(certs[0].verifier, Some(Verdict::Pass { .. })), "{certs:?}");
    assert!(certs[0].canary.is_none(), "a promotion has no new canary");
    assert_eq!(row["anchor"]["certificate"].as_str().map(str::len), Some(64), "{row}");
}

#[test]
fn anchor_promote_shared_failing_replay_retires_copy_and_settles() {
    let (f, row) = promote("fail");
    assert_eq!(row["anchor"]["replay"]["pass"], false, "{row}");
    assert!(row["shared_id"].is_null(), "{row}");
    let blocked = row["blocked"].as_str().unwrap();
    assert!(blocked.starts_with("anchor replay failed: task anchor-task"), "{blocked}");
    let copy = f.get(row["anchor"]["retired"].as_str().unwrap());
    assert_eq!(copy.status, Status::Retired);
    assert_eq!(copy.branch, "shared");
    assert_eq!(copy.status_note.as_deref(), Some(blocked));
    assert!(f.certificates(&copy.id).is_empty());
    let k = f.get(&f.kept);
    assert_eq!(k.status, Status::Kept, "the kept entry stays");
    assert!(k.path.join("units/old.md").is_file());

    // A failed anchor settles the promotion: no new copy, no second replay.
    let out = f.run(&["transfer", "promote"], "pass");
    assert_eq!(out["promotions"], json!([]), "{out}");
    assert_eq!(f.rollouts(), 1);
}

#[test]
fn anchor_promote_shared_deferred_while_task_locked_then_retried() {
    let f = promote_fixture();
    let runs = f.env.root.join("runs").join(ANCHOR_TASK);
    std::fs::create_dir_all(&runs).unwrap();
    let lock = runs.join(".lock");
    std::fs::write(&lock, format!("{}\n", std::process::id())).unwrap();

    let row = promote_row(&f, "pass");
    assert_eq!(f.rollouts(), 0, "no rollout while the task is locked");
    let blocked = row["blocked"].as_str().unwrap();
    assert!(blocked.starts_with("anchor replay deferred: another run of task anchor-task"), "{blocked}");
    assert!(row["shared_id"].is_null(), "{row}");
    assert_eq!(f.get(row["anchor"]["retired"].as_str().unwrap()).status, Status::Retired);
    assert_eq!(f.get(&f.kept).status, Status::Kept, "the kept entry stays");
    assert!(!runs.join("1").exists(), "no workspace materialized");

    // Lock released: the next pass stages a fresh copy and replays.
    std::fs::remove_file(&lock).unwrap();
    let row = promote_row(&f, "pass");
    assert_eq!(row["anchor"]["replay"]["pass"], true, "{row}");
    assert_eq!(f.rollouts(), 1);
    assert_eq!(f.get(row["shared_id"].as_str().unwrap()).status, Status::Kept);
    assert_eq!(f.get(&f.kept).status, Status::Dormant);
}

#[test]
fn anchor_manual_replay_reports_and_stages_nothing() {
    let f = fixture(Some(VERIFIER), true);
    let out = f.run(&["anchor", "replay", &f.sibling], "fail");
    assert_eq!(out["kept"], f.kept.as_str(), "{out}");
    assert_eq!(out["anchor"]["outcome"], "replayed", "{out}");
    assert_eq!(out["anchor"]["pass"], false, "{out}");
    assert_eq!(out["staged"], false);
    assert_eq!(f.rollouts(), 1);
    assert_eq!(f.get(&f.sibling).status, Status::Provisional);
    assert_eq!(f.get(&f.kept).status, Status::Kept);
    assert!(f.certificates(&f.sibling).is_empty());

    let out = f.run(&["anchor", "replay", &f.sibling], "pass");
    assert_eq!(out["anchor"]["pass"], true, "{out}");
    assert_eq!(f.get(&f.sibling).status, Status::Provisional, "a passing manual replay keeps nothing");
    assert_eq!(f.worktrees(), 1);

    // The kept entry itself replaces nothing.
    let (v, code) = f.env.run_cmd(f.cmd(&["anchor", "replay", &f.kept], "pass"));
    assert_eq!(code, 1, "{v}");
    assert!(v["error"].as_str().unwrap().contains("replaces no kept entry"), "{v}");
}

/// Batch next-run rule: a pending entry of task `demo` that takes the kept entry's slot
/// (same name, no `sibling_of`) passes `demo`'s next run; the kept entry's own task is
/// replayed first.
fn next_run(verdict: &str) -> (Fixture, Value) {
    let f = fixture(Some(VERIFIER), false);
    let repo = f.env.repo.to_string_lossy().to_string();
    f.env.run(&["task", "add", "demo", "--repo", &repo, "--prompt", "Make the check pass", "--verifier", "true", "--max-time", "30s"]);
    {
        let l = f.ledger();
        let mut s = l.get_entry(&f.sibling).unwrap().unwrap();
        s.origin = rematch_core::types::Origin::Task { task_id: "demo".into() };
        s.pending_next_run = true;
        l.upsert_entry(&s).unwrap();
    }
    let job = json!({
        "id": "job-anchor", "kind": "run", "status": "running",
        "task": "demo", "verifier": "true", "width": 1, "seed": 3,
        "editor": format!("sh {}", script("batch-editor.sh")),
        "omp_bin": scripts().join("fake-omp-batch.sh"),
    });
    std::fs::create_dir_all(f.env.root.join("jobs")).unwrap();
    std::fs::write(f.env.root.join("jobs/job-anchor.json"), job.to_string()).unwrap();
    let out = f.run(&["run-worker", "--job", "job-anchor"], verdict);
    (f, out)
}

#[test]
fn anchor_batch_next_run_passing_replay_keeps_and_certifies() {
    let (f, out) = next_run("pass");
    let row = &out["next_run"][0];
    assert_eq!(row["status"], "kept", "{out}");
    assert_eq!(row["anchor"]["kept"], f.kept.as_str(), "{out}");
    assert_eq!(row["anchor"]["replay"]["pass"], true, "{out}");
    assert_eq!(f.rollouts(), 2, "reference + one anchor replay");
    assert_eq!(f.get(&f.sibling).status, Status::Kept);
    assert_eq!(f.get(&f.kept).status, Status::Dormant);
    let certs = f.certificates(&f.sibling);
    assert_eq!(certs.len(), 2, "next-run receipt + anchor receipt: {certs:?}");
    assert!(certs.iter().all(|c| matches!(c.verifier, Some(Verdict::Pass { .. }))));
    assert_eq!(f.worktrees(), 1);
}

#[test]
fn anchor_batch_next_run_failing_replay_retires_pending_entry() {
    let (f, out) = next_run("fail");
    let row = &out["next_run"][0];
    assert_eq!(row["status"], "retired", "{out}");
    assert_eq!(row["anchor"]["replay"]["pass"], false, "{out}");
    let s = f.get(&f.sibling);
    assert_eq!(s.status, Status::Retired);
    assert!(s.status_note.unwrap_or_default().starts_with("anchor replay failed: task anchor-task"));
    assert!(!s.pending_next_run);
    let k = f.get(&f.kept);
    assert_eq!(k.status, Status::Kept, "the kept entry stays");
    assert!(k.path.join("units/old.md").is_file());
    assert_eq!(f.worktrees(), 1);
}
