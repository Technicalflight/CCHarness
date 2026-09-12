// Right-side preview panel — the third column of the app.
// Two tabs:
//   文件   workspace file explorer + read-only code viewer (line numbers,
//          lightweight syntax highlighting) reusing the agent-tool path guards
//   浏览器 embedded web preview (iframe) for locally running dev servers,
//          with URL bar, quick presets and open-in-system-browser
// The panel is toggled from the slim right rail; state lives in the store so
// any view (tool cards, review diffs) can pop a file open programmatically.
import { useEffect, useRef, useMemo, useState } from "react";
import { useApp, type PreviewTab } from "../store";
import * as api from "../lib/api";
import type { WorkspaceEntry } from "../lib/api";
import { Icon } from "../lib/icons";
import { GitTab } from "./GitTab";

// ---------- lightweight syntax highlighting ----------

const KEYWORDS =
  "abstract|as|async|await|break|case|catch|class|const|continue|debugger|declare|default|delete|do|else|enum|export|extends|finally|fn|for|from|function|get|if|impl|implements|import|in|instanceof|interface|is|let|loop|match|mod|mut|new|of|package|private|protected|pub|public|readonly|return|self|set|static|struct|super|switch|this|throw|trait|try|type|typeof|use|var|void|while|with|yield|true|false|null|undefined|None|Some|Ok|Err";

type Lang = "c-like" | "hash" | "none";

function langOf(file: string): Lang {
  if (/\.(json|css|scss|html|xml|md|toml|ya?ml)$/i.test(file)) return "none";
  if (/\.(py|sh|bash|rb|yml|yaml|toml|ini|cfg)$/i.test(file)) return "hash";
  return "c-like"; // js/ts/tsx/jsx/rs/go/java/c/cpp/…
}

