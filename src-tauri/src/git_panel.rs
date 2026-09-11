// Git management panel (the right-side panel's "Git" tab): status lists,
// staging, commit, branch switching, per-file diff and recent history for
// the session's main workspace.
//
// Every command here is user-driven (a click in the panel), never something
// the model calls — that's why they don't go through the approval flow.
// All git invocations reuse worktree::git (CREATE_NO_WINDOW + 30s deadline,
// spawned without a shell → paths and messages can't inject anything).

use serde::Serialize;
use std::path::Path;

/// Diff display cap (chars) — same spirit as worktree::DIFF_CAP.
const DIFF_CAP: usize = 160_000;

#[derive(Serialize, Debug)]
pub struct GitCommit {
    pub hash: String,
    pub subject: String,
    pub author: String,
    /// Unix seconds.
    pub ts: i64,
}

#[derive(Serialize, Debug)]
pub struct GitFile {
    /// Porcelain letter: M/A/D/R/U/C/T or "?" for untracked.
    pub status: String,
    /// Workspace-relative path.
    pub path: String,
}

#[derive(Serialize, Debug)]
pub struct GitOverview {
    pub repo: bool,
    pub branch: Option<String>,
    pub ahead: Option<i64>,
    pub behind: Option<i64>,
    pub staged: Vec<GitFile>,
    pub unstaged: Vec<GitFile>,
    pub untracked: Vec<GitFile>,
    pub log: Vec<GitCommit>,
}

#[derive(Serialize, Debug)]
pub struct GitBranch {
    pub name: String,
    pub current: bool,
}

/// Parse `git status --porcelain -uall` into staged/unstaged/untracked lists.
/// Column X = index (staged), column Y = worktree (unstaged); a file can
/// legitimately appear in both (e.g. "MM").
fn parse_status(out: &str) -> (Vec<GitFile>, Vec<GitFile>, Vec<GitFile>) {
    let mut staged = Vec::new();
    let mut unstaged = Vec::new();
    let mut untracked = Vec::new();
    for line in out.lines() {
        if line.chars().count() < 4 {
            continue;
        }
        let mut ch = line.chars();
        let x = ch.next().unwrap_or(' ');
        let y = ch.next().unwrap_or(' ');
        let _ = ch.next(); // separator space
        let body: String = ch.collect();
        // rename lines carry "old -> new" — track the destination
        let path = match body.split_once(" -> ") {
            Some((_old, new)) => new.to_string(),
            None => body,
        };
        let path = path.trim_matches('"').to_string();
        if x == '?' && y == '?' {
            untracked.push(GitFile { status: "?".into(), path });
            continue;
        }
        if x != ' ' {
            staged.push(GitFile { status: x.to_string(), path: path.clone() });
        }
        if y != ' ' {
            unstaged.push(GitFile { status: y.to_string(), path });
        }
    }
    (staged, unstaged, untracked)
}

fn overview_impl(root: &Path) -> Result<GitOverview, String> {
    let status_out = crate::worktree::git(root, &["status", "--porcelain", "-uall"])?;
    let (staged, unstaged, untracked) = parse_status(&status_out);
    // symbolic-ref works even before the first commit (unborn branch);
    // rev-parse --abbrev-ref covers detached HEAD (returns "HEAD")
    let branch = match crate::worktree::git(root, &["symbolic-ref", "--short", "-q", "HEAD"]) {
        Ok(b) if !b.trim().is_empty() => Some(b.trim().to_string()),
        _ => crate::worktree::git(root, &["rev-parse", "--abbrev-ref", "HEAD"])
            .ok()
            .map(|b| b.trim().to_string())
            .filter(|b| !b.is_empty()),
    };
    // unborn branch → no history; that's not an error for the panel
    let log_out = crate::worktree::git(
        root,
        &["log", "--pretty=format:%h%x1f%s%x1f%an%x1f%at", "-n", "30"],
    )
    .unwrap_or_default();
    let log = log_out
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let mut it = l.split('\x1f');
            GitCommit {
                hash: it.next().unwrap_or("").to_string(),
                subject: it.next().unwrap_or("").to_string(),
                author: it.next().unwrap_or("").to_string(),
                ts: it.next().and_then(|t| t.parse().ok()).unwrap_or(0),
            }
        })
        .collect();
    // no upstream is the normal case for local repos — counts stay None
    let (ahead, behind) = match crate::worktree::git(
        root,
        &["rev-list", "--left-right", "--count", "HEAD...@{upstream}"],
    ) {
        Ok(s) => {
            let mut it = s.split_whitespace();
            (
                it.next().and_then(|v| v.parse().ok()),
                it.next().and_then(|v| v.parse().ok()),
            )
        }
        Err(_) => (None, None),
    };
    Ok(GitOverview {
        repo: true,
        branch,
        ahead,
        behind,
        staged,
        unstaged,
        untracked,
        log,
    })
}

