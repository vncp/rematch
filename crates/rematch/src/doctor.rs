//! `doctor`: model roles, ledger schema, improver files, root paths, kill switch, and one
//! block per harness and service — omp (binary, extension linked and loadable), Claude
//! Code (plugin link, plugin validates), Codex (skill-root link), profile (remote,
//! ahead/behind, launchd), lake (export lag, index pin), built-ins — then `ready` with
//! every problem. A harness that is not set up is reported, never a problem; one that is
//! set up and broken is.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::Result;
use rematch_core::ledger::{enum_str, EntryFilter, SessionFilter, SCHEMA_VERSION};
use serde_json::{json, Value};

use crate::app::{AppCtx, IMPROVER_FILES};
use crate::config::{self, ROLE_CLASSIFIER, ROLE_EDITOR, ROLE_SOLVER};
use crate::omp;
use crate::proc::run_bounded;

pub const PLUGIN_NAME: &str = "@vncp/rematch";

fn plugins_dir(ctx: &AppCtx) -> PathBuf {
    std::env::var_os("OMP_PLUGINS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| ctx.paths.config_root.join("plugins"))
}

pub fn cmd_doctor(ctx: &AppCtx) -> Result<Value> {
    let mut problems: Vec<String> = Vec::new();
    let mut lines = Vec::new();

    let bin = omp::omp_bin();
    let mut c = Command::new(&bin);
    c.arg("--version");
    let mut omp_problems = Vec::new();
    let version = match run_bounded(c, Duration::from_secs(15)) {
        Ok(b) if b.code == Some(0) => Some(b.stdout.trim().to_string()),
        Ok(b) => {
            omp_problems.push(format!("`{} --version` exited {:?}: {}", bin.display(), b.code, b.stderr.trim()));
            None
        }
        Err(e) => {
            omp_problems.push(format!("omp not runnable at {}: {e:#}", bin.display()));
            None
        }
    };

    let mut roles = serde_json::Map::new();
    for role in [ROLE_EDITOR, ROLE_CLASSIFIER, ROLE_SOLVER] {
        match config::resolve_role(&ctx.paths, &ctx.cwd, role) {
            Ok(r) => {
                lines.push(format!("role {role}: {} (via {})", r.selector, r.role_used));
                roles.insert(role.into(), json!({"selector": r.selector, "via": r.role_used, "model": r.model}));
            }
            Err(e) => {
                problems.push(format!("role {role}: {e:#}"));
                lines.push(format!("role {role}: unresolved"));
                roles.insert(role.into(), json!({"error": format!("{e:#}")}));
            }
        }
    }

    let plugins = plugins_dir(ctx);
    let lock = plugins.join("omp-plugins.lock.json");
    let in_lock = std::fs::read_to_string(&lock).map(|t| t.contains(&format!("\"{PLUGIN_NAME}\""))).unwrap_or(false);
    let module = plugins.join("node_modules").join(PLUGIN_NAME);
    let linked_path = std::fs::read_link(&module).ok();
    let module_exists = module.exists();
    let linked = in_lock || module_exists;
    let loads = extension_loads(&module);
    if !linked {
        omp_problems.push(format!("extension not linked: run `omp plugin link <repo>/extension` ({PLUGIN_NAME} missing from {})", lock.display()));
    } else if let Err(why) = &loads {
        omp_problems.push(format!("extension {PLUGIN_NAME} does not load: {why}; run `omp plugin link <repo>/extension`"));
    }
    let omp_harness = Harness {
        line: format!(
            "omp: {} · {} ({}) · extension {PLUGIN_NAME} {}",
            if omp_problems.is_empty() { "ok" } else { "problem" },
            bin.display(),
            version.as_deref().unwrap_or("not runnable"),
            match (&loads, linked) {
                (_, false) => "not linked".to_string(),
                (Ok(entries), true) => format!(
                    "linked{}, loads {}",
                    std::fs::canonicalize(&module).map(|t| format!(" → {}", t.display())).unwrap_or_default(),
                    entries.join(", ")
                ),
                (Err(why), true) => format!("linked, does not load ({why})"),
            }
        ),
        json: json!({"bin": bin, "version": version, "extension": {"linked": linked, "loads": loads.is_ok(),
                     "entries": loads.as_ref().ok(), "error": loads.as_ref().err()}}),
        problems: omp_problems,
        set_up: true,
    };

    let schema = ctx.ledger.schema_version()?;
    if schema != SCHEMA_VERSION {
        problems.push(format!("ledger schema {schema}, binary expects {SCHEMA_VERSION}"));
    }
    lines.push(format!("ledger: {} (schema {schema})", ctx.paths.ledger_file.display()));

    let improver: Vec<Value> = IMPROVER_FILES
        .iter()
        .map(|(name, _)| {
            let p = ctx.paths.improver.join(name);
            let present = p.is_file();
            if !present {
                problems.push(format!("improver file missing: {}", p.display()));
            }
            json!({"file": name, "present": present})
        })
        .collect();
    if let Err(e) = rematch_core::lint::LintConfig::load(&ctx.paths.improver.join("lint.toml")) {
        problems.push(format!("lint.toml invalid: {e:#}"));
    }
    // `match` and `skills sync` treat an unreadable domain policy as "no domain"; say so here.
    if let Err(e) = ctx.domain_policy() {
        problems.push(format!("domain policy invalid (policy.json domains/caps, branches/*/domain.yml or domain_rules.toml): {e:#}"));
    }

    let sessions = ctx.ledger.list_sessions(&SessionFilter::default())?;
    let traced = sessions.iter().filter(|s| s.source == "trace").count();
    let mut by_status = std::collections::BTreeMap::<String, u32>::new();
    for e in ctx.ledger.list_entries(&EntryFilter::default())? {
        *by_status.entry(enum_str(&e.status)).or_default() += 1;
    }
    let pending = std::fs::read_dir(&ctx.paths.pending)
        .map(|r| r.flatten().filter(|e| e.path().extension().is_some_and(|x| x == "json")).count())
        .unwrap_or(0);
    let off = ctx.paths.is_off();
    let existing_profiles = ctx.paths.list_profiles();
    let policy = crate::scope::export::Policy::load(&ctx.paths.root).unwrap_or_default();
    let mut skipped_profiles: Vec<String> = policy
        .skip_profiles
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    skipped_profiles.sort();
    skipped_profiles.dedup();
    let active_profile = ctx.paths.profile.as_deref().unwrap_or("default");
    let profiles_note = if existing_profiles.is_empty() {
        "none".to_string()
    } else {
        existing_profiles.join(", ")
    };
    let skip_note = if skipped_profiles.is_empty() {
        String::new()
    } else {
        format!(" (skipped: {})", skipped_profiles.join(", "))
    };
    lines.push(format!(
        "agent_dir: {} (profile: {active_profile} · profiles: {profiles_note}{skip_note})",
        ctx.paths.agent_dir.display()
    ));
    lines.push(format!(
        "root: {} · {} sessions ({} traced) · {} pending · off: {off}",
        ctx.paths.root.display(),
        sessions.len(),
        traced,
        pending
    ));

    let harnesses = [
        ("omp", omp_harness),
        ("claude-code", claude_code_harness(ctx)),
        ("codex", codex_harness(ctx)),
        ("pi", pi_harness(ctx)),
        ("opencode", {
            let (line, json, problems, set_up) = crate::opencode_event::doctor_harness(ctx);
            Harness { line, json, problems, set_up }
        }),
        ("profile", profile_harness(ctx)?),
        ("lake", lake_harness(ctx)?),
        ("built-ins", builtins_harness(ctx)?),
    ];
    let mut block = serde_json::Map::new();
    let mut harness_lines = Vec::new();
    for (name, h) in harnesses {
        let status = if !h.problems.is_empty() {
            "problem"
        } else if h.set_up {
            "ok"
        } else {
            "not set up"
        };
        let mut j = h.json;
        j["status"] = json!(status);
        j["problems"] = json!(h.problems);
        problems.extend(h.problems);
        harness_lines.push(h.line);
        block.insert(name.into(), j);
    }
    lines.splice(0..0, harness_lines);
    for p in &problems {
        lines.push(format!("problem: {p}"));
    }
    let ready = problems.is_empty();
    lines.push(if ready { "ready: true".to_string() } else { format!("ready: false ({} problem(s))", problems.len()) });

    Ok(json!({
        "ready": ready,
        "problems": problems,
        "harnesses": block,
        "omp": {"bin": bin, "version": version},
        "roles": roles,
        "extension": {"linked": linked, "loads": loads.is_ok(), "lock_file": lock, "in_lock": in_lock, "module": module,
                      "module_exists": module_exists, "link_target": linked_path},
        "ledger": {"path": ctx.paths.ledger_file, "schema_version": schema, "binary_schema_version": SCHEMA_VERSION,
                   "sessions": sessions.len(), "traced_sessions": traced, "entries": by_status},
        "improver": {"dir": ctx.paths.improver, "files": improver},
        "paths": {"root": ctx.paths.root, "config_root": ctx.paths.config_root, "profile": ctx.paths.profile,
                  "agent_dir": ctx.paths.agent_dir, "traces": ctx.paths.traces, "pending": ctx.paths.pending,
                  "staging": ctx.paths.staging, "drafts": ctx.paths.drafts, "archive": ctx.paths.archive, "jobs": ctx.paths.jobs,
                  "skills": ctx.paths.skills_dir(), "hooks": ctx.paths.hooks_dir()},
        "profiles": {"active": ctx.paths.profile, "existing": existing_profiles, "skipped": skipped_profiles},
        "pending": pending,
        "off": off,
        "version": env!("CARGO_PKG_VERSION"),
        "lines": lines,
    }))
}

/// One block of `doctor`'s `harnesses`: its text line, its JSON (status and problems are
/// added by the caller), and whether it is set up at all.
struct Harness {
    line: String,
    json: Value,
    problems: Vec<String>,
    set_up: bool,
}

fn resolves_to(p: &Path, want: &Path) -> bool {
    match (std::fs::canonicalize(p), std::fs::canonicalize(want)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// The linked extension loads: its module dir resolves, its `package.json` is ours and every
/// `omp.extensions` entry is a file. Ok: the entries.
fn extension_loads(module: &Path) -> Result<Vec<String>, String> {
    let dir = std::fs::canonicalize(module).map_err(|e| format!("{} does not resolve ({e})", module.display()))?;
    let pj = dir.join("package.json");
    let text = std::fs::read_to_string(&pj).map_err(|e| format!("{}: {e}", pj.display()))?;
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", pj.display()))?;
    if v["name"] != PLUGIN_NAME {
        return Err(format!("{} names {}, not {PLUGIN_NAME}", pj.display(), v["name"]));
    }
    let entries: Vec<String> = v["omp"]["extensions"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect();
    if entries.is_empty() {
        return Err(format!("{} lists no omp.extensions", pj.display()));
    }
    if let Some(m) = entries.iter().find(|e| !dir.join(e).is_file()) {
        return Err(format!("entry {m} is missing from {}", dir.display()));
    }
    Ok(entries)
}

/// `<config dir>/skills/rematch` (`$CLAUDE_CONFIG_DIR`, else `~/.claude`): the plugin link is
/// present and resolves, its manifest and hooks parse, its `skills/` reaches the shared dir,
/// and `claude plugin validate` passes (`$CLAUDE_BIN`, else `claude` on PATH; static checks
/// only without one).
fn claude_code_harness(ctx: &AppCtx) -> Harness {
    let Some(link) = rematch_core::import_claude::config_dir().map(|d| d.join("skills").join("rematch")) else {
        return Harness { line: "claude-code: not set up (HOME is not set)".into(), json: json!({"link": null}), problems: vec![], set_up: false };
    };
    if std::fs::symlink_metadata(&link).is_err() {
        return Harness {
            line: format!("claude-code: not set up ({} absent)", link.display()),
            json: json!({"link": link, "present": false}),
            problems: vec![],
            set_up: false,
        };
    }
    let mut problems = Vec::new();
    let target = std::fs::canonicalize(&link).ok();
    if target.is_none() {
        problems.push(format!("{} does not resolve (a dangling link): link it to <repo>/adapters/claude-code", link.display()));
    }
    let manifest: Option<Value> = std::fs::read_to_string(link.join(".claude-plugin/plugin.json")).ok().and_then(|t| serde_json::from_str(&t).ok());
    if target.is_some() && manifest.as_ref().is_none_or(|m| m["name"] != "rematch") {
        problems.push(format!("{}/.claude-plugin/plugin.json is missing or not the rematch plugin", link.display()));
    }
    let hooks_ok = std::fs::read_to_string(link.join("hooks/hooks.json")).ok().is_some_and(|t| serde_json::from_str::<Value>(&t).is_ok());
    if target.is_some() && !hooks_ok {
        problems.push(format!("{}/hooks/hooks.json is missing or not JSON", link.display()));
    }
    let skills = link.join("skills");
    let skills_ok = resolves_to(&skills, &ctx.paths.skills_shared);
    if target.is_some() && !skills_ok {
        problems.push(format!(
            "{} does not reach {}: run `rematch skills sync --harness claude-code`",
            skills.display(),
            ctx.paths.skills_shared.display()
        ));
    }
    let claude = std::env::var_os("CLAUDE_BIN").filter(|v| !v.is_empty()).map(PathBuf::from).or_else(|| omp::which("claude"));
    let validated = match (&claude, &target) {
        (Some(bin), Some(t)) => {
            let mut c = Command::new(bin);
            c.args(["plugin", "validate"]).arg(t);
            match run_bounded(c, Duration::from_secs(30)) {
                Ok(b) if b.code == Some(0) => Some(true),
                Ok(b) => {
                    let out = format!("{}\n{}", b.stdout, b.stderr);
                    let tail = out.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("").to_string();
                    problems.push(format!("`claude plugin validate {}` exited {:?}: {tail}", t.display(), b.code));
                    Some(false)
                }
                Err(e) => {
                    problems.push(format!("`claude plugin validate` did not run: {e:#}"));
                    Some(false)
                }
            }
        }
        _ => None,
    };
    let line = format!(
        "claude-code: {} · {} → {} · skills/ {} · {}",
        if problems.is_empty() { "ok" } else { "problem" },
        link.display(),
        target.as_deref().map(|t| t.display().to_string()).unwrap_or_else(|| "dangling".into()),
        if skills_ok { "→ shared dir" } else { "not linked" },
        match validated {
            Some(true) => "claude plugin validate passed".to_string(),
            Some(false) => "claude plugin validate failed".to_string(),
            None if claude.is_none() => "claude not on PATH: static checks only".to_string(),
            None => "not validated".to_string(),
        }
    );
    Harness {
        line,
        json: json!({"link": link, "present": true, "target": target, "manifest": manifest.is_some(), "hooks": hooks_ok,
                     "skills_linked": skills_ok, "claude": claude, "validated": validated}),
        problems,
        set_up: true,
    }
}

/// `~/.agents/skills/rematch` (Codex's user skill root): present and resolving to the
/// shared skills dir.
fn codex_harness(ctx: &AppCtx) -> Harness {
    let Some(link) = std::env::var_os("HOME").filter(|h| !h.is_empty()).map(|h| PathBuf::from(h).join(".agents/skills/rematch")) else {
        return Harness { line: "codex: not set up (HOME is not set)".into(), json: json!({"link": null}), problems: vec![], set_up: false };
    };
    if std::fs::symlink_metadata(&link).is_err() {
        return Harness {
            line: format!("codex: not set up ({} absent; ln -s {} {})", link.display(), ctx.paths.skills_shared.display(), link.display()),
            json: json!({"link": link, "present": false}),
            problems: vec![],
            set_up: false,
        };
    }
    let reaches = resolves_to(&link, &ctx.paths.skills_shared);
    let problems = if reaches {
        vec![]
    } else {
        vec![format!(
            "{} does not reach {}: run `rematch skills sync --harness codex`, or re-point it",
            link.display(),
            ctx.paths.skills_shared.display()
        )]
    };
    Harness {
        line: format!(
            "codex: {} · {} → {}",
            if reaches { "ok" } else { "problem" },
            link.display(),
            if reaches { "shared dir".to_string() } else { std::fs::read_link(&link).map(|t| t.display().to_string()).unwrap_or_else(|_| "?".into()) }
        ),
        json: json!({"link": link, "present": true, "reaches_shared": reaches}),
        problems,
        set_up: true,
    }
}

/// Name of the Pi package (`adapters/pi/package.json`).
pub const PI_PACKAGE_NAME: &str = "@vncp/rematch-pi";

/// The rematch Pi package in Pi's user settings (`<pi agent dir>/settings.json`: a `packages`
/// source or an `extensions` path whose `package.json` is ours, relative paths from the agent
/// dir) and the `pi` binary on `PATH` (information only: a machine without Pi is not set up,
/// never a problem). A listed source that no longer holds the package is a problem.
fn pi_harness(ctx: &AppCtx) -> Harness {
    let pi_bin = std::env::var_os("PI_BIN").filter(|v| !v.is_empty()).map(PathBuf::from).or_else(|| omp::which("pi"));
    let Some(agent) = rematch_core::import_pi::default_agent_dir(&ctx.paths.config_root) else {
        return Harness { line: "pi: not set up (HOME is not set)".into(), json: json!({"agent_dir": null}), problems: vec![], set_up: false };
    };
    let settings_path = agent.join("settings.json");
    let settings: Option<Value> = std::fs::read_to_string(&settings_path).ok().and_then(|t| serde_json::from_str(&t).ok());
    let sources: Vec<String> = settings
        .iter()
        .flat_map(|s| {
            let packages = s["packages"].as_array().into_iter().flatten().filter_map(|p| p.as_str().or_else(|| p["source"].as_str()));
            let extensions = s["extensions"].as_array().into_iter().flatten().filter_map(Value::as_str);
            packages.chain(extensions).map(str::to_string).collect::<Vec<_>>()
        })
        .collect();
    // Pi resolves a relative source lexically against the settings dir (`path.resolve`), so `..`
    // pops a name here even where the dir is reached through a symlink (`/tmp` → `/private/tmp`).
    let resolve = |src: &str| -> Option<PathBuf> {
        if src.starts_with("npm:") || src.starts_with("git:") || src.contains("://") {
            return None;
        }
        let src = src.trim_start_matches(['+', '-']);
        let joined = match src.strip_prefix("~/") {
            Some(rest) => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(rest),
            None => agent.join(src),
        };
        let mut out = PathBuf::new();
        for c in joined.components() {
            match c {
                std::path::Component::ParentDir => {
                    out.pop();
                }
                std::path::Component::CurDir => {}
                other => out.push(other),
            }
        }
        Some(out)
    };
    let ours = |dir: &Path| {
        std::fs::read_to_string(dir.join("package.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .is_some_and(|p| p["name"] == PI_PACKAGE_NAME)
    };
    let mut problems = Vec::new();
    let mut installed: Option<PathBuf> = None;
    for (src, dir) in sources.iter().filter_map(|s| resolve(s).map(|d| (s, d))) {
        if ours(&dir) {
            installed = Some(dir);
            break;
        }
        if src.contains("adapters/pi") && !dir.is_dir() {
            problems.push(format!("Pi settings {} list {src}, which no longer exists: pi install <rematch checkout>/adapters/pi", settings_path.display()));
        }
    }
    let bin_note = match &pi_bin {
        Some(b) => format!("pi at {}", b.display()),
        None => "pi not on PATH".into(),
    };
    let json = json!({"agent_dir": agent, "settings": settings_path, "package": installed, "pi": pi_bin});
    match &installed {
        Some(dir) => Harness {
            line: format!("pi: {} · package {} · {bin_note}", if problems.is_empty() { "ok" } else { "problem" }, dir.display()),
            json,
            problems,
            set_up: true,
        },
        None => Harness {
            line: format!("pi: not set up (pi install <rematch checkout>/adapters/pi; {bin_note})"),
            json,
            set_up: !problems.is_empty(),
            problems,
        },
    }
}

/// The profile: remote, ahead/behind as of the last fetch (no network), conflicts, and the
/// launchd job (`launchctl list <label>`, `$LAUNCHCTL_BIN` else `launchctl`) once its plist
/// is written.
fn profile_harness(ctx: &AppCtx) -> Result<Harness> {
    if !rematch_core::profile::present(&ctx.paths) {
        return Ok(Harness {
            line: "profile: not set up (rematch profile init [--remote <url>] [--adopt])".into(),
            json: json!({"present": false}),
            problems: vec![],
            set_up: false,
        });
    }
    let st = crate::profile_cmd::status(ctx)?;
    let mut problems = Vec::new();
    let conflicts = st["conflicts"].as_array().map_or(0, Vec::len);
    if conflicts > 0 {
        problems.push(crate::profile_cmd::conflict_line(&rematch_core::profile::dir(&ctx.paths), &serde_json::from_value::<Vec<String>>(st["conflicts"].clone()).unwrap_or_default()));
    }
    let plist = crate::profile_cmd::launchd_plist_path();
    let installed = plist.as_ref().is_some_and(|p| p.is_file());
    let loaded = if installed {
        let bin = std::env::var_os("LAUNCHCTL_BIN").filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| "launchctl".into());
        let mut c = Command::new(bin);
        c.args(["list", crate::profile_cmd::LAUNCHD_LABEL]);
        run_bounded(c, Duration::from_secs(10)).ok().map(|b| b.code == Some(0))
    } else {
        None
    };
    if installed && loaded == Some(false) {
        problems.push(format!(
            "the profile-sync launchd job is written but not loaded: launchctl load -w {}",
            plist.as_deref().map(|p| p.display().to_string()).unwrap_or_default()
        ));
    }
    let remote = st["remote"].as_str();
    let line = format!(
        "profile: {} · {} · {} · launchd {}",
        if problems.is_empty() { "ok" } else { "problem" },
        remote.map(|r| format!("origin {r}")).unwrap_or_else(|| "no remote".into()),
        match (st["ahead"].as_u64(), st["behind"].as_u64()) {
            (Some(a), Some(b)) => format!("{a} ahead, {b} behind (last fetch)"),
            _ => "not fetched".into(),
        },
        match (installed, loaded) {
            (false, _) => "not installed",
            (true, Some(true)) => "loaded",
            (true, Some(false)) => "written, not loaded",
            (true, None) => "written, launchctl not runnable",
        }
    );
    Ok(Harness {
        line,
        json: json!({"present": true, "dir": st["dir"], "remote": remote, "branch": st["branch"], "ahead": st["ahead"],
                     "behind": st["behind"], "dirty": st["dirty"], "conflicts": st["conflicts"],
                     "launchd": {"plist": plist, "installed": installed, "loaded": loaded}}),
        problems,
        set_up: true,
    })
}

fn lake_harness(ctx: &AppCtx) -> Result<Harness> {
    let mut v = crate::lake_cmd::doctor_status(ctx)?;
    let problems: Vec<String> = serde_json::from_value(v["problems"].take()).unwrap_or_default();
    let line = v["line"].as_str().unwrap_or("lake: ?").to_string();
    let set_up = v["present"] == true;
    if let Some(o) = v.as_object_mut() {
        o.remove("line");
        o.remove("status");
    }
    Ok(Harness { line, json: v, problems, set_up })
}

/// Each built-in entry's status; one that was never installed is a problem.
fn builtins_harness(ctx: &AppCtx) -> Result<Harness> {
    let ls = crate::builtins::cmd_builtins(ctx, crate::builtins::BuiltinsCmd::Ls)?;
    let items = ls["builtins"].as_array().cloned().unwrap_or_default();
    let missing: Vec<&str> = items.iter().filter(|x| x["status"].is_null()).filter_map(|x| x["name"].as_str()).collect();
    let problems = if missing.is_empty() {
        vec![]
    } else {
        vec![format!("built-in(s) not installed: {} (rematch builtins reinstall)", missing.join(", "))]
    };
    let each: Vec<String> = items
        .iter()
        .map(|x| match x["status"].as_str() {
            Some(s) => format!("{} {s}{}", x["name"].as_str().unwrap_or("?"), if x["approved"] == true { ", approved" } else { ", awaiting approval" }),
            None => format!("{} not installed", x["name"].as_str().unwrap_or("?")),
        })
        .collect();
    Ok(Harness {
        line: format!("built-ins: {} · {}", if problems.is_empty() { "ok" } else { "problem" }, each.join(" · ")),
        json: json!({"builtins": items}),
        problems,
        set_up: true,
    })
}
