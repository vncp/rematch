//! `rematch lake export|import|rebuild|doctor|index|query|migrate` (ADR 001, DESIGN §19).
//! The Parquet side lives in `rematch-lake`, behind the cargo feature `lake` (default on);
//! without it every subcommand but `migrate` reports the missing feature. Nothing in a
//! session reads or writes the lake: `export` runs at the end of `reflect --pending` and on
//! demand; `index` (an external Bellwether binary) only on demand.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand};
use serde_json::Value;

use crate::app::AppCtx;

#[derive(Subcommand, Debug)]
pub enum LakeCmd {
    /// Journal rows past this machine's watermark and closed sessions' traces → Parquet.
    Export(ExportArgs),
    /// Merge another machine's lake (absent partition files, blobs, watermarks).
    Import(ImportArgs),
    /// A fresh ledger from the lake's journal partitions.
    Rebuild(RebuildArgs),
    /// Watermarks, export lag, files per table, orphan files, the pre-journal snapshot range, the Bellwether pin and each table's index, sessions counted with no counters row.
    Doctor(DoctorArgs),
    /// Bellwether indexes per table under REMATCH_ROOT/lake-index/ with the ADR 001 key table (on demand; never synced).
    Index(IndexArgs),
    /// One key lookup over a lake table → rows (JSON): planned by the table's index and its residual, else a full scan (`indexed: false`).
    Query(QueryArgs),
    /// Fill identity fields on rows that predate them: content_sha256 on entries, repo_remote/subpath on predicates and sessions whose repo is on this machine (idempotent).
    Migrate,
}

// Without the feature the arguments are parsed (so usage stays the same) but never read.
#[cfg_attr(not(feature = "lake"), allow(dead_code))]
#[derive(Args, Debug)]
pub struct ExportArgs {
    /// Drain the whole journal and every closed session in one run (default: one bounded run).
    #[arg(long)]
    pub all: bool,
}

#[cfg_attr(not(feature = "lake"), allow(dead_code))]
#[derive(Args, Debug)]
pub struct ImportArgs {
    /// The other lake: its directory, or the REMATCH_ROOT that holds it.
    #[arg(long, value_name = "LAKE")]
    pub from: PathBuf,
}

#[cfg_attr(not(feature = "lake"), allow(dead_code))]
#[derive(Args, Debug)]
pub struct RebuildArgs {
    /// Lake to rebuild from (default: REMATCH_ROOT/lake).
    #[arg(long, value_name = "LAKE")]
    pub from: Option<PathBuf>,
    /// Replace a non-empty ledger (the current file is kept as `ledger.sqlite.pre-rebuild-<ms>`).
    #[arg(long)]
    pub force: bool,
    /// Machine whose journal to replay (default: this machine's id).
    #[arg(long)]
    pub machine: Option<String>,
}

#[cfg_attr(not(feature = "lake"), allow(dead_code))]
#[derive(Args, Debug)]
pub struct DoctorArgs {
    /// The Bellwether binary (default: $REMATCH_BELLWETHER, else `bellwether` on PATH).
    #[arg(long, value_name = "PATH")]
    pub bellwether: Option<PathBuf>,
}

#[cfg_attr(not(feature = "lake"), allow(dead_code))]
#[derive(Args, Debug)]
pub struct IndexArgs {
    /// Tables to index (default: all): events sessions redirects entries counters proposals receipts outcomes.
    pub tables: Vec<String>,
    /// The Bellwether binary (default: $REMATCH_BELLWETHER, else `bellwether` on PATH).
    #[arg(long, value_name = "PATH")]
    pub bellwether: Option<PathBuf>,
    /// Rebuild every index under this binary's version when the lake pins another one.
    #[arg(long)]
    pub repin: bool,
}