fn cap_diff(out: String) -> String {
    if out.chars().count() > DIFF_CAP {
        let cut: String = out.chars().take(DIFF_CAP).collect();
        return format!("{cut}\n…[diff 已截断]");
    }
    out
}

// ---------- commands ----------

/// Full panel payload for a workspace: repo flag, branch, upstream counts,
/// staged/unstaged/untracked lists and the recent commit log.
#[tauri::command]
pub fn git_overview(workspace: String) -> Result<GitOverview, String> {
    if !crate::worktree::is_git_repo(&workspace) {
        return Ok(GitOverview {
            repo: false,
            branch: None,
            ahead: None,
            behind: None,
            staged: Vec::new(),
            unstaged: Vec::new(),
            untracked: Vec::new(),
            log: Vec::new(),
        });
    }
    overview_impl(Path::new(&workspace))
}

/// Stage the given workspace-relative paths in one `git add` invocation.
#[tauri::command]
pub fn git_stage(workspace: String, paths: Vec<String>) -> Result<(), String> {
    if paths.is_empty() {
        return Err("没有要暂存的文件".into());
    }
    let mut args: Vec<&str> = vec!["add", "--"];
    args.extend(paths.iter().map(|s| s.as_str()));
    crate::worktree::git(Path::new(&workspace), &args).map(|_| ())
}

/// Stage every change including untracked files (git add -A).
#[tauri::command]
pub fn git_stage_all(workspace: String) -> Result<(), String> {
    crate::worktree::git(Path::new(&workspace), &["add", "-A", "--"]).map(|_| ())
}

/// Unstage one path. `git reset HEAD --` fails before the first commit
/// (unborn HEAD) — fall back to removing it from the index instead.
#[tauri::command]
pub fn git_unstage(workspace: String, path: String) -> Result<(), String> {
    let root = Path::new(&workspace);
    if crate::worktree::git(root, &["reset", "-q", "HEAD", "--", &path]).is_ok() {
        return Ok(());
    }
    crate::worktree::git(root, &["rm", "--cached", "-q", "--", &path]).map(|_| ())
}

/// Throw away the working-tree changes of one tracked file (the frontend
/// confirms before calling — this cannot be undone).
#[tauri::command]
pub fn git_discard(workspace: String, path: String) -> Result<(), String> {
    crate::worktree::git(Path::new(&workspace), &["checkout", "-q", "--", &path]).map(|_| ())
}

/// Commit whatever is staged with the given message; returns the new short hash.
#[tauri::command]
pub fn git_commit(workspace: String, message: String) -> Result<String, String> {
    let root = Path::new(&workspace);
    if message.trim().is_empty() {
        return Err("提交信息不能为空".into());
    }
    crate::worktree::git(root, &["commit", "-m", &message])?;
    crate::worktree::git(root, &["rev-parse", "--short", "HEAD"])
}

/// Combined diff (staged + unstaged vs HEAD) of one file; the frontend
/// colors the lines. Untracked files get a synthesized all-additions diff
/// from their content (git diff shows nothing for them).
#[tauri::command]
pub fn git_file_diff(workspace: String, path: String) -> Result<String, String> {
    let root = Path::new(&workspace);
    let st = crate::worktree::git(root, &["status", "--porcelain", "-uall", "--", &path])?;
    if st.lines().any(|l| l.starts_with("??")) {
        let content = std::fs::read_to_string(root.join(&path))
            .map_err(|e| format!("读取新文件失败: {e}"))?;
        let mut out = format!("--- /dev/null\n+++ b/{path}\n@@ 新文件 @@\n");
        for line in content.lines() {
            out.push_str(&format!("+{line}\n"));
        }
        return Ok(cap_diff(out));
    }
    // `git diff HEAD` fails on an unborn branch — fall back to index-vs-worktree
    let out = match crate::worktree::git(root, &["diff", "--no-color", "HEAD", "--", &path]) {
        Ok(o) if !o.trim().is_empty() => o,
        _ => crate::worktree::git(root, &["diff", "--no-color", "--", &path])?,
    };
    Ok(cap_diff(out))
}

