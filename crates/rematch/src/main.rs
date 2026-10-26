//! `rematch`: clap dispatch → exactly one JSON envelope on stdout (DESIGN §2).
//! Exit codes: 0 ok, 1 error, 2 usage, 130 cancelled. Logs go to stderr.

pub mod app;
pub mod batch;
pub mod builtins;
pub mod config;
pub mod create;
pub mod digest;
pub mod doctor;
pub mod editor;
pub mod envelope;
pub mod import_cmd;
pub mod improve;
pub mod jobs;
pub mod library_cmd;
pub mod omp;
pub mod proc;
pub mod reflect;
pub mod retrieve;
pub mod scope;
pub mod skills_cmd;
pub mod trace_event;
pub mod uninstall;

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use serde_json::{json, Value};

use app::AppCtx;
use envelope::{Envelope, EXIT_OK, EXIT_USAGE};

#[derive(Parser, Debug)]
#[command(name = "rematch", version, about = "rematch: trace-grounded harness evolution for omp, Claude Code and Codex")]
struct Cli {
    /// Human-readable output instead of the JSON envelope.
    #[arg(long, global = true)]
    text: bool,
    /// Directory the command runs against (repo/cwd for match, digest, create, …).
    #[arg(long, global = true, value_name = "DIR")]
    cwd: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// omp binary + version, roles, extension link, ledger schema, roots, off state.
    Doctor,
    /// Import omp session history and/or a compound-forge checkout.
    Import(import_cmd::ImportArgs),
    /// Count finished traces, then reflect on pending sessions (idempotent).
    Reflect(reflect::ReflectArgs),
    /// Staging dirs, entries and hooks that load for a directory.
    Match(digest::MatchArgs),
    /// The shared skill directory every harness loads (REMATCH_ROOT/skills): sync.
    Skills {
        #[command(subcommand)]
        cmd: skills_cmd::SkillsCmd,
    },
    /// Units of the loaded skill packages that match a query (the `retrieve` context hook).
    Retrieve(retrieve::RetrieveArgs),
    /// Session-start digest: new statuses, approvals, review list, transfers.
    Digest,
    /// Append an event for a session (marks); the trace file stays the primary input.
    Record(reflect::RecordArgs),
    /// Harness hook payload on stdin → DESIGN §3 trace records (Claude Code adapter).
    TraceEvent(trace_event::TraceEventArgs),
    /// Harness `PreToolUse` payload on stdin → approved block/stuck hooks verdict (Claude Code adapter).
    HookCheck(trace_event::TraceEventArgs),
    /// Greenfield: create an entry from sessions, a task, or a description (draft).
    Create(create::CreateArgs),
    /// Brownfield: register, audit, split or propose a sibling for an existing skill.
    Improve(improve::ImproveArgs),
    /// Draft → provisional.
    Activate(create::ActivateArgs),
    /// Batch run with a verifier (detached; returns a job id).
    Run(batch::run::RunArgs),
    /// Internal: detached batch worker.
    #[command(hide = true)]
    RunWorker(batch::worker::RunWorkerArgs),
    /// Job status (or all jobs + entry counts).
    Status { job: Option<String> },
    /// Library: ls, why, disable, rollback, pin, unpin, approve, decline, set-fork-path, export, import-export.
    Library {
        #[command(subcommand)]
        cmd: library_cmd::LibraryCmd,
    },
    /// Batch task definitions.
    Task(batch::tasks::TaskArgs),
    /// Cross-branch transfer of kept invariant lessons.
    Transfer(scope::transfer::TransferArgs),
    /// Open a team/fork PR for entries.
    Ship(scope::ship::ShipArgs),
    /// Evidence drift: mark entries stale (or re-canary one).
    Stale(scope::stale::StaleArgs),
    /// Per-family split of entries whose counters diverge.
    Split(scope::split::SplitArgs),
    /// Anchor replay of a kept entry's originating task with its sibling in place (manual: reports, stages nothing).
    Anchor(scope::anchor::AnchorArgs),
    /// Built-in entries (context unit retrieval, tool-output truncation, stuck detection).
    Builtins {
        #[command(subcommand)]
        cmd: builtins::BuiltinsCmd,
    },
    /// Fold finished traces into counters and decide.
    Count(reflect::CountArgs),
    /// Override the redirect class of a session's last turn.
    Mark(reflect::MarkArgs),
    /// Kill switch on (writes the `off` marker).
    Off,
    /// Kill switch off (removes the `off` marker).
    On,
    /// Remove every file rematch wrote (plan only without --yes).
    Uninstall(uninstall::UninstallArgs),
}

impl Cmd {
    fn name(&self) -> &'static str {
        match self {
            Cmd::Doctor => "doctor",
            Cmd::Import(_) => "import",
            Cmd::Reflect(_) => "reflect",
            Cmd::Match(_) => "match",
            Cmd::Skills { .. } => "skills",
            Cmd::Digest => "digest",
            Cmd::Retrieve(_) => "retrieve",
            Cmd::Record(_) => "record",
            Cmd::TraceEvent(_) => "trace-event",
            Cmd::HookCheck(_) => "hook-check",
            Cmd::Create(_) => "create",
            Cmd::Improve(_) => "improve",
            Cmd::Activate(_) => "activate",
            Cmd::Run(_) => "run",
            Cmd::RunWorker(_) => "run-worker",
            Cmd::Status { .. } => "status",
            Cmd::Library { .. } => "library",
            Cmd::Task(_) => "task",
            Cmd::Transfer(_) => "transfer",
            Cmd::Ship(_) => "ship",
            Cmd::Stale(_) => "stale",
            Cmd::Split(_) => "split",
            Cmd::Anchor(_) => "anchor",
            Cmd::Builtins { .. } => "builtins",
            Cmd::Count(_) => "count",
            Cmd::Mark(_) => "mark",
            Cmd::Off => "off",
            Cmd::On => "on",
            Cmd::Uninstall(_) => "uninstall",
        }
    }
}

