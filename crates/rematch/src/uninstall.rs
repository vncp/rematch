//! `uninstall [--yes]`: every file rematch wrote and nothing else
//! (`library::uninstall_plan`). Without `--yes` only the plan is printed.

use anyhow::Result;
use clap::Args;
use rematch_core::library;
use serde_json::{json, Value};

use crate::app::AppCtx;

#[derive(Args, Debug, Clone, Default)]
pub struct UninstallArgs {
    /// Apply the plan (otherwise print it).
    #[arg(long)]
    pub yes: bool,
}

pub fn cmd_uninstall(ctx: &AppCtx, a: UninstallArgs) -> Result<Value> {
    let plan = library::uninstall_plan(&ctx.paths, &ctx.ledger)?;
    let mut lines: Vec<String> = plan.restore.iter().map(|(f, t)| format!("restore {} → {}", f.display(), t.display())).collect();
    lines.extend(plan.remove.iter().map(|p| format!("remove {}", p.display())));
    let note = "the extension link and the PATH shim belong to omp/install.sh: `omp plugin uninstall @vncp/rematch` and remove ~/.local/bin/rematch";
    if !a.yes {
        lines.insert(0, "uninstall plan (re-run with --yes to apply):".into());
        lines.push(note.into());
        return Ok(json!({"applied": false, "plan": plan, "note": note, "lines": lines}));
    }
    library::apply_uninstall(&plan)?;
    lines.insert(0, "uninstalled:".into());
    lines.push(note.into());
    Ok(json!({"applied": true, "plan": plan, "note": note, "lines": lines}))
}
