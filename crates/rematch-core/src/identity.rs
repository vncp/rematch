//! Repo identity that survives machines (ADR 001 "Identity that survives machines"): the
//! normalized remote, the toplevel-relative subpath and HEAD. File reads only (`.git`,
//! `config`, `HEAD`, refs); never a git process, so importers and `match` can use it.

use std::path::{Path, PathBuf};

use crate::ledger::sha256_hex;

/// Prefix of the identity of a repo without a remote.
pub const LOCAL_PREFIX: &str = "local:";

/// Hosts whose repo paths are case-insensitive: the path part is lowercased.
const CASE_INSENSITIVE_HOSTS: &[&str] = &["github.com", "gitlab.com"];

/// Normalize a remote URL to `host/org/repo`: scheme, user, port, `.git` and trailing `/`
/// stripped; host lowercased; path lowercased on github.com and gitlab.com. Accepts
/// `scheme://[user@]host[:port]/path`, scp-like `[user@]host:path`, an already normalized
/// `host/path` (idempotent) and a `local:` identity (returned as is); a filesystem remote
/// (`/path`, `./path`, `file://`) becomes `file:<path>`. `None` for an empty URL or one
/// without a path.
pub fn normalize_remote(url: &str) -> Option<String> {
    let s = url.trim();
    if s.is_empty() {
        return None;
    }
    if s.starts_with(LOCAL_PREFIX) {
        return Some(s.to_string());
    }
    let (host, path) = if let Some((scheme, rest)) = s.split_once("://") {
        if scheme.eq_ignore_ascii_case("file") {
            return file_remote(rest);
        }
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        (strip_user_port(authority), path)
    } else if s.starts_with('/') || s.starts_with('.') || s.starts_with('~') {
        return file_remote(s);
    } else {
        match s.split_once(':') {
            // scp-like `git@host:org/repo`: no `/` before the colon.
            Some((left, right)) if !left.contains('/') => (strip_user_port(left), right),
            _ => s.split_once('/').map_or((s, ""), |(h, p)| (h, p)),
        }
    };
    let host = host.to_ascii_lowercase();
    let mut path = trim_repo_path(path);
    if host.is_empty() || path.is_empty() {
        return None;
    }
    if CASE_INSENSITIVE_HOSTS.contains(&host.as_str()) {
        path = path.to_lowercase();
    }
    Some(format!("{host}/{path}"))
}

fn strip_user_port(authority: &str) -> &str {
    let host = authority.rsplit('@').next().unwrap_or(authority);
    host.split(':').next().unwrap_or(host)
}

fn trim_repo_path(path: &str) -> String {
    let p = path.trim().trim_matches('/');
    let p = p.strip_suffix(".git").unwrap_or(p).trim_end_matches('/');
    p.split('/').filter(|s| !s.is_empty()).collect::<Vec<_>>().join("/")
}

fn file_remote(path: &str) -> Option<String> {
    let p = trim_repo_path(path);
    let root = if path.trim().starts_with('/') { "/" } else { "" };
    (!p.is_empty()).then(|| format!("file:{root}{p}"))
}

/// Identity of a repo without a remote: `local:<sha256 of the toplevel path>`.
pub fn local_identity(toplevel: &Path) -> String {
    let p = std::fs::canonicalize(toplevel).unwrap_or_else(|_| toplevel.to_path_buf());
    format!("{LOCAL_PREFIX}{}", sha256_hex(p.to_string_lossy().as_bytes()))
}

/// `(git dir, common dir)` of a worktree toplevel: `.git` as a directory (both the same), or
/// a `.git` file `gitdir: <path>` (linked worktree, submodule) whose `commondir` names the
/// shared dir. `None` when `toplevel/.git` is neither.
pub fn git_dirs(toplevel: &Path) -> Option<(PathBuf, PathBuf)> {
    let dotgit = toplevel.join(".git");
    if dotgit.is_dir() {
        return Some((dotgit.clone(), dotgit));
    }
    let text = std::fs::read_to_string(&dotgit).ok()?;
    let rel = text.lines().find_map(|l| l.trim().strip_prefix("gitdir:"))?.trim();
    let gitdir = toplevel.join(rel);
    let common = match std::fs::read_to_string(gitdir.join("commondir")) {
        Ok(c) if !c.trim().is_empty() => gitdir.join(c.trim()),
        _ => gitdir.clone(),
    };
    Some((gitdir, common))
}

