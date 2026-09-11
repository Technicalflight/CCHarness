// Git worktree isolation (loop-engineering "worktree isolation"): enabling
// isolation for a session creates a real git worktree on its own branch off
// the workspace's current HEAD. Every agent tool is then redirected into
// that worktree, and the user merges the reviewed diff back into the main
// checkout — or discards it wholesale. The main workspace stays untouched
// until an explicit merge, so autonomous turns and parallel experiments can
// never trample in-progress work.
//
// Design notes:
// - The worktree lives under <data_dir>/worktrees/<session_id>, never inside
//   the workspace, so tool scans and the file explorer stay clean.
// - Diffs/merges reference the *base commit* recorded at creation, not HEAD:
//   even if the model runs `git commit` inside the worktree, nothing is lost.
// - Merge uses `git apply` of the full diff (worktrees share the main repo's
//   object store, so no fetch is needed) — changes land as working-tree
//   edits the user can still inspect before committing. On any apply error
//   the worktree is kept: fail-closed, like every other surface here.

use crate::types_rs::WtState;
use std::path::Path;
use std::time::{Duration, Instant};

const GIT_TIMEOUT_SECS: u64 = 30;
/// Deadline for network operations (push/pull/fetch) — remote latency and
/// first-time credential prompts need far more headroom than local calls.
const GIT_NET_TIMEOUT_SECS: u64 = 300;
/// Diff text cap (chars) — neither the UI nor a human wants megabytes of patch.
const DIFF_CAP: usize = 160_000;

/// Run one git command in `cwd`, returning trimmed stdout on success.
/// Blocking with a hard kill deadline (same pattern as the run_command tool).
/// Also shared by the git management panel (git_panel module).
pub(crate) fn git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    git_deadline(cwd, args, GIT_TIMEOUT_SECS)
}

/// Network operations (push/pull/fetch) get a longer leash: large repos and
/// first-time credential prompts (Git Credential Manager window) can take
/// minutes while still being alive.
pub(crate) fn git_net(cwd: &Path, args: &[&str]) -> Result<String, String> {
    git_deadline(cwd, args, GIT_NET_TIMEOUT_SECS)
}

fn git_deadline(cwd: &Path, args: &[&str], secs: u64) -> Result<String, String> {
    let mut c = std::process::Command::new("git");
    c.args(args)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW — no console flash
    }
    let mut child = c
        .spawn()
        .map_err(|e| format!("无法启动 git（请确认已安装并加入 PATH）: {e}"))?;
    let out = child.stdout.take().expect("stdout piped");
    let err = child.stderr.take().expect("stderr piped");
    let t_out = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = std::io::Read::read_to_end(&mut std::io::BufReader::new(out), &mut b);
        b
    });
    let t_err = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = std::io::Read::read_to_end(&mut std::io::BufReader::new(err), &mut b);
        b
    });
    let deadline = Instant::now() + Duration::from_secs(secs);
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                return Err(format!("等待 git 退出失败: {e}"));
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("git 命令超时（{secs} 秒），进程已终止"));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stdout = String::from_utf8_lossy(&t_out.join().unwrap_or_default()).to_string();
    let stderr = String::from_utf8_lossy(&t_err.join().unwrap_or_default()).to_string();
    match status {
        Some(st) if st.success() => Ok(stdout.trim_end().to_string()),
        Some(st) => Err(format!(
            "git {} 失败（退出码 {}）：{}",
            args.first().copied().unwrap_or("?"),
            st.code().unwrap_or(-1),
            if stderr.trim().is_empty() { stdout.trim() } else { stderr.trim() }
        )),
        None => Err("git 命令被超时终止".into()),
    }
}

/// True when the directory is a git working tree (worktree add requires one).
pub fn is_git_repo(workspace: &str) -> bool {
    matches!(
        git(Path::new(workspace), &["rev-parse", "--is-inside-work-tree"]),
        Ok(out) if out.trim() == "true"
    )
}

/// True when the main workspace has no uncommitted changes — required to
/// enable isolation, so a later `git apply` can't collide with dirty files.
pub fn is_clean(workspace: &str) -> bool {
    matches!(
        git(Path::new(workspace), &["status", "--porcelain"]),
        Ok(out) if out.trim().is_empty()
    )
}