/// Local branches with the checked-out one flagged.
#[tauri::command]
pub fn git_branches(workspace: String) -> Result<Vec<GitBranch>, String> {
    let out = crate::worktree::git(
        Path::new(&workspace),
        // NB: for-each-ref formats do NOT understand %xNN escapes (that's a
        // git-log pretty feature) — so no separator is usable here. Instead:
        // %(HEAD) renders "*" for the checked-out branch and " " otherwise
        // (trimmed away), and "*" is never part of a legal refname.
        &["branch", "--format=%(refname:short)%(HEAD)"],
    )?;
    Ok(out
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let l = l.trim();
            match l.strip_suffix('*') {
                Some(name) => GitBranch { name: name.trim().to_string(), current: true },
                None => GitBranch { name: l.to_string(), current: false },
            }
        })
        .collect())
}

/// Check out a local branch (the panel only offers names from git_branches;
/// git itself refuses anything unsafe, e.g. a branch held by a worktree).
#[tauri::command]
pub fn git_switch(workspace: String, name: String) -> Result<(), String> {
    sane_token("分支名", name.trim(), false)?;
    crate::worktree::git(Path::new(&workspace), &["checkout", "-q", name.trim()]).map(|_| ())
}

// ---------- remote repositories (push/pull/fetch + remote management) ----------

#[derive(Serialize, Debug)]
pub struct GitRemote {
    pub name: String,
    /// Fetch URL (push URL is almost always identical; one row per remote).
    pub url: String,
}

/// Configured remotes of the workspace repo (parsed from `git remote -v`).
#[tauri::command]
pub fn git_remotes(workspace: String) -> Result<Vec<GitRemote>, String> {
    let out = crate::worktree::git(Path::new(&workspace), &["remote", "-v"])?;
    let mut seen = std::collections::HashSet::new();
    let mut remotes = Vec::new();
    for line in out.lines() {
        let Some((name, rest)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let Some(url) = rest.trim().strip_suffix(" (fetch)") else {
            continue;
        };
        if seen.insert(name.to_string()) {
            remotes.push(GitRemote { name: name.to_string(), url: url.trim().to_string() });
        }
    }
    Ok(remotes)
}

/// Refuse option-looking operands: a renderer-compromised value starting
/// with `-` would be parsed as a git FLAG, not a name (P2 hardening).
/// Remote names additionally get a strict charset (git ref/remote rules).
fn sane_token(kind: &str, v: &str, strict: bool) -> Result<(), String> {
    if v.is_empty() || v.starts_with('-') {
        return Err(format!("非法{kind}: {v}"));
    }
    if strict && !v.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-') {
        return Err(format!("非法{kind}（仅允许字母数字 . _ -）: {v}"));
    }
    Ok(())
}

/// Register a remote (name + URL). Git rejects duplicate names — the error
/// surfaces so the user can remove the old one or pick another name.
#[tauri::command]
pub fn git_remote_add(workspace: String, name: String, url: String) -> Result<(), String> {
    let name = name.trim();
    let url = url.trim();
    if name.is_empty() || url.is_empty() {
        return Err("远程名称和 URL 都不能为空".into());
    }
    sane_token("远程名", name, true)?;
    sane_token("URL", url, false)?;
    crate::worktree::git(Path::new(&workspace), &["remote", "add", name, url]).map(|_| ())
}

/// Remove a configured remote (local files are untouched).
#[tauri::command]
pub fn git_remote_remove(workspace: String, name: String) -> Result<(), String> {
    sane_token("远程名", name.trim(), true)?;
    crate::worktree::git(Path::new(&workspace), &["remote", "remove", name.trim()]).map(|_| ())
}

/// Push the current branch to a remote. `set_upstream` (-u) links the local
/// branch to its remote counterpart on the first push so later pull/fetch
/// and the ahead/behind counters work. Network op → long deadline.
#[tauri::command]
pub fn git_push(
    workspace: String,
    remote: String,
    branch: String,
    set_upstream: bool,
) -> Result<String, String> {
    sane_token("远程名", remote.trim(), true)?;
    sane_token("分支名", branch.trim(), false)?;
    let remote = remote.trim();
    let branch = branch.trim();
    let mut args: Vec<&str> = vec!["push"];
    if set_upstream {
        args.push("-u");
    }
    args.push(remote);
    args.push(branch);
    crate::worktree::git_net(Path::new(&workspace), &args)
}

