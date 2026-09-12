// Right-side panel Git tab — manage the session's main workspace repo:
// status lists (staged/unstaged/untracked), staging, commit, branch
// switching, per-file diff and recent history. All actions are explicit
// user clicks — nothing here runs on the model's behalf.
import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import { useApp } from "../store";
import * as api from "../lib/api";
import type { GitBranch, GitFile, GitOverview, GitRemote } from "../types";
import { Icon } from "../lib/icons";
import { ConfirmDialog } from "./Dialog";

const LT_CLS: Record<string, string> = {
  M: "lt-m",
  A: "lt-a",
  D: "lt-d",
  "?": "lt-q",
  R: "lt-r",
  C: "lt-a",
  T: "lt-m",
  U: "lt-u",
};

function ltCls(s: string): string {
  return LT_CLS[s] ?? "lt-m";
}

function relTime(ts: number): string {
  if (!ts) return "";
  const mins = Math.floor((Date.now() - ts * 1000) / 60000);
  if (mins < 1) return "刚刚";
  if (mins < 60) return `${mins} 分钟前`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return `${hours} 小时前`;
  const days = Math.floor(hours / 24);
  if (days < 30) return `${days} 天前`;
  return new Date(ts * 1000).toLocaleDateString();
}

/** One change row: status letter + path + hover actions. Click → diff. */
function FileRow({
  f,
  onOpen,
  actions,
}: {
  f: GitFile;
  onOpen: (path: string) => void;
  actions: ReactNode;
}) {
  return (
    <div className="pv-item git-row" onClick={() => onOpen(f.path)}>
      <span className={`git-lt ${ltCls(f.status)}`}>{f.status}</span>
      <span className="pv-item-name" title={f.path}>
        {f.path}
      </span>
      <span className="git-row-actions" onClick={(e) => e.stopPropagation()}>
        {actions}
      </span>
    </div>
  );
}