#[cfg_attr(not(feature = "lake"), allow(dead_code))]
#[derive(Args, Debug)]
pub struct QueryArgs {
    /// The table: events sessions redirects entries counters proposals receipts outcomes.
    pub table: String,
    /// A key of the table (ADR 001 key table), e.g. exact:path, expr:tool_cmd, list:facets, temporal2:entry_id.
    #[arg(long)]
    pub key: String,
    /// Key values, comma-separated (list keys: elements; a two-column key: `a:b`, split at the last ':').
    #[arg(long, value_delimiter = ',')]
    pub values: Vec<String>,
    /// Window start (inclusive) on the table's time column: ms UTC, RFC 3339, or YYYY-MM-DD.
    #[arg(long)]
    pub from: Option<String>,
    /// Window end (inclusive), as --from.
    #[arg(long)]
    pub to: Option<String>,
    /// The version valid at this instant (temporal2; a temporal key's --at).
    #[arg(long)]
    pub valid: Option<String>,
    /// As the ledger knew it at this instant (temporal2).
    #[arg(long)]
    pub known: Option<String>,
    /// The newest version per key (with --known: the newest known by then).
    #[arg(long)]
    pub latest: bool,
    /// List keys: rows holding every value (default: any).
    #[arg(long)]
    pub all: bool,
    /// At most this many rows.
    #[arg(long)]
    pub limit: Option<usize>,
    /// The Bellwether binary (default: $REMATCH_BELLWETHER, else `bellwether` on PATH).
    #[arg(long, value_name = "PATH")]
    pub bellwether: Option<PathBuf>,
}

/// `lake migrate` is the ledger's own (L1) and needs no Parquet; the rest need the feature.
pub fn cmd_lake(ctx: &AppCtx, cmd: LakeCmd) -> Result<Value> {
    match cmd {
        LakeCmd::Migrate => crate::lake_migrate::cmd_lake_migrate(ctx),
        other => imp::run(ctx, other),
    }
}

/// `lake rebuild` runs before the usual context is opened: opening it would create the
/// ledger and stage the built-in entries, so a fresh root would never be empty.
pub fn cmd_lake_rebuild(a: RebuildArgs) -> Result<Value> {
    imp::rebuild(a)
}

/// The bounded export at the end of `reflect --pending`: never fails the caller (errors are
/// logged and reported); `None` when built without the lake feature.
pub fn export_after_reflect(ctx: &AppCtx) -> Option<Value> {
    imp::export_after_reflect(ctx)
}

#[cfg(feature = "lake")]
mod imp {
    use anyhow::{anyhow, Result};
    use rematch_core::paths::RematchPaths;
    use rematch_core::trace::TraceRecord;
    use rematch_core::types::SessionRecord;
    use serde_json::{json, Value};

    use super::{LakeCmd, QueryArgs, RebuildArgs};
    use crate::app::AppCtx;
    use crate::jobs::LockFile;

    fn lock(root: &std::path::Path) -> Result<LockFile> {
        LockFile::acquire(root.join(".lake.lock"))?
            .ok_or_else(|| anyhow!("another lake export, import, index or rebuild is running"))
    }

