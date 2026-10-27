//! `uninstall [--yes]`: every file rematch wrote and nothing else
//! (`library::uninstall_plan`). Without `--yes` only the plan is printed. The profile
//! (`REMATCH_ROOT/profile/`, the user's git repo, DESIGN §20) stays; the links into it go. The
//! personal `AGENTS.md` loses only its unchanged rematch blocks (`instructions::uninstall`); a
//! block the user edited stays as is and is reported.

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

pub fn cmd_uninstall(ctx: AppCtx, a: UninstallArgs) -> Result<Value> {
    let mut plan = library::uninstall_plan(&ctx.paths, &ctx.ledger)?;
    // Kept copies older versions wrote under ~/.agents/skills for Codex (`.rematch-codex.json`),
    // and the agent copies in the Claude Code plugin's agents/ (listed in its marker).
    let mut first = crate::skills_cmd::legacy_codex_copies();
    if let Some(root) = crate::skills_cmd::claude_plugin_root() {
        let copies = crate::skills_cmd::plugin_agent_copies(&root);
        first.extend(copies.into_iter().map(|(p, _)| p));
        let marker = root.join("agents").join(crate::skills_cmd::PLUGIN_AGENTS_MARKER);
        if marker.is_file() {
            first.push(marker);
        }
    }
    plan.remove.splice(0..0, first);
    let plist = std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join("Library/LaunchAgents").join(format!("{}.plist", crate::profile_cmd::LAUNCHD_LABEL)))
        .filter(|p| p.is_file());
    if let Some(p) = &plist {
        plan.remove.push(p.clone());
    }
    let profile = rematch_core::profile::dir(&ctx.paths);
    let kept: Vec<&std::path::Path> = std::iter::once(profile.as_path()).filter(|p| p.exists()).collect();
    let mut lines: Vec<String> = plan.restore.iter().map(|(f, t)| format!("restore {} → {}", f.display(), t.display())).collect();
    lines.extend(plan.remove.iter().map(|p| format!("remove {}", p.display())));
    lines.extend(kept.iter().map(|p| format!("keep {} (the profile: your git repo; only the links into it are removed)", p.display())));
    // Before the root goes: its state file says whether rematch created the personal file.
    let instructions = rematch_core::instructions::uninstall(&ctx.paths, !a.yes)?;
    lines.extend(rematch_core::instructions::Sync { conflicts: Vec::new(), ..instructions.clone() }.lines());
    for c in &instructions.conflicts {
        lines.push(match c.name.as_str() {
            "*" => format!("keep {} untouched: {} (remove the rematch marker lines by hand)", instructions.target.display(), c.reason),
            name => format!("keep block {name} in {} (edited by you; remove its rematch marker lines by hand if you want it plain)", instructions.target.display()),
        });
    }
    let mut note = "the extension link and the PATH shim belong to omp/install.sh (install.ps1 on Windows): `omp plugin uninstall @vncp/rematch` and remove ~/.local/bin/rematch (rematch.exe on Windows); links you made for other harnesses (~/.agents/skills/rematch, ~/.claude/skills/rematch) are yours to remove".to_string();
    if let Some(p) = &plist {
        note.push_str(&format!("; unload the profile sync job first: launchctl unload -w {}", p.display()));
    }
    let note = note.as_str();
    if !a.yes {
        lines.insert(0, "uninstall plan (re-run with --yes to apply):".into());
        lines.push(note.into());
        return Ok(json!({"applied": false, "plan": plan, "kept": kept, "instructions": instructions, "note": note, "lines": lines}));
    }
    // Repo-local trial delivery (Codex `.agents/skills/rematch-trials`, Claude Code
    // `.claude/agents/<name>.md`) is listed under the root: withdraw it before the root goes.
    let withdrawn = rematch_core::project_views::withdraw_all(&ctx.paths)?;
    lines.extend(withdrawn.iter().map(|p| format!("removed repo-local trial delivery {}", p.display())));
    // Close the ledger first: Windows cannot delete a file that is still open.
    drop(ctx);
    library::apply_uninstall(&plan)?;
    lines.insert(0, "uninstalled:".into());
    lines.push(note.into());
    Ok(json!({"applied": true, "plan": plan, "kept": kept, "instructions": instructions, "note": note, "lines": lines}))
}