function escapeHtml(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

/** One-pass tokenizer: comments → strings → keywords/numbers/functions. */
function highlight(code: string, file: string): string {
  const lang = langOf(file);
  if (lang === "none") return escapeHtml(code);
  const lineComment = lang === "hash" ? "#" : "\\/\\/";
  const re = new RegExp(
    `(${lineComment}.*$|\\/\\*[\\s\\S]*?\\*\\/)` + // 1 comment
      `|("(?:[^"\\\\\\n]|\\\\.)*"|'(?:[^'\\\\\\n]|\\\\.)*'|\`(?:[^\`\\\\]|\\\\.)*\`)` + // 2 string
      `|\\b(${KEYWORDS})\\b` + // 3 keyword
      `|\\b(0x[0-9a-fA-F]+|\\d+(?:\\.\\d+)?)\\b` + // 4 number
      `|([A-Za-z_$][\\w$]*)(?=\\s*\\()`, // 5 call
    "gm"
  );
  return escapeHtml(code).replace(re, (m, c, s, kw, num, fn) => {
    if (c) return `<span class="tok-c">${m}</span>`;
    if (s) return `<span class="tok-s">${m}</span>`;
    if (kw) return `<span class="tok-k">${m}</span>`;
    if (num) return `<span class="tok-n">${m}</span>`;
    if (fn) return `<span class="tok-f">${m}</span>`;
    return m;
  });
}

// ---------- files tab ----------

function fmtSize(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
}

function FilesTab({
  workspace,
  initialFile,
  onInitialFileConsumed,
}: {
  workspace: string;
  initialFile: string | null;
  onInitialFileConsumed: () => void;
}) {
  const [dir, setDir] = useState("");
  const [entries, setEntries] = useState<WorkspaceEntry[] | null>(null);
  const [file, setFile] = useState<string | null>(null);
  const [content, setContent] = useState("");
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(false);
  const consumedRef = useRef<string | null>(null);
  const fsVersion = useApp((s) => s.fsVersion);

  const loadDir = async (rel: string) => {
    setLoading(true);
    setError("");
    try {
      setEntries(await api.listWorkspaceDir(workspace, rel));
    } catch (e) {
      setEntries([]);
      setError(String(e));
    } finally {
      setLoading(false);
    }
  };

  const openFile = async (rel: string) => {
    setLoading(true);
    setError("");
    try {
      setContent(await api.readWorkspaceFile(workspace, rel));
      setFile(rel);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    setFile(null);
    setDir("");
    void loadDir("");
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [workspace]);

  // a write-tool landed somewhere in the workspace — re-read in place with no
  // loading flash; if the open file was deleted, fall back to the directory
  useEffect(() => {
    if (fsVersion === 0) return;
    if (file) {
      void (async () => {
        try {
          setContent(await api.readWorkspaceFile(workspace, file));
        } catch {
          setFile(null);
          void loadDir(dir);
        }
      })();
    } else {
      void loadDir(dir);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [fsVersion]);

  // a file pushed from elsewhere (tool card / diff) opens on arrival
  useEffect(() => {
    if (initialFile && initialFile !== consumedRef.current) {
      consumedRef.current = initialFile;
      void openFile(initialFile);
      onInitialFileConsumed();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [initialFile]);

  const crumbs = dir ? dir.split("/") : [];

  if (file) {
    const lines = content.split("\n");
    return (
      <div className="pv-code">
        <div className="pv-code-bar">
          <button className="pv-ib" title="返回目录" onClick={() => setFile(null)}>
            <Icon name="chevronRight" size={14} style={{ transform: "rotate(180deg)" }} />
          </button>
          <span className="pv-code-path" title={file}>
            {file}
          </span>
          <button
            className="pv-ib"
            title="复制内容"
            onClick={() => void navigator.clipboard.writeText(content)}
          >
            <Icon name="copy" size={14} />
          </button>
        </div>
        <div className="pv-code-body">
          <div className="pv-gutter">
            {lines.map((_, i) => (
              <div key={i}>{i + 1}</div>
            ))}
          </div>
          <pre className="pv-pre">
            <code dangerouslySetInnerHTML={{ __html: highlight(content, file) }} />
          </pre>
        </div>
      </div>
    );
  }

  return (
    <div className="pv-files">
      <div className="pv-crumbs">
        <button className={`pv-crumb ${dir === "" ? "on" : ""}`} onClick={() => void loadDir("")}>
          <Icon name="folder" size={12} /> 根目录
        </button>
        {crumbs.map((c, i) => (
          <span key={i} className="pv-crumb-wrap">
            <Icon name="chevronRight" size={11} />
            <button
              className={`pv-crumb ${i === crumbs.length - 1 ? "on" : ""}`}
              onClick={() => void loadDir(crumbs.slice(0, i + 1).join("/"))}
            >
              {c}
            </button>
          </span>
        ))}
        <span style={{ flex: 1 }} />
        <button className="pv-ib" title="刷新" onClick={() => void loadDir(dir)}>
          <Icon name="refresh" size={13} />
        </button>
      </div>
      {error && <div className="pv-empty">加载失败：{error}</div>}
      {entries && entries.length === 0 && !error && <div className="pv-empty">空目录</div>}
      <div className="pv-list">
        {entries?.map((e) => (
          <button
            key={e.name}
            className="pv-item"
            onClick={() => {
              const rel = dir ? `${dir}/${e.name}` : e.name;
              if (e.is_dir) {
                setDir(rel);
                void loadDir(rel);
              } else {
                void openFile(rel);
              }
            }}
          >
            <Icon name={e.is_dir ? "folder" : "edit"} size={14} />
            <span className="pv-item-name">{e.name}</span>
            <span className="pv-item-meta">{e.is_dir ? "" : fmtSize(e.size)}</span>
          </button>
        ))}
      </div>
      {loading && <div className="pv-loading">加载中…</div>}
    </div>
  );
}

// ---------- browser tab ----------

const PRESETS = [
  { label: "Vite · 5173", url: "http://localhost:5173" },
  { label: "Node · 3000", url: "http://localhost:3000" },
  { label: "Flask · 5000", url: "http://localhost:5000" },
  { label: "FastAPI · 8000", url: "http://localhost:8000" },
  { label: "其他 · 8080", url: "http://localhost:8080" },
];

function BrowserTab() {
  const toast = useApp((s) => s.toast);
  const [input, setInput] = useState("");
  const [url, setUrl] = useState("");
  const [reloadKey, setReloadKey] = useState(0);
  // mobile-viewport simulation: the SAME iframe node gets re-laid-out into a
  // phone frame (CSS class swap → no page reload), with a device bezel,
  // notch decoration and the page rendered at a phone's viewport width
  const [device, setDevice] = useState(() => localStorage.getItem("cc.pvDevice") === "1");

  const toggleDevice = () => {
    setDevice((d) => {
      localStorage.setItem("cc.pvDevice", d ? "0" : "1");
      return !d;
    });
  };

  const go = (u: string) => {
    let v = u.trim();
    if (!v) return;
    if (!/^https?:\/\//i.test(v)) v = `http://${v}`;
    // sandbox hardening (P2): this panel is for LOCAL dev servers only. The
    // app's own origin (tauri.localhost) must never be framed — same-origin
    // content in the frame could reach the IPC surface.
    let host = "";
    try {
      host = new URL(v).hostname;
    } catch {
      toast("error", "非法地址");
      return;
    }
    if (host === location.hostname) {
      toast("error", "不能内嵌应用自身来源");
      return;
    }
    const local =
      host === "localhost" || host === "127.0.0.1" || host === "[::1]" || host.endsWith(".localhost");
    if (!local) {
      toast("error", "内嵌预览仅支持本地服务（localhost / 127.0.0.1）");
      return;
    }
    setInput(v);
    setUrl(v);
    setReloadKey((k) => k + 1);
  };

  return (
    <div className={`pv-web ${device ? "device" : ""}`}>
      <div className="pv-web-bar">
        <button className="pv-ib" title="刷新" onClick={() => setReloadKey((k) => k + 1)}>
          <Icon name="refresh" size={14} />
        </button>
        <input
          className="pv-url"
          placeholder="输入地址，如 localhost:5173"
          spellCheck={false}
          value={input}
          onChange={(e) => setInput(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") go(input);
          }}
        />
        <button
          className={`pv-ib ${device ? "on" : ""}`}
          title={device ? "切换为桌面视口" : "切换为手机视口（375×812 竖屏模拟）"}
          aria-pressed={device}
          onClick={toggleDevice}
        >
          <Icon name="smartphone" size={14} />
        </button>
        <button
          className="pv-ib"
          title="在系统浏览器打开"
          onClick={async () => {
            if (!url) return;
            try {
              await api.openExternal(url);
            } catch (e) {
              toast("error", `打开失败: ${String(e)}`);
            }
          }}
        >
          <Icon name="external" size={14} />
        </button>
      </div>
      {!url && (
        <div className="pv-web-start">
          <Icon name="globe" size={22} />
          <div className="pv-web-hint">在输入框输入本地服务的地址即可内嵌预览；以下为常用端口：</div>
          <div className="pv-presets">
            {PRESETS.map((p) => (
              <button key={p.url} className="pv-preset" onClick={() => go(p.url)}>
                {p.label}
              </button>
            ))}
          </div>
          <div className="pv-web-note">带 X-Frame-Options 限制的站点无法内嵌，可点右上角在系统浏览器打开。</div>
        </div>
      )}
      {url && (
        <iframe
          key={reloadKey}
          className="pv-frame"
          src={url}
          title="预览"
          // no allow-same-origin: framed pages run in an opaque origin, so
          // even a malicious local server cannot touch the app's IPC
          // surface (MDN flags scripts+same-origin together as unsafe)
          sandbox="allow-scripts allow-forms allow-popups"
        />
      )}
    </div>
  );
}

// ---------- tasks tab (todo_write tool + 任务 panel) ----------

const STATUS_META: Record<
  string,
  { icon: "checkSquare" | "square" | "bolt" | "check"; label: string; cls: string }
> = {
  pending: { icon: "square", label: "待办", cls: "t-pending" },
  in_progress: { icon: "bolt", label: "进行中", cls: "t-active" },
  done: { icon: "check", label: "已完成", cls: "t-done" },
};

function TasksTab({ sessionId }: { sessionId: string }) {
  const todos = useApp((s) => s.todos[sessionId]);
  const refreshTodos = useApp((s) => s.refreshTodos);
  const [loading, setLoading] = useState(false);

  const refresh = async () => {
    setLoading(true);
    await refreshTodos(sessionId);
    setLoading(false);
  };

  useEffect(() => {
    void refreshTodos(sessionId);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionId]);

  if (!todos || todos.length === 0) {
    return (
      <div className="pv-empty">
        <Icon name="checkSquare" size={22} />
        还没有任务清单 —— 让智能体处理长任务时，它会用 todo_write 工具把计划同步到这里
      </div>
    );
  }
  const done = todos.filter((t) => t.status === "done").length;
  return (
    <div className="pv-tasks">
      <div className="pv-tasks-head">
        <span>
          {done}/{todos.length} 已完成
        </span>
        <span style={{ flex: 1, height: 3, borderRadius: 2, background: "var(--bg3)", overflow: "hidden" }}>
          <span
            style={{
              display: "block",
              height: "100%",
              width: `${Math.round((done / todos.length) * 100)}%`,
              background: "var(--accent)",
              transition: "width 0.3s",
            }}
          />
        </span>
        <button className="pv-ib" title="刷新" onClick={() => void refresh()}>
          <Icon name="refresh" size={13} />
        </button>
      </div>
      <div className="pv-task-list">
        {todos.map((t, i) => {
          const meta = STATUS_META[t.status] ?? STATUS_META.pending;
          return (
            <div key={i} className={`pv-task ${meta.cls}`}>
              <Icon name={meta.icon} size={14} />
              <span className="pv-task-text" title={t.text}>
                {t.text}
              </span>
              <span className="pv-task-status">{meta.label}</span>
            </div>
          );
        })}
      </div>
      {loading && <div className="pv-loading">刷新中…</div>}
    </div>
  );
}

// ---------- panel shell ----------

const TABS: { id: PreviewTab; label: string; icon: "folder" | "branch" | "globe" | "checkSquare" }[] = [
  { id: "files", label: "文件", icon: "folder" },
  { id: "git", label: "Git", icon: "branch" },
  { id: "browser", label: "浏览器", icon: "globe" },
  { id: "tasks", label: "任务", icon: "checkSquare" },
];

export function PreviewPanel() {
  // per-field selectors: this panel is always mounted — a whole-store
  // subscription re-rendered it (and every tab inside) on each stream delta
  const previewOpen = useApp((s) => s.previewOpen);
  const previewTab = useApp((s) => s.previewTab);
  const previewFile = useApp((s) => s.previewFile);
  const setPreview = useApp((s) => s.setPreview);
  const setPanelW = useApp((s) => s.setPanelW);
  const sessions = useApp((s) => s.sessions);
  const activeSessionId = useApp((s) => s.activeSessionId);
  const panelW = useApp((s) => s.panelW);
  const workspace = useMemo(
    () => sessions.find((s) => s.id === activeSessionId)?.workspace ?? null,
    [sessions, activeSessionId]
  );
  // worktree isolation active for this session — the git tab manages the
  // main workspace, so flag it in the UI
  const wtActive = sessions.find((s) => s.id === activeSessionId)?.wt != null;

  // draggable splitter: the panel lives INSIDE the window as a real grid
  // column — the window itself never changes size. Drag left to widen
  // (clamped by setPanelW), release to persist.
  const drag = useRef<{ x: number; w: number } | null>(null);
  const onResizeStart = (e: React.MouseEvent) => {
    e.preventDefault();
    drag.current = { x: e.clientX, w: panelW };
    const move = (ev: MouseEvent) => {
      if (!drag.current) return;
      setPanelW(drag.current.w + (drag.current.x - ev.clientX));
    };
    const up = () => {
      drag.current = null;
      window.removeEventListener("mousemove", move);
      window.removeEventListener("mouseup", up);
    };
    window.addEventListener("mousemove", move);
    window.addEventListener("mouseup", up);
  };

  return (
    <>
      {/* rail — always visible on the right edge */}
      <div className="pv-rail">
        {TABS.map((t) => (
          <button
            key={t.id}
            className={`pv-rail-btn ${previewOpen && previewTab === t.id ? "on" : ""}`}
            title={t.label}
            onClick={() => setPreview({ open: !(previewOpen && previewTab === t.id), tab: t.id })}
          >
            <Icon name={t.icon} size={17} />
          </button>
        ))}
      </div>

      {previewOpen && (
        <aside className="preview-panel fade-in">
          <div className="pv-resizer" title="拖拽调整面板宽度" onMouseDown={onResizeStart} />
          <div className="pv-head">
            {TABS.map((t) => (
              <button
                key={t.id}
                className={`pv-tab ${previewTab === t.id ? "on" : ""}`}
                onClick={() => setPreview({ tab: t.id })}
              >
                <Icon name={t.icon} size={14} /> {t.label}
              </button>
            ))}
            <span style={{ flex: 1 }} />
            <button className="pv-ib" title="收起面板" onClick={() => setPreview({ open: false })}>
              <Icon name="chevronRight" size={15} />
            </button>
          </div>
          <div className="pv-body">
            {previewTab === "files" ? (
              workspace ? (
                <FilesTab
                  workspace={workspace}
                  initialFile={previewFile}
                  onInitialFileConsumed={() => setPreview({ file: null })}
                />
              ) : (
                <div className="pv-empty">
                  <Icon name="folder" size={22} />
                  当前会话未绑定工作区 —— 先在会话顶部绑定文件夹
                </div>
              )
            ) : previewTab === "git" ? (
              workspace ? (
                <GitTab workspace={workspace} wtActive={wtActive} />
              ) : (
                <div className="pv-empty">
                  <Icon name="branch" size={22} />
                  当前会话未绑定工作区 —— 先在会话顶部绑定文件夹
                </div>
              )
            ) : previewTab === "tasks" ? (
              activeSessionId ? (
                <TasksTab sessionId={activeSessionId} />
              ) : (
                <div className="pv-empty">
                  <Icon name="checkSquare" size={22} />
                  选择或创建一个会话后，这里会显示智能体的任务清单
                </div>
              )
            ) : (
              <BrowserTab />
            )}
          </div>
        </aside>
      )}
    </>
  );
}