/// Create the session's worktree: a fresh branch off the workspace's current
/// HEAD, checked out under <data_dir>/worktrees/<session_id>.
pub fn create(workspace: &str, data_dir: &Path, session_id: &str) -> Result<WtState, String> {
    let root = Path::new(workspace);
    let base_head = git(root, &["rev-parse", "HEAD"])?.trim().to_string();
    let sid: String = session_id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(8).collect();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let branch = format!("cch-{sid}-{ts}");
    // the PATH component needs the same sanitizing as the branch label: a
    // raw session_id with separators/dots would aim the remove_dir_all
    // below OUTSIDE the worktrees root (P2 hardening; same discipline as
    // sessions::sanitize_id)
    let sid_path: String = session_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    let sid_path = if sid_path.is_empty() { "unknown".to_string() } else { sid_path };
    let dir = data_dir.join("worktrees").join(&sid_path);
    // a leftover from an earlier crash would make `worktree add` fail —
    // clear the directory and let git drop its stale registry entry first
    if dir.exists() {
        // belt & suspenders: never recurse-delete outside the worktrees root
        if !dir.starts_with(data_dir.join("worktrees")) {
            return Err("非法会话标识".into());
        }
        let _ = std::fs::remove_dir_all(&dir);
        let _ = git(root, &["worktree", "prune"]);
    }
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建 worktree 目录失败: {e}"))?;
    }
    git(
        root,
        &["worktree", "add", "-b", &branch, &dir.to_string_lossy(), "HEAD"],
    )
    .map_err(|e| format!("创建 worktree 失败: {e}"))?;
    Ok(WtState {
        path: dir.to_string_lossy().to_string(),
        branch,
        base_head,
        created_at: crate::sessions::now_ms(),
    })
}

/// Stage everything (including untracked files) so diffs see the full state.
fn stage_all(state: &WtState) -> Result<(), String> {
    git(Path::new(&state.path), &["add", "-A", "--"]).map(|_| ())
}

/// (status_letter, path) pairs of the worktree vs a clean checkout — the
/// capsule badge and merge summary. Porcelain status is read-only, so
/// untracked files show up without staging anything.
pub fn changed_files(state: &WtState) -> Result<Vec<(String, String)>, String> {
    let out = git(Path::new(&state.path), &["status", "--porcelain"])?;
    let mut files = Vec::new();
    for line in out.lines() {
        if line.chars().count() < 4 {
            continue;
        }
        let body: String = line.chars().skip(3).collect();
        // rename lines carry "old -> new" — report the destination
        let path = match body.split_once(" -> ") {
            Some((_old, new)) => new.to_string(),
            None => body,
        };
        let path = path.trim_matches('"').to_string();
        let letter = line
            .chars()
            .take(2)
            .find(|c| *c != ' ')
            .unwrap_or('M')
            .to_string();
        files.push((letter, path));
    }
    Ok(files)
}

/// Raw full patch of the isolation branch vs its base commit — NOT trimmed
/// (git apply requires every patch line, the last one included, to be
/// newline-terminated) and NOT capped (a truncated patch would be corrupt).
/// `binary` embeds binary deltas for `git apply`; the UI variant uses a
/// plain diff (binary files collapse to a "differ" line, no base85 noise).
fn diff_raw(state: &WtState, binary: bool) -> Result<String, String> {
    stage_all(state)?;
    let mut args: Vec<&str> = vec!["diff", "--no-color"];
    if binary {
        args.push("--binary");
    }
    args.push(state.base_head.as_str());
    let out = git(Path::new(&state.path), &args)?;
    // git() trims the trailing newline for command output ergonomics —
    // patches must get it back
    if out.is_empty() {
        Ok(out)
    } else {
        Ok(format!("{out}\n"))
    }
}

/// Diff text for the UI: plain (non-binary) patch, capped for display.
pub fn diff_text(state: &WtState) -> Result<String, String> {
    let out = diff_raw(state, false)?;
    if out.chars().count() > DIFF_CAP {
        let cut: String = out.chars().take(DIFF_CAP).collect();
        return Ok(format!("{cut}\n…[diff 已截断]"));
    }
    Ok(out)
}

/// Remove the worktree and its branch, tolerating pre-torn-down state
/// (dir already gone, stale registry entry, branch already deleted).
fn cleanup(state: &WtState, root: &Path) {
    let _ = git(root, &["worktree", "remove", "--force", &state.path]);
    let _ = std::fs::remove_dir_all(&state.path);
    let _ = git(root, &["worktree", "prune"]);
    let _ = git(root, &["branch", "-D", &state.branch]);
}