/// URL of `origin`, else of the first remote with a URL, from git config text.
pub fn remote_url_from_config(text: &str) -> Option<String> {
    let mut first: Option<String> = None;
    let mut section: Option<String> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') {
            let inner = line.trim_start_matches('[').split(']').next().unwrap_or("").trim();
            section = inner.strip_prefix("remote").and_then(|r| {
                let r = r.trim();
                r.strip_prefix('"').and_then(|r| r.strip_suffix('"')).map(str::to_string)
            });
            continue;
        }
        let Some(remote) = &section else { continue };
        let Some((k, v)) = line.split_once('=') else { continue };
        if !k.trim().eq_ignore_ascii_case("url") {
            continue;
        }
        let v = v.trim();
        let v = v.strip_prefix('"').and_then(|x| x.strip_suffix('"')).unwrap_or(v).to_string();
        if remote == "origin" {
            return Some(v);
        }
        if first.is_none() {
            first = Some(v);
        }
    }
    first
}

/// The repo's identity: [`normalize_remote`] of its origin (else first) remote; a repo
/// without a remote gets [`local_identity`] of its main worktree's toplevel, so linked
/// worktrees of one repo share it. `None` when `toplevel` is not a git worktree.
pub fn repo_remote_of(toplevel: &Path) -> Option<String> {
    let (_, common) = git_dirs(toplevel)?;
    if let Some(url) = std::fs::read_to_string(common.join("config")).ok().as_deref().and_then(remote_url_from_config) {
        if let Some(n) = normalize_remote(&url) {
            return Some(n);
        }
    }
    let common = std::fs::canonicalize(&common).unwrap_or(common);
    let main = match (common.file_name(), common.parent()) {
        (Some(n), Some(parent)) if n == ".git" => parent.to_path_buf(),
        _ => toplevel.to_path_buf(),
    };
    Some(local_identity(&main))
}

/// HEAD commit of a worktree: a detached sha, else the branch ref from the worktree's git
/// dir, the common dir, or `packed-refs`. `None` without commits.
pub fn head_sha_of(toplevel: &Path) -> Option<String> {
    let (gitdir, common) = git_dirs(toplevel)?;
    let head = std::fs::read_to_string(gitdir.join("HEAD")).ok()?;
    let head = head.trim();
    let Some(r) = head.strip_prefix("ref:").map(str::trim) else {
        return is_sha(head).then(|| head.to_string());
    };
    for dir in [&gitdir, &common] {
        if let Ok(s) = std::fs::read_to_string(dir.join(r)) {
            if is_sha(s.trim()) {
                return Some(s.trim().to_string());
            }
        }
    }
    let packed = std::fs::read_to_string(common.join("packed-refs")).ok()?;
    packed.lines().find_map(|l| {
        let (sha, name) = l.trim().split_once(' ')?;
        (name == r && is_sha(sha)).then(|| sha.to_string())
    })
}

fn is_sha(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `cwd` relative to the repo toplevel, `/`-separated (`""` at the top); both spellings of
/// a symlinked root are tried. `None` when `cwd` is outside the repo.
pub fn subpath_of(toplevel: &Path, cwd: &Path) -> Option<String> {
    let rel = |top: &Path, c: &Path| {
        c.strip_prefix(top)
            .ok()
            .map(|r| r.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join("/"))
    };
    rel(toplevel, cwd).or_else(|| {
        let top = std::fs::canonicalize(toplevel).ok()?;
        let c = std::fs::canonicalize(cwd).ok()?;
        rel(&top, &c)
    })
}

/// `(repo_remote, subpath)` of a session whose repo toplevel and cwd are given, when the repo
/// is a worktree on this machine (importers and adapters; file reads only).
pub fn session_place(repo: Option<&str>, cwd: &str) -> (Option<String>, Option<String>) {
    match repo.filter(|r| !r.is_empty()).map(Path::new) {
        Some(top) => (repo_remote_of(top), subpath_of(top, Path::new(cwd))),
        None => (None, None),
    }
}

/// Fill a session's missing `repo_remote` / `subpath` from its repo on this machine (a trace
/// whose writer could not ask git, e.g. the extension's origin-only lookup on a repo without
/// `origin`). Fields already set are kept.
pub fn fill_session(s: &mut crate::types::SessionRecord) {
    if s.repo_remote.is_some() && s.subpath.is_some() {
        return;
    }
    let (remote, subpath) = session_place(s.repo.as_deref(), &s.cwd);
    if s.repo_remote.is_none() {
        s.repo_remote = remote;
    }
    if s.subpath.is_none() {
        s.subpath = subpath;
    }
}

/// What a session or entry records about its repo (ADR 001).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoIdentity {
    pub remote: Option<String>,
    pub subpath: Option<String>,
    pub head_sha: Option<String>,
}