export function GitTab({ workspace, wtActive }: { workspace: string; wtActive: boolean }) {
  const toast = useApp((s) => s.toast);
  const fsVersion = useApp((s) => s.fsVersion);
  const [ov, setOv] = useState<GitOverview | null>(null);
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(false);
  const [branches, setBranches] = useState<GitBranch[]>([]);
  const [branchMenu, setBranchMenu] = useState(false);
  const [remotes, setRemotes] = useState<GitRemote[]>([]);
  const [remoteForm, setRemoteForm] = useState(false);
  const [rname, setRname] = useState("");
  const [rurl, setRurl] = useState("");
  const [syncing, setSyncing] = useState<string | null>(null);
  const [msg, setMsg] = useState("");
  const [committing, setCommitting] = useState(false);
  const [busyPath, setBusyPath] = useState<string | null>(null);
  const [diffPath, setDiffPath] = useState<string | null>(null);
  const [diffText, setDiffText] = useState("");
  const [diffLoading, setDiffLoading] = useState(false);
  // 自绘确认框（应用纪律：不调用浏览器 confirm）——挂起动作随状态一起存
  const [confirmAsk, setConfirmAsk] = useState<null | {
    title: string;
    description: string;
    danger?: boolean;
    action: () => void;
  }>(null);
  const menuRef = useRef<HTMLDivElement | null>(null);

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      setOv(await api.gitOverview(workspace));
      setError("");
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, [workspace]);

  const loadBranches = useCallback(async () => {
    try {
      setBranches(await api.gitBranches(workspace));
    } catch {
      setBranches([]);
    }
  }, [workspace]);

  const loadRemotes = useCallback(async () => {
    try {
      setRemotes(await api.gitRemotes(workspace));
    } catch {
      setRemotes([]);
    }
  }, [workspace]);

  useEffect(() => {
    setDiffPath(null);
    setBranchMenu(false);
    void refresh();
    void loadBranches();
    void loadRemotes();
  }, [refresh, loadBranches, loadRemotes]);

  // a write-tool landed somewhere in the workspace → re-read status silently
  useEffect(() => {
    if (fsVersion === 0) return;
    void refresh();
  }, [fsVersion, refresh]);

  // close the branch menu on outside clicks
  useEffect(() => {
    if (!branchMenu) return;
    const onDoc = (e: MouseEvent) => {
      if (menuRef.current && !menuRef.current.contains(e.target as Node)) {
        setBranchMenu(false);
      }
    };
    document.addEventListener("mousedown", onDoc);
    return () => document.removeEventListener("mousedown", onDoc);
  }, [branchMenu]);

  const run = async (label: string, fn: () => Promise<unknown>) => {
    try {
      await fn();
      await refresh();
    } catch (e) {
      toast("error", `${label}失败: ${String(e)}`);
    } finally {
      setBusyPath(null);
    }
  };

  const stage = (path: string) => {
    setBusyPath(path);
    void run("暂存", () => api.gitStage(workspace, [path]));
  };
  const stageEverything = () => void run("暂存全部", () => api.gitStageAll(workspace));
  const unstage = (path: string) => {
    setBusyPath(path);
    void run("取消暂存", () => api.gitUnstage(workspace, path));
  };
  const discard = (path: string) => {
    setConfirmAsk({
      title: "丢弃未提交改动",
      description: `「${path}」的未提交改动将被还原，此操作不可恢复。`,
      danger: true,
      action: () => {
        setBusyPath(path);
        void run("丢弃改动", () => api.gitDiscard(workspace, path));
      },
    });
  };
  const switchBranch = (name: string) => {
    setBranchMenu(false);
    if (name === ov?.branch) return;
    void run("切换分支", () => api.gitSwitch(workspace, name)).then(loadBranches);
  };

  // ---- remote sync (push/pull/fetch + remote management) ----

  /** origin if present, else the first configured remote. */
  const pickRemote = (): string | null =>
    remotes.find((r) => r.name === "origin")?.name ?? remotes[0]?.name ?? null;

  const needRemote = (): string | null => {
    const r = pickRemote();
    if (!r) toast("error", "尚未配置远程仓库 —— 先在下方添加");
    return r;
  };

  const sync = async (label: string, fn: () => Promise<unknown>) => {
    if (syncing) return;
    setSyncing(label);
    try {
      const res = await fn();
      // git prints short summaries ("Already up to date.", push refs) on stdout
      if (typeof res === "string" && res.trim() && res.trim().length <= 160) {
        toast("info", `${label}: ${res.trim().split("\n").pop()}`);
      } else {
        toast("success", `${label}完成`);
      }
      await refresh();
    } catch (e) {
      toast("error", `${label}失败: ${String(e)}`);
    } finally {
      setSyncing(null);
    }
  };

  const push = () => {
    const remote = needRemote();
    if (!remote) return;
    // no upstream detected (ahead/behind unknown) → link on first push (-u)
    void sync("推送", () => api.gitPush(workspace, remote, ov?.branch ?? "HEAD", ov?.ahead == null));
  };
  const pull = () => {
    if (!needRemote()) return;
    void sync("拉取", () => api.gitPull(workspace));
  };
  const fetchAll = () => {
    if (!needRemote()) return;
    void sync("获取", () => api.gitFetch(workspace));
  };

  const addRemote = async () => {
    const name = rname.trim() || "origin";
    const url = rurl.trim();
    if (!url) {
      toast("error", "请填写远程仓库 URL");
      return;
    }
    if (syncing) return;
    setSyncing("add");
    try {
      await api.gitRemoteAdd(workspace, name, url);
      setRname("");
      setRurl("");
      setRemoteForm(false);
      toast("success", `已连接远程 ${name}`);
      await loadRemotes();
    } catch (e) {
      toast("error", `添加远程失败: ${String(e)}`);
    } finally {
      setSyncing(null);
    }
  };

  const removeRemote = (name: string) => {
    setConfirmAsk({
      title: "移除远程仓库",
      description: `将移除远程「${name}」（本地文件不受影响）。`,
      danger: true,
      action: () => {
        if (syncing) return;
        setSyncing(name);
        void (async () => {
          try {
            await api.gitRemoteRemove(workspace, name);
            await loadRemotes();
          } catch (e) {
            toast("error", `移除远程失败: ${String(e)}`);
          } finally {
            setSyncing(null);
          }
        })();
      },
    });
  };

  const commit = async () => {
    const message = msg.trim();
    if (!message || !ov || ov.staged.length === 0 || committing) return;
    setCommitting(true);
    try {
      const hash = await api.gitCommit(workspace, message);
      setMsg("");
      toast("success", `已提交 ${hash}`);
      await refresh();
    } catch (e) {
      toast("error", `提交失败: ${String(e)}`);
    } finally {
      setCommitting(false);
    }
  };

  const openDiff = async (path: string) => {
    setDiffPath(path);
    setDiffText("");
    setDiffLoading(true);
    try {
      setDiffText(await api.gitFileDiff(workspace, path));
    } catch (e) {
      setDiffText(`加载失败: ${String(e)}`);
    } finally {
      setDiffLoading(false);
    }
  };

  if (!ov && !error) {
    return <div className="pv-empty">读取 git 状态中…</div>;
  }
  if (error && !ov) {
    return (
      <div className="pv-empty">
        <Icon name="branch" size={22} />
        读取 git 状态失败：{error}
      </div>
    );
  }
  if (ov && !ov.repo) {
    return (
      <div className="pv-empty">
        <Icon name="branch" size={22} />
        当前工作区不是 git 仓库 —— 在目录中执行 git init 并完成首次提交后即可使用
      </div>
    );
  }
  if (!ov) return null;

  // per-file diff viewer
  if (diffPath) {
    const lines = diffText.split("\n");
    return (
      <div className="git-diff">
        <div className="pv-code-bar">
          <button className="pv-ib" title="返回状态列表" onClick={() => setDiffPath(null)}>
            <Icon name="chevronRight" size={14} style={{ transform: "rotate(180deg)" }} />
          </button>
          <span className="pv-code-path" title={diffPath}>
            {diffPath}
          </span>
          <button className="pv-ib" title="刷新 diff" onClick={() => void openDiff(diffPath)}>
            <Icon name="refresh" size={14} />
          </button>
        </div>
        {diffLoading ? (
          <div className="pv-loading">加载中…</div>
        ) : (
          <pre className="git-diff-body">
            {lines.map((ln, i) => {
              const cls =
                ln.startsWith("+++") || ln.startsWith("---")
                  ? ""
                  : ln.startsWith("+")
                    ? "dl-add"
                    : ln.startsWith("-")
                      ? "dl-del"
                      : ln.startsWith("@@")
                        ? "dl-hunk"
                        : "";
              return (
                <div key={i} className={cls}>
                  {ln || " "}
                </div>
              );
            })}
          </pre>
        )}
      </div>
    );
  }

  const { staged, unstaged, untracked, log } = ov;
  const showAb = ov.ahead != null && (ov.ahead > 0 || (ov.behind ?? 0) > 0);
  const clean = staged.length === 0 && unstaged.length === 0 && untracked.length === 0;

  return (
    <div className="pv-git">
      <div className="git-top">
        <div className="git-branch-wrap" ref={menuRef}>
          <button
            className="git-branch-chip"
            title="切换分支"
            onClick={() => setBranchMenu((v) => !v)}
          >
            <Icon name="branch" size={13} />
            <span className="git-branch-name">{ov.branch ?? "HEAD"}</span>
            {showAb && (
              <span className="git-ab" title="领先/落后上游">
                ↑{ov.ahead} ↓{ov.behind ?? 0}
              </span>
            )}
          </button>
          {branchMenu && (
            <div className="git-branches">
              {branches.length === 0 && <div className="git-branches-empty">没有本地分支</div>}
              {branches.map((b) => (
                <button
                  key={b.name}
                  className={`git-branch-item ${b.current ? "on" : ""}`}
                  onClick={() => switchBranch(b.name)}
                >
                  {b.current && <Icon name="check" size={12} />}
                  <span className="git-branch-item-name">{b.name}</span>
                </button>
              ))}
            </div>
          )}
        </div>
        {wtActive && (
          <span
            className="git-wt-hint"
            title="会话已开启 worktree 隔离：智能体改动落在隔离分支，此处管理的是主工作区"
          >
            隔离中
          </span>
        )}
        <span style={{ flex: 1 }} />
        {loading && <span className="git-refreshing">刷新中…</span>}
        <button className="pv-ib" title="刷新" onClick={() => void refresh()}>
          <Icon name="refresh" size={13} />
        </button>
      </div>

      <div className="git-sec git-remotes">
        <div className="git-sec-title">
          远程仓库
          <span style={{ flex: 1 }} />
          {(syncing === "推送" || syncing === "拉取" || syncing === "获取") && (
            <span className="git-refreshing">{syncing}中…</span>
          )}
          {remotes.length > 0 && (
            <>
              <button className="git-mini-link" disabled={!!syncing} onClick={fetchAll}>
                获取
              </button>
              <button className="git-mini-link" disabled={!!syncing} onClick={pull}>
                拉取
              </button>
              <button className="git-mini-link" disabled={!!syncing} onClick={push}>
                推送
              </button>
            </>
          )}
        </div>
        {remotes.map((r) => (
          <div key={r.name} className="git-remote-row">
            <span className="git-remote-name">{r.name}</span>
            <span className="git-remote-url" title={r.url}>
              {r.url}
            </span>
            <span className="git-row-actions">
              <button
                className="pv-ib git-mini"
                title="移除远程"
                disabled={!!syncing}
                onClick={() => removeRemote(r.name)}
              >
                <Icon name="x" size={13} />
              </button>
            </span>
          </div>
        ))}
        {remoteForm ? (
          <div className="git-remote-form">
            <div className="git-remote-form-row">
              <input
                className="git-remote-in"
                placeholder="名称（默认 origin）"
                value={rname}
                spellCheck={false}
                onChange={(e) => setRname(e.target.value)}
              />
              <input
                className="git-remote-in git-remote-url-in"
                placeholder="https://github.com/... 或 git@..."
                value={rurl}
                spellCheck={false}
                onChange={(e) => setRurl(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") void addRemote();
                }}
              />
            </div>
            <div className="git-remote-form-row">
              <button className="git-commit-btn" disabled={!!syncing} onClick={() => void addRemote()}>
                连接
              </button>
              <button className="git-mini-link" onClick={() => setRemoteForm(false)}>
                取消
              </button>
              <span className="git-meta">连接后即可在右上角拉取 / 推送</span>
            </div>
          </div>
        ) : (
          <button className="git-mini-link git-remote-add" onClick={() => setRemoteForm(true)}>
            <Icon name="plus" size={12} /> 添加远程仓库
          </button>
        )}
      </div>

      {clean ? (
        <div className="git-clean">
          <Icon name="check" size={20} />
          工作区是干净的，没有未提交的改动
        </div>
      ) : (
        <>
          {staged.length > 0 && (
            <div className="git-sec">
              <div className="git-sec-title">已暂存 · {staged.length}</div>
              <div className="git-sec-list">
                {staged.map((f) => (
                  <FileRow
                    key={`s-${f.path}`}
                    f={f}
                    onOpen={(p) => void openDiff(p)}
                    actions={
                      <button
                        className="pv-ib git-mini"
                        title="取消暂存"
                        disabled={busyPath === f.path}
                        onClick={() => unstage(f.path)}
                      >
                        <Icon name="x" size={13} />
                      </button>
                    }
                  />
                ))}
              </div>
            </div>
          )}
          {(unstaged.length > 0 || untracked.length > 0) && (
            <div className="git-sec">
              <div className="git-sec-title">
                未暂存 · {unstaged.length + untracked.length}
                <span style={{ flex: 1 }} />
                <button className="git-mini-link" onClick={stageEverything}>
                  全部暂存
                </button>
              </div>
              <div className="git-sec-list">
                {unstaged.map((f) => (
                  <FileRow
                    key={`u-${f.path}`}
                    f={f}
                    onOpen={(p) => void openDiff(p)}
                    actions={
                      <>
                        <button
                          className="pv-ib git-mini"
                          title="暂存"
                          disabled={busyPath === f.path}
                          onClick={() => stage(f.path)}
                        >
                          <Icon name="plus" size={13} />
                        </button>
                        <button
                          className="pv-ib git-mini"
                          title="丢弃改动"
                          disabled={busyPath === f.path}
                          onClick={() => discard(f.path)}
                        >
                          <Icon name="trash" size={13} />
                        </button>
                      </>
                    }
                  />
                ))}
                {untracked.map((f) => (
                  <FileRow
                    key={`t-${f.path}`}
                    f={f}
                    onOpen={(p) => void openDiff(p)}
                    actions={
                      <button
                        className="pv-ib git-mini"
                        title="暂存"
                        disabled={busyPath === f.path}
                        onClick={() => stage(f.path)}
                      >
                        <Icon name="plus" size={13} />
                      </button>
                    }
                  />
                ))}
              </div>
            </div>
          )}
          <div className="git-commit">
            <textarea
              className="git-msg"
              rows={2}
              placeholder="提交信息…（Ctrl+Enter 提交）"
              value={msg}
              spellCheck={false}
              onChange={(e) => setMsg(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) void commit();
              }}
            />
            <button
              className="git-commit-btn"
              disabled={!msg.trim() || staged.length === 0 || committing}
              onClick={() => void commit()}
            >
              {committing ? "提交中…" : `提交 ${staged.length} 个文件`}
            </button>
          </div>
        </>
      )}

      {log.length > 0 && (
        <div className="git-sec git-log">
          <div className="git-sec-title">提交历史</div>
          <div className="git-sec-list">
            {log.map((c) => (
              <div
                key={c.hash}
                className="git-log-item"
                title={`${c.author} · ${new Date(c.ts * 1000).toLocaleString()}`}
              >
                <span className="git-hash">{c.hash}</span>
                <span className="git-subject">{c.subject}</span>
                <span className="git-meta">
                  {c.author} · {relTime(c.ts)}
                </span>
              </div>
            ))}
          </div>
        </div>
      )}
      {confirmAsk && (
        <ConfirmDialog
          title={confirmAsk.title}
          description={confirmAsk.description}
          danger={confirmAsk.danger}
          onCancel={() => setConfirmAsk(null)}
          onConfirm={() => {
            const act = confirmAsk.action;
            setConfirmAsk(null);
            act();
          }}
        />
      )}
    </div>
  );
}
