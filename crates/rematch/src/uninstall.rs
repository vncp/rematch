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
    let mut plan = library::uninstall_plan(&ctx.paths, &ctx.ledger)?;
    // Kept copies `library sync --harness codex` wrote under ~/.agents/skills (DESIGN §18.2.3).
    plan.remove.splice(0..0, crate::library_cmd::codex_copies());
    let mut lines: Vec<String> = plan.restore.iter().map(|(f, t)| format!("restore {} → {}", f.display(), t.display())).collect();
    lines.extend(plan.remove.iter().map(|p| format!("remove {}", p.display())));
    let note = "the extension link and the PATH shim belong to omp/install.sh: `omp plugin uninstall @vncp/rematch` and remove ~/.local/bin/rematch; links you made for other harnesses (~/.agents/skills/rematch, ~/.claude/skills/rematch) are yours to remove";
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