/// Identity of `cwd` inside the worktree `toplevel` (file reads only).
pub fn identify(toplevel: &Path, cwd: &Path) -> RepoIdentity {
    RepoIdentity {
        remote: repo_remote_of(toplevel),
        subpath: subpath_of(toplevel, cwd),
        head_sha: head_sha_of(toplevel),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_https_ssh_scp_and_is_idempotent() {
        let want = Some("github.com/vncp/rematch".to_string());
        for url in [
            "https://github.com/vncp/rematch.git",
            "https://user:tok@GitHub.com/Vncp/Rematch/",
            "ssh://git@github.com:22/vncp/rematch.git",
            "git@github.com:vncp/rematch.git",
            "git@github.com:/Vncp/rematch",
            "github.com/vncp/rematch",
            "  http://github.com//vncp/rematch.git/  ",
        ] {
            assert_eq!(normalize_remote(url), want, "{url}");
        }
        // other hosts keep path case, lowercase the host
        assert_eq!(
            normalize_remote("ssh://git@Git.Example.ORG/Team/Svc.git").as_deref(),
            Some("git.example.org/Team/Svc")
        );
        assert_eq!(normalize_remote("git@gitlab.com:Grp/Sub/Repo.git").as_deref(), Some("gitlab.com/grp/sub/repo"));
        assert_eq!(normalize_remote("/srv/git/repo.git").as_deref(), Some("file:/srv/git/repo"));
        assert_eq!(normalize_remote("file:///srv/git/repo/").as_deref(), Some("file:/srv/git/repo"));
        assert_eq!(normalize_remote("local:abc").as_deref(), Some("local:abc"));
        assert_eq!(normalize_remote(""), None);
        assert_eq!(normalize_remote("https://github.com/"), None);
        let n = normalize_remote("git@github.com:vncp/rematch.git").unwrap();
        assert_eq!(normalize_remote(&n).unwrap(), n);
    }

    #[test]
    fn config_prefers_origin_else_first_remote() {
        let cfg = "[core]\n\tbare = false\n[remote \"upstream\"]\n\turl = git@github.com:up/r.git\n[remote \"origin\"]\n\turl=https://github.com/me/r\n";
        assert_eq!(remote_url_from_config(cfg).as_deref(), Some("https://github.com/me/r"));
        let cfg = "[remote \"fork\"]\n\tfetch = +refs/heads/*:refs/remotes/fork/*\n\turl = \"git@h.io:a/b.git\"\n";
        assert_eq!(remote_url_from_config(cfg).as_deref(), Some("git@h.io:a/b.git"));
        assert_eq!(remote_url_from_config("[core]\n\turl = nope\n"), None);
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git").args(args).current_dir(dir).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    #[test]
    fn repo_identity_with_remote_worktree_and_without_remote() {
        let t = tempfile::tempdir().unwrap();
        let main = t.path().join("main");
        std::fs::create_dir_all(main.join("crates/x")).unwrap();
        git(&main, &["init", "-q"]);
        // No remote: a local identity of the toplevel, no HEAD yet.
        let local = repo_remote_of(&main).unwrap();
        assert_eq!(local, local_identity(&main));
        assert!(local.starts_with(LOCAL_PREFIX));
        assert_eq!(head_sha_of(&main), None);
        git(&main, &["-c", "user.email=a@b", "-c", "user.name=a", "commit", "-q", "--allow-empty", "-m", "x"]);
        let head = head_sha_of(&main).unwrap();
        assert_eq!(head.len(), 40);
        // A linked worktree of a remote-less repo shares the main toplevel's identity.
        let wt = t.path().join("wt");
        git(&main, &["worktree", "add", "-q", "-b", "side", wt.to_str().unwrap()]);
        assert_eq!(repo_remote_of(&wt).as_deref(), Some(local.as_str()));
        assert_eq!(head_sha_of(&wt).as_deref(), Some(head.as_str()));
        git(&main, &["remote", "add", "origin", "git@github.com:Org/Repo.git"]);
        assert_eq!(repo_remote_of(&main).as_deref(), Some("github.com/org/repo"));
        assert_eq!(repo_remote_of(&wt).as_deref(), Some("github.com/org/repo"));
        let id = identify(&main, &main.join("crates/x"));
        assert_eq!(id.subpath.as_deref(), Some("crates/x"));
        assert_eq!(identify(&main, &main).subpath.as_deref(), Some(""));
        assert_eq!(subpath_of(&main, t.path()), None);
        assert_eq!(repo_remote_of(t.path()), None, "not a worktree");
    }
}