    /// An instant: integer ms UTC, RFC 3339, or a `YYYY-MM-DD` date (its UTC midnight).
    fn instant(flag: &str, s: &str) -> Result<i64> {
        if let Ok(ms) = s.parse::<i64>() {
            return Ok(ms);
        }
        if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
            return Ok(t.timestamp_millis());
        }
        if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
            return Ok(d.and_hms_opt(0, 0, 0).map(|t| t.and_utc().timestamp_millis()).unwrap_or(0));
        }
        Err(anyhow!("--{flag} {s:?} is not ms UTC, RFC 3339, or YYYY-MM-DD"))
    }

    fn query(ctx: &AppCtx, a: QueryArgs) -> Result<Value> {
        let at = |flag: &str, v: &Option<String>| v.as_deref().map(|s| instant(flag, s)).transpose();
        let window = match (at("from", &a.from)?, at("to", &a.to)?) {
            (None, None) => None,
            (lo, hi) => Some((lo.unwrap_or(i64::MIN), hi.unwrap_or(i64::MAX))),
        };
        let (valid, known) = (at("valid", &a.valid)?, at("known", &a.known)?);
        let spec = rematch_lake::query::QuerySpec {
            table: a.table,
            key: a.key,
            values: a.values,
            window,
            asof: (valid.is_some() || known.is_some()).then_some(rematch_lake::query::AsOf { valid, known }),
            latest: a.latest,
            all: a.all,
            limit: a.limit,
        };
        let bin = a.bellwether.as_deref().map(crate::app::absolute).transpose()?;
        let out = rematch_lake::query::run(&ctx.paths.root, &spec, bin.as_deref())?;
        let mut v = out.to_json(&spec);
        v["lines"] = json!([format!(
            "{} row(s) from {} by {}{}",
            out.rows.len(),
            spec.table,
            spec.key,
            match (&out.reason, out.indexed) {
                (_, true) => " (indexed)".to_string(),
                (Some(r), false) => format!(" (scan: {r})"),
                (None, false) => " (scan)".to_string(),
            }
        )]);
        Ok(v)
    }

    /// Records of a ledger session without a trace file, synthesized from its harness
    /// transcript by the converter `reflect --history` uses.
    struct HistorySource<'a>(&'a AppCtx);

    impl rematch_lake::EventSource for HistorySource<'_> {
        fn records(&self, session: &SessionRecord) -> Result<Option<Vec<TraceRecord>>> {
            crate::import_cmd::records_for_session(self.0, session)
        }
    }

    pub fn run(ctx: &AppCtx, cmd: LakeCmd) -> Result<Value> {
        let lake = rematch_lake::lake_dir(&ctx.paths.root);
        match cmd {
            LakeCmd::Export(a) => {
                let _l = lock(&ctx.paths.root)?;
                let opts = rematch_lake::ExportOpts { all: a.all, ..Default::default() };
                rematch_lake::export(&ctx.ledger, &lake, &ctx.paths.traces, &HistorySource(ctx), opts)
            }
            LakeCmd::Import(a) => {
                let _l = lock(&ctx.paths.root)?;
                rematch_lake::import(&lake, &crate::app::absolute(&a.from)?)
            }
            LakeCmd::Doctor(a) => {
                let mut v = rematch_lake::doctor(&ctx.ledger, &lake)?;
                let bin = a.bellwether.as_deref().map(crate::app::absolute).transpose()?;
                let mut ix = rematch_lake::index::doctor(&ctx.paths.root, bin.as_deref())?;
                let more = ix.as_object_mut().and_then(|o| o.remove("lines"));
                if let (Some(lines), Some(Value::Array(more))) = (v["lines"].as_array_mut(), more) {
                    lines.extend(more);
                }
                v["index"] = ix;
                Ok(v)
            }
            LakeCmd::Index(a) => {
                let _l = lock(&ctx.paths.root)?;
                let bin = a.bellwether.as_deref().map(crate::app::absolute).transpose()?;
                rematch_lake::index::index(&ctx.paths.root, &a.tables, bin.as_deref(), a.repin)
            }
            LakeCmd::Query(a) => query(ctx, a),
            LakeCmd::Rebuild(a) => rebuild(a),
            LakeCmd::Migrate => unreachable!("dispatched by cmd_lake"),
        }
    }

    pub fn rebuild(a: RebuildArgs) -> Result<Value> {
        let paths = RematchPaths::resolve();
        paths.ensure_dirs()?;
        let _l = lock(&paths.root)?;
        let from = match &a.from {
            Some(p) => crate::app::absolute(p)?,
            None => rematch_lake::lake_dir(&paths.root),
        };
        rematch_lake::rebuild(&paths.ledger_file, &from, a.machine.as_deref(), a.force)
    }

    pub fn export_after_reflect(ctx: &AppCtx) -> Option<Value> {
        let Ok(Some(_l)) = LockFile::acquire(ctx.paths.root.join(".lake.lock")) else {
            return Some(json!({"skipped": "another lake export, import or rebuild is running"}));
        };
        let lake = rematch_lake::lake_dir(&ctx.paths.root);
        let opts = rematch_lake::ExportOpts::default();
        match rematch_lake::export(&ctx.ledger, &lake, &ctx.paths.traces, &HistorySource(ctx), opts) {
            Ok(mut v) => {
                if let Some(o) = v.as_object_mut() {
                    o.remove("lines");
                }
                Some(v)
            }
            Err(e) => {
                eprintln!("rematch reflect: lake export failed: {e:#}");
                Some(json!({"error": format!("{e:#}")}))
            }
        }
    }
}

#[cfg(not(feature = "lake"))]
mod imp {
    use anyhow::{bail, Result};
    use serde_json::Value;

    use super::{LakeCmd, RebuildArgs};
    use crate::app::AppCtx;

    pub const MISSING: &str = "built without the lake feature";

    pub fn run(_ctx: &AppCtx, _cmd: LakeCmd) -> Result<Value> {
        bail!(MISSING)
    }

    pub fn rebuild(_a: RebuildArgs) -> Result<Value> {
        bail!(MISSING)
    }

    pub fn export_after_reflect(_ctx: &AppCtx) -> Option<Value> {
        None
    }
}