/// Apply the isolation branch's full diff back into the main workspace and
/// tear the worktree down. Fail-closed: an apply error leaves the worktree
/// (and the session's WtState) intact so nothing is lost.
pub fn merge(state: &WtState, main_ws: &str) -> Result<String, String> {
    let patch = diff_raw(state, true)?;
    let files = changed_files(state).unwrap_or_default();
    let root = Path::new(main_ws);
    if patch.trim().is_empty() {
        cleanup(state, root);
        return Ok("隔离分支没有任何改动 —— worktree 已清理".into());
    }
    // hand the patch over via a temp file: --binary patches can exceed any
    // comfortable argv limit (it lives next to the worktree, under data_dir)
    let patch_path = Path::new(&state.path)
        .parent()
        .ok_or("无法定位 worktree 目录")?
        .join("merge.patch");
    std::fs::write(&patch_path, &patch).map_err(|e| format!("写 patch 失败: {e}"))?;
    let patch_str = patch_path.to_string_lossy().to_string();
    let applied = git(root, &["apply", "--whitespace=nowarn", &patch_str]);
    let _ = std::fs::remove_file(&patch_path);
    applied.map_err(|e| {
        format!(
            "合并失败（主工作区未被改动）: {e} —— 主工作区可能有冲突的未提交改动，请提交或 stash 后重试"
        )
    })?;
    cleanup(state, root);
    Ok(format!(
        "已合并 {} 个文件的改动回主工作区（分支 {} 已清理）。改动以未提交形式落盘，可在主工作区检查后自行 commit",
        files.len(),
        state.branch
    ))
}

/// Discard the isolation branch and worktree — every change made inside the
/// worktree is thrown away (frontend confirms before calling).
pub fn discard(state: &WtState, main_ws: &str) -> Result<(), String> {
    cleanup(state, Path::new(main_ws));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a throwaway git repo with one commit; returns its path.
    fn seed_repo(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cch_wt_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.to_str().unwrap();
        git(&dir, &["init", "-q"]).unwrap();
        git(&dir, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "--allow-empty", "-q", "-m", "init"]).unwrap();
        std::fs::write(dir.join("base.txt"), "hello\n").unwrap();
        let _ = git(Path::new(p), &["add", "-A"]);
        git(&dir, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "base"]).unwrap();
        dir
    }

    /// git apply honors core.autocrlf — on Windows checkout-side files land
    /// with CRLF. Compare content, not line endings.
    fn norm(s: String) -> String {
        s.replace("\r\n", "\n")
    }

    #[test]
    fn repo_detection_and_cleanliness() {
        let dir = seed_repo("detect");
        let p = dir.to_str().unwrap();
        assert!(is_git_repo(p));
        assert!(is_clean(p));
        std::fs::write(dir.join("dirty.txt"), "x").unwrap();
        assert!(!is_clean(p));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!is_git_repo(p));
    }

    #[test]
    fn create_diff_merge_roundtrip() {
        let dir = seed_repo("merge");
        let ws = dir.to_str().unwrap().to_string();
        let data = dir.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let st = create(&ws, &data, "sess-test01").unwrap();
        assert!(Path::new(&st.path).is_dir());

        // change an existing file + add a new one inside the worktree
        std::fs::write(Path::new(&st.path).join("base.txt"), "hello\nworld\n").unwrap();
        std::fs::write(Path::new(&st.path).join("new.txt"), "added\n").unwrap();

        let files = changed_files(&st).unwrap();
        assert_eq!(files.len(), 2, "{files:?}");

        // the main workspace is untouched until merge
        assert_eq!(std::fs::read_to_string(dir.join("base.txt")).unwrap(), "hello\n");
        assert!(!dir.join("new.txt").exists());

        // merge lands both changes as working-tree edits and tears down
        let summary = merge(&st, &ws).unwrap();
        assert!(summary.contains("2 个文件"), "{summary}");
        assert_eq!(norm(std::fs::read_to_string(dir.join("base.txt")).unwrap()), "hello\nworld\n");
        assert_eq!(norm(std::fs::read_to_string(dir.join("new.txt")).unwrap()), "added\n");
        assert!(!Path::new(&st.path).exists());
        assert!(git(&dir, &["branch", "--list", &st.branch]).unwrap().trim().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_survives_model_commit_inside_worktree() {
        let dir = seed_repo("commit");
        let ws = dir.to_str().unwrap().to_string();
        let data = dir.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let st = create(&ws, &data, "sess-test02").unwrap();
        std::fs::write(Path::new(&st.path).join("base.txt"), "changed\n").unwrap();
        // a runaway `git commit` inside the worktree must not hide changes:
        // diffs reference the recorded base commit, not the moving HEAD
        git(Path::new(&st.path), &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-aqm", "sneaky"]).unwrap();
        let patch = diff_text(&st).unwrap();
        assert!(patch.contains("changed"), "{patch}");
        merge(&st, &ws).unwrap();
        assert_eq!(norm(std::fs::read_to_string(dir.join("base.txt")).unwrap()), "changed\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discard_throws_everything_away() {
        let dir = seed_repo("discard");
        let ws = dir.to_str().unwrap().to_string();
        let data = dir.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let st = create(&ws, &data, "sess-test03").unwrap();
        std::fs::write(Path::new(&st.path).join("junk.txt"), "x\n").unwrap();
        discard(&st, &ws).unwrap();
        assert!(!dir.join("junk.txt").exists());
        assert!(!Path::new(&st.path).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