/// Pull (fetch + merge) the current branch's upstream. Network op → long
/// deadline. A missing upstream gets a hint pointing at the push button.
#[tauri::command]
pub fn git_pull(workspace: String) -> Result<String, String> {
    match crate::worktree::git_net(Path::new(&workspace), &["pull"]) {
        Ok(out) => Ok(out),
        Err(e) if e.contains("no tracking information") => Err(format!(
            "{e}\n提示：当前分支尚未关联远程分支 —— 先「推送」一次（首次推送自动关联），再回来拉取"
        )),
        Err(e) => Err(e),
    }
}

/// Fetch all remotes (with prune) so the ahead/behind counters refresh.
/// Network op → long deadline.
#[tauri::command]
pub fn git_fetch(workspace: String) -> Result<String, String> {
    crate::worktree::git_net(Path::new(&workspace), &["fetch", "--all", "--prune"])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a throwaway git repo with one empty commit; returns its path.
    fn seed_repo(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cch_gitp_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let g = |args: &[&str]| {
            crate::worktree::git(&dir, args).unwrap_or_else(|e| panic!("git {args:?}: {e}"))
        };
        g(&["init", "-q"]);
        // repo-local identity — the host machine may have none configured
        // (git commit refuses to run without one)
        g(&["config", "user.name", "t"]);
        g(&["config", "user.email", "t@t"]);
        g(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "--allow-empty", "-q", "-m", "init"]);
        dir
    }

    #[test]
    fn status_parsing_groups_by_column() {
        let dir = seed_repo("parse");
        let ws = dir.to_str().unwrap();
        std::fs::write(dir.join("mod.txt"), "v2\n").unwrap();
        std::fs::write(dir.join("new.txt"), "n\n").unwrap();
        crate::worktree::git(&dir, &["add", "mod.txt"]).unwrap();
        let out = crate::worktree::git(Path::new(ws), &["status", "--porcelain", "-uall"]).unwrap();
        let (staged, unstaged, untracked) = parse_status(&out);
        // mod.txt is new to the (empty) HEAD → staged as "A "
        assert_eq!(staged.len(), 1, "{staged:?}");
        assert_eq!(staged[0].status, "A");
        assert_eq!(staged[0].path, "mod.txt");
        assert!(unstaged.is_empty(), "{unstaged:?}");
        assert_eq!(untracked.len(), 1);
        assert_eq!(untracked[0].status, "?");
        assert_eq!(untracked[0].path, "new.txt");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn overview_stage_commit_log_roundtrip() {
        let dir = seed_repo("flow");
        let ws = dir.to_str().unwrap();
        std::fs::write(dir.join("a.txt"), "hello\n").unwrap();
        let ov = overview_impl(Path::new(ws)).unwrap();
        assert!(ov.repo);
        assert!(ov.branch.is_some(), "branch should be detected");
        assert_eq!(ov.untracked.len(), 1);

        git_stage(ws.to_string(), vec!["a.txt".to_string()]).unwrap();
        let ov = overview_impl(Path::new(ws)).unwrap();
        assert_eq!(ov.staged.len(), 1);

        let hash = git_commit(ws.to_string(), "add a.txt".to_string()).unwrap();
        assert!(!hash.is_empty());
        let ov = overview_impl(Path::new(ws)).unwrap();
        assert!(ov.staged.is_empty() && ov.unstaged.is_empty() && ov.untracked.is_empty());
        assert_eq!(ov.log.len(), 2, "{:?}", ov.log);
        assert_eq!(ov.log[0].subject, "add a.txt");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unstage_discard_diff_and_branches() {
        let dir = seed_repo("edges");
        let ws = dir.to_str().unwrap();
        std::fs::write(dir.join("f.txt"), "one\n").unwrap();
        git_stage(ws.to_string(), vec!["f.txt".to_string()]).unwrap();
        git_commit(ws.to_string(), "c1".to_string()).unwrap();

        // stage a new file, then unstage it back to untracked
        std::fs::write(dir.join("g.txt"), "two\n").unwrap();
        git_stage(ws.to_string(), vec!["g.txt".to_string()]).unwrap();
        git_unstage(ws.to_string(), "g.txt".to_string()).unwrap();
        let ov = overview_impl(Path::new(ws)).unwrap();
        assert_eq!(ov.untracked.len(), 1, "{:?}", ov.untracked);

        // untracked file diff → synthesized all-additions patch
        let d = git_file_diff(ws.to_string(), "g.txt".to_string()).unwrap();
        assert!(d.contains("+two"), "{d}");

        // modify a tracked file: diff shows the change; discard restores it
        std::fs::write(dir.join("f.txt"), "one\ntwo\n").unwrap();
        let d = git_file_diff(ws.to_string(), "f.txt".to_string()).unwrap();
        assert!(d.contains("+two"), "{d}");
        git_discard(ws.to_string(), "f.txt".to_string()).unwrap();
        // core.autocrlf may restore the file with CRLF — compare content only
        let restored = std::fs::read_to_string(dir.join("f.txt")).unwrap().replace("\r\n", "\n");
        assert_eq!(restored, "one\n");

        // branches list marks the current one; switching moves the marker
        crate::worktree::git(Path::new(ws), &["branch", "feature"]).unwrap();
        let brs = git_branches(ws.to_string()).unwrap();
        assert_eq!(brs.len(), 2, "{brs:?}");
        let cur = brs.iter().find(|b| b.current).unwrap();
        assert_ne!(cur.name, "feature");
        git_switch(ws.to_string(), "feature".to_string()).unwrap();
        let brs = git_branches(ws.to_string()).unwrap();
        assert!(brs.iter().find(|b| b.name == "feature").unwrap().current);
        assert_eq!(brs.iter().filter(|b| b.current).count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Bare repo usable as a local-path remote (local transport, no network).
    fn seed_bare(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cch_gitb_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        crate::worktree::git(&dir, &["init", "-q", "--bare"])
            .unwrap_or_else(|e| panic!("bare init: {e}"));
        dir
    }

    #[test]
    fn remote_add_list_push_pull_roundtrip() {
        let bare = seed_bare("remote");
        let bare_url = bare.to_str().unwrap().to_string();

        // add + list + duplicate-rejection + remove
        let dir = seed_repo("remotes");
        let ws = dir.to_str().unwrap().to_string();
        assert!(git_remotes(ws.clone()).unwrap().is_empty());
        git_remote_add(ws.clone(), "origin".into(), bare_url.clone()).unwrap();
        assert!(git_remote_add(ws.clone(), "origin".into(), bare_url.clone()).is_err());
        let rms = git_remotes(ws.clone()).unwrap();
        assert_eq!(rms.len(), 1, "{rms:?}");
        assert_eq!(rms[0].name, "origin");
        assert_eq!(rms[0].url, bare_url);
        git_remote_remove(ws.clone(), "origin".into()).unwrap();
        assert!(git_remotes(ws.clone()).unwrap().is_empty());

        // push with upstream → bare holds the same commit
        git_remote_add(ws.clone(), "origin".into(), bare_url.clone()).unwrap();
        std::fs::write(dir.join("shared.txt"), "v1\n").unwrap();
        git_stage(ws.clone(), vec!["shared.txt".into()]).unwrap();
        git_commit(ws.clone(), "add shared".into()).unwrap();
        let branch = git_branches(ws.clone())
            .unwrap()
            .into_iter()
            .find(|b| b.current)
            .unwrap()
            .name;
        git_push(ws.clone(), "origin".into(), branch.clone(), true).unwrap();
        let bare_head = crate::worktree::git(&bare, &["rev-parse", &branch]).unwrap();
        let ws_head = crate::worktree::git(Path::new(&ws), &["rev-parse", &branch]).unwrap();
        assert_eq!(bare_head, ws_head);

        // fetch refreshes remote refs without touching the working tree
        git_fetch(ws.clone()).unwrap();
        assert!(dir.join("peer.txt").exists() == false);

        // a second clone moves the remote forward; pulling brings it in
        let clone_dir = std::env::temp_dir().join(format!("cch_gitc_remote_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&clone_dir);
        crate::worktree::git(
            &std::env::temp_dir(),
            &["clone", "-q", &bare_url, clone_dir.to_str().unwrap()],
        )
        .unwrap();
        crate::worktree::git(&clone_dir, &["config", "user.name", "t"]).unwrap();
        crate::worktree::git(&clone_dir, &["config", "user.email", "t@t"]).unwrap();
        std::fs::write(clone_dir.join("peer.txt"), "from peer\n").unwrap();
        crate::worktree::git(&clone_dir, &["add", "-A"]).unwrap();
        crate::worktree::git(&clone_dir, &["commit", "-qm", "peer commit"]).unwrap();
        crate::worktree::git(&clone_dir, &["push", "-q", "origin", &branch]).unwrap();
        git_pull(ws.clone()).unwrap();
        // autocrlf may check the file out with CRLF — compare content only
        let pulled = std::fs::read_to_string(dir.join("peer.txt"))
            .unwrap()
            .replace("\r\n", "\n");
        assert_eq!(pulled, "from peer\n");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&bare);
        let _ = std::fs::remove_dir_all(&clone_dir);
    }
}