fn dispatch(ctx: &AppCtx, cmd: Cmd) -> anyhow::Result<Value> {
    match cmd {
        Cmd::Doctor => doctor::cmd_doctor(ctx),
        Cmd::Import(a) => import_cmd::cmd_import(ctx, a),
        Cmd::Reflect(a) => reflect::cmd_reflect(ctx, a),
        Cmd::Match(a) => digest::cmd_match(ctx, a),
        Cmd::Skills { cmd } => skills_cmd::cmd_skills(ctx, cmd),
        Cmd::Digest => digest::cmd_digest(ctx),
        Cmd::Retrieve(a) => retrieve::cmd_retrieve(ctx, a),
        Cmd::Record(a) => reflect::cmd_record(ctx, a),
        Cmd::TraceEvent(a) => trace_event::cmd_trace_event(ctx, a),
        Cmd::HookCheck(a) => trace_event::cmd_hook_check(ctx, a),
        Cmd::Create(a) => create::cmd_create(ctx, a),
        Cmd::Improve(a) => improve::cmd_improve(ctx, a),
        Cmd::Activate(a) => create::cmd_activate(ctx, a),
        Cmd::Run(a) => batch::run::cmd_run(ctx, a),
        Cmd::RunWorker(a) => batch::worker::cmd_run_worker(ctx, a),
        Cmd::Status { job } => jobs::cmd_status(ctx, job.as_deref()),
        Cmd::Library { cmd } => library_cmd::cmd_library(ctx, cmd),
        Cmd::Task(a) => batch::tasks::cmd_task(ctx, a),
        Cmd::Transfer(a) => scope::transfer::cmd_transfer(ctx, a),
        Cmd::Ship(a) => scope::ship::cmd_ship(ctx, a),
        Cmd::Stale(a) => scope::stale::cmd_stale(ctx, a),
        Cmd::Split(a) => scope::split::cmd_split(ctx, a),
        Cmd::Anchor(a) => scope::anchor::cmd_anchor(ctx, a),
        Cmd::Builtins { cmd } => builtins::cmd_builtins(ctx, cmd),
        Cmd::Count(a) => reflect::cmd_count(ctx, a),
        Cmd::Mark(a) => reflect::cmd_mark(ctx, a),
        Cmd::Off => library_cmd::cmd_off(ctx),
        Cmd::On => library_cmd::cmd_on(ctx),
        Cmd::Uninstall(a) => uninstall::cmd_uninstall(ctx, a),
    }
}

/// Operation name for a usage error: the first non-flag argument.
fn op_from_argv() -> String {
    std::env::args().skip(1).find(|a| !a.starts_with('-')).unwrap_or_else(|| "usage".into())
}

fn main() {
    proc::install_cancel_handler();
    let text_flag = std::env::args().any(|a| a == "--text");
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) => {
            use clap::error::ErrorKind;
            if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand) {
                if e.kind() == ErrorKind::DisplayVersion && !text_flag {
                    Envelope::ok("version", json!({"version": env!("CARGO_PKG_VERSION")})).print(false);
                } else {
                    let _ = e.print();
                }
                std::process::exit(if e.kind() == ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand { EXIT_USAGE } else { EXIT_OK });
            }
            let env = Envelope::err(&op_from_argv(), e.render().to_string().trim().to_string());
            // A hook must never block by accident (exit 2 is Claude Code's block code).
            if std::env::args().any(|a| a == "--hook-output") {
                std::process::exit(trace_event::emit_hook(&env));
            }
            env.print(text_flag);
            std::process::exit(EXIT_USAGE);
        }
    };
    let op = cli.cmd.name();
    let text = cli.text;
    let hook_output = matches!(&cli.cmd, Cmd::TraceEvent(a) | Cmd::HookCheck(a) if a.hook_output);
    let (env, code) = match AppCtx::open_with(cli.cwd.clone(), text) {
        Ok(ctx) => {
            let (env, code) = Envelope::from_result(op, dispatch(&ctx, cli.cmd));
            finish_job(&ctx, op, &env);
            (env, code)
        }
        Err(e) => Envelope::from_result(op, Err(e)),
    };
    if hook_output {
        std::process::exit(trace_event::emit_hook(&env));
    }
    env.print(text);
    std::process::exit(code);
}

/// A detached child (`jobs::spawn_detached`) records its final status; `run-worker`
/// manages its own job record.
fn finish_job(ctx: &AppCtx, op: &str, env: &Envelope) {
    let Some(id) = std::env::var(jobs::JOB_ID_ENV).ok().filter(|s| !s.is_empty()) else { return };
    if op == "run-worker" {
        return;
    }
    let status = if env.ok { "done" } else { "failed" };
    if let Err(e) = jobs::update(ctx, &id, status, &json!({"result": env, "finished_at": rematch_core::ledger::now_ms()})) {
        eprintln!("rematch: job {id} status update failed: {e:#}");
    }
}
