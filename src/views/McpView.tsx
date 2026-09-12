// MCP service management: stdio/http servers, connection status, trust.
// Tools discovered from enabled servers flow into the chat tool loop as
// mcp__<server_id>__<tool>; untrusted servers gate every call behind the
// approval card.
//
// Add/edit runs through one modal dialog (ServerDialog). The form is laid
// out in our own sectioned style (small-caps section titles + hairline
// rules): connection type as a segmented control, presets collapsed behind
// a toggle, and advanced knobs (cwd / call timeout / http headers) tucked
// into a collapsible so the default path stays short.
import { useEffect, useMemo, useState } from "react";
import { useApp } from "../store";
import * as api from "../lib/api";
import { Icon, type IconName } from "../lib/icons";
import { ConfirmDialog } from "../components/Dialog";
import type { McpServer, McpStatusEntry } from "../types";

/* ------------------------------------------------------------------ */
/* helpers                                                             */
/* ------------------------------------------------------------------ */

/** Parse an args line the way a shell would: whitespace splits, quotes keep
 * spaces inside one token. "写成一行，和 README 里一样。引号会被识别。" */
export function parseArgsLine(line: string): string[] {
  const out: string[] = [];
  let cur = "";
  let quote: '"' | "'" | null = null;
  let started = false;
  for (const ch of line) {
    if (quote) {
      if (ch === quote) quote = null;
      else cur += ch;
    } else if (ch === '"' || ch === "'") {
      quote = ch;
      started = true;
    } else if (/\s/.test(ch)) {
      if (cur || started) {
        out.push(cur);
        cur = "";
        started = false;
      }
    } else {
      cur += ch;
    }
  }
  if (cur || started) out.push(cur);
  return out;
}

/** Inverse of parseArgsLine for round-tripping args into an editable line. */
export function argsLine(args: string[]): string {
  return args
    .map((a) => (/\s/.test(a) ? `"${a.replace(/"/g, '\\"')}"` : a))
    .join(" ");
}

/** Identifier slug: lowercase ascii letters/digits/dash, non-empty. */
function slugify(name: string): string {
  const s = name
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "");
  return s || `server-${Math.random().toString(36).slice(2, 6)}`;
}

function blankServer(): McpServer {
  return {
    id: "",
    name: "",
    transport: "stdio",
    command: "",
    args: [],
    url: "",
    enabled: true,
    trusted: false,
    allow_local: false,
    env: {},
    description: "",
    cwd: "",
    timeout_secs: 0,
    headers: {},
  };
}

/* ------------------------------------------------------------------ */
/* presets — one click fills the whole stdio form                      */
/* ------------------------------------------------------------------ */

const PRESETS: { label: string; name: string; command: string; args: string; desc: string }[] = [
  {
    label: "Context7",
    name: "Context7",
    command: "npx",
    args: "-y @upstash/context7-mcp",
    desc: "获取最新的库文档",
  },
  {
    label: "filesystem",
    name: "filesystem",
    command: "npx",
    args: "-y @modelcontextprotocol/server-filesystem",
    desc: "工作区文件读写（启动参数需追加目录）",
  },
  {
    label: "fetch",
    name: "fetch",
    command: "uvx",
    args: "mcp-server-fetch",
    desc: "抓取网页并转为 Markdown",
  },
  {
    label: "sequential-thinking",
    name: "sequential-thinking",
    command: "npx",
    args: "-y @modelcontextprotocol/server-sequential-thinking",
    desc: "给模型一个结构化思考缓冲区",
  },
];

/* ------------------------------------------------------------------ */
/* dialog                                                              */
/* ------------------------------------------------------------------ */

type KV = { key: string; value: string };

function kvToRows(obj: Record<string, string> | undefined): KV[] {
  return Object.entries(obj ?? {}).map(([key, value]) => ({ key, value }));
}

function rowsToKv(rows: KV[]): Record<string, string> {
  const out: Record<string, string> = {};
  for (const r of rows) if (r.key.trim()) out[r.key.trim()] = r.value;
  return out;
}

function ServerDialog({
  initial,
  isNew,
  existingIds,
  onDone,
  onCancel,
}: {
  /** The server being edited, or a blank draft when adding. */
  initial: McpServer;
  isNew: boolean;
  existingIds: string[];
  onDone: (s: McpServer) => void;
  onCancel: () => void;
}) {
  const [draft, setDraft] = useState<McpServer>({
    ...initial,
    cwd: initial.cwd ?? "",
    timeout_secs: initial.timeout_secs ?? 0,
    headers: initial.headers ?? {},
  });
  const [argsText, setArgsText] = useState<string>(argsLine(initial.args));
  const [envRows, setEnvRows] = useState<KV[]>(() => kvToRows(initial.env));
  const [hdrRows, setHdrRows] = useState<KV[]>(() => kvToRows(initial.headers));
  const [showPresets, setShowPresets] = useState(false);
  const [idTouched, setIdTouched] = useState<boolean>(false);

  const set = (patch: Partial<McpServer>) => setDraft((d) => ({ ...d, ...patch }));

  // 名称 → 标识符 auto-fill (only while adding and the user hasn't edited it).
  const slug = useMemo(() => slugify(draft.name), [draft.name]);
  useEffect(() => {
    if (isNew && !idTouched && draft.name) set({ id: slug });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [slug]);

  const idError =
    !/^[a-z0-9][a-z0-9_-]*$/.test(draft.id)
      ? "只能用小写字母、数字、- 和 _"
      : existingIds.includes(draft.id)
        ? "该标识符已被其他服务占用"
        : null;
  const stdioMissing = draft.transport === "stdio" && !draft.command.trim();
  const httpMissing = draft.transport === "http" && !/^https?:\/\/.+/.test(draft.url.trim());
  const valid = draft.name.trim() && !idError && !stdioMissing && !httpMissing;

  const save = () => {
    if (!valid) return;
    onDone({
      ...draft,
      name: draft.name.trim(),
      args: parseArgsLine(argsText),
      env: rowsToKv(envRows),
      headers: rowsToKv(hdrRows),
      cwd: (draft.cwd ?? "").trim(),
      timeout_secs: Math.max(0, Math.floor(draft.timeout_secs ?? 0)),
    });
  };

  return (
    <div className="dialog-overlay" onMouseDown={(e) => e.target === e.currentTarget && onCancel()}>
      <div className="dialog wide mcp-dlg" role="dialog" aria-label={isNew ? "添加 MCP 服务器" : "编辑 MCP 服务器"}>
        <div className="dlg-head">
          <span className="dlg-title-ico"><Icon name="plug" size={16} /></span>
          <div style={{ flex: 1, minWidth: 0 }}>
            <h3>{isNew ? "添加 MCP 服务器" : "编辑 MCP 服务器"}</h3>
            <div className="dlg-sub">在本地运行一条命令，或调用一个 HTTP 端点 —— 工具以 mcp__标识符__名称 进入对话。</div>
          </div>
          <button className="dlg-x" onClick={onCancel} aria-label="关闭">
            <Icon name="x" size={15} />
          </button>
        </div>

        {isNew && (
          <div className="tpl-row">
            <button className="tpl-toggle" onClick={() => setShowPresets((o) => !o)}>
              <Icon name="spark" size={12} /> 从模板填充
              <span className="tpl-caret">{showPresets ? "▾" : "▸"}</span>
            </button>
            {showPresets && (
              <div className="preset-row">
                {PRESETS.map((p) => (
                  <button
                    key={p.label}
                    className="preset-chip"
                    title={`${p.command} ${p.args} —— ${p.desc}`}
                    onClick={() => {
                      setDraft((d) => ({
                        ...d,
                        name: p.name,
                        transport: "stdio",
                        command: p.command,
                        description: p.desc,
                      }));
                      setArgsText(p.args);
                    }}
                  >
                    {p.label}
                  </button>
                ))}
              </div>
            )}
          </div>
        )}

        <div className="dlg-sec">
          <div className="dlg-sec-title">连接方式</div>
          <div className="seg seg-mcp">
            <button
              className={`seg-btn ${draft.transport === "stdio" ? "on" : ""}`}
              onClick={() => set({ transport: "stdio" })}
            >
              <Icon name="terminal" size={13} /> 本地程序
            </button>
            <button
              className={`seg-btn ${draft.transport === "http" ? "on" : ""}`}
              onClick={() => set({ transport: "http" })}
            >
              <Icon name="globe" size={13} /> HTTP 地址
            </button>
          </div>
          <div className="sec-note">
            {draft.transport === "stdio" ? "应用在这台电脑上启动并管理子进程。" : "应用直接向端点发送 JSON-RPC 请求。"}
          </div>
        </div>

        <div className="dlg-sec">
          <div className="dlg-sec-title">身份</div>
          <div className="grid-2">
            <div className="field">
              <label>名称</label>
              <input
                value={draft.name}
                placeholder="如 Context7"
                onChange={(e) => set({ name: e.target.value })}
              />
              <div className="fld-hint">显示在服务列表里。</div>
            </div>
            <div className="field">
              <label>标识符</label>
              <input
                value={draft.id}
                placeholder="context7"
                spellCheck={false}
                disabled={!isNew}
                className={idError ? "invalid" : ""}
                onChange={(e) => {
                  setIdTouched(true);
                  set({ id: e.target.value });
                }}
              />
              <div className="fld-hint">
                {isNew ? "工具名前缀，保存后不能修改。" : `工具前缀 mcp__${draft.id}__工具（不可修改）。`}
              </div>
              {idError && <div className="fld-err">{idError}</div>}
            </div>
          </div>
        </div>

        <div className="dlg-sec">
          <div className="dlg-sec-title">{draft.transport === "stdio" ? "命令" : "端点"}</div>
          {draft.transport === "stdio" ? (
            <>
              <div className="field" style={{ marginBottom: 10 }}>
                <label>命令</label>
                <input
                  value={draft.command}
                  placeholder="npx"
                  spellCheck={false}
                  onChange={(e) => set({ command: e.target.value })}
                />
                <div className="fld-hint">PATH 中的可执行文件，或者绝对路径。</div>
              </div>
              <div className="field">
                <label>参数</label>
                <input
                  value={argsText}
                  placeholder="-y @upstash/context7-mcp"
                  spellCheck={false}
                  onChange={(e) => setArgsText(e.target.value)}
                />
                <div className="fld-hint">写成一行，和 README 里一样。引号会被识别。</div>
              </div>
            </>
          ) : (
            <div className="field">
              <label>端点 URL</label>
              <input
                value={draft.url}
                placeholder="https://example.com/mcp"
                spellCheck={false}
                onChange={(e) => set({ url: e.target.value })}
              />
              <div className="fld-hint">支持 Streamable HTTP / SSE 的 MCP 端点。</div>
              <div className="row" style={{ gap: 6, marginTop: 6 }}>
                <button
                  className={`switch ${draft.allow_local ? "on" : ""}`}
                  role="switch"
                  aria-checked={draft.allow_local}
                  onClick={() => set({ allow_local: !draft.allow_local })}
                />
                <span className="fld-hint" style={{ margin: 0 }}>允许本地/内网地址（SSRF 防护默认拒绝）</span>
              </div>
            </div>
          )}
        </div>

        <div className="dlg-sec">
          <div className="dlg-sec-title">环境变量</div>
          <div className="sec-note" style={{ marginBottom: 8 }}>只传给这个服务器，不继承系统环境。输入时值是隐藏的。</div>
          {envRows.map((r, i) => (
            <div className="kv-row" key={i}>
              <input
                value={r.key}
                placeholder="变量名，如 API_TOKEN"
                spellCheck={false}
                onChange={(e) =>
                  setEnvRows((rows) => rows.map((x, j) => (j === i ? { ...x, key: e.target.value } : x)))
                }
              />
              <input
                type="password"
                value={r.value}
                placeholder="值（隐藏）"
                autoComplete="off"
                onChange={(e) =>
                  setEnvRows((rows) => rows.map((x, j) => (j === i ? { ...x, value: e.target.value } : x)))
                }
              />
              <button
                className="kv-del"
                title="删除变量"
                onClick={() => setEnvRows((rows) => rows.filter((_, j) => j !== i))}
              >
                <Icon name="trash" size={13} />
              </button>
            </div>
          ))}
          <button className="kv-add" onClick={() => setEnvRows((rows) => [...rows, { key: "", value: "" }])}>
            <Icon name="plus" size={12} /> 添加变量
          </button>
        </div>

        <details className="dlg-adv">
          <summary>高级选项</summary>
          <div className="adv-body">
            {draft.transport === "stdio" && (
              <div className="field">
                <label>工作目录（可选）</label>
                <div style={{ display: "flex", gap: 8 }}>
                  <input
                    value={draft.cwd ?? ""}
                    placeholder="默认继承应用启动目录"
                    spellCheck={false}
                    onChange={(e) => set({ cwd: e.target.value })}
                  />
                  <button
                    className="btn small ghost"
                    style={{ flex: "none" }}
                    title="从资源管理器选择目录"
                    onClick={async () => {
                      const dir = await api.pickDirectory("选择 MCP 服务工作目录");
                      if (dir) set({ cwd: dir });
                    }}
                  >
                    浏览…
                  </button>
                </div>
                <div className="fld-hint">子进程的工作目录；参数里的相对路径基于它解析。</div>
              </div>
            )}
            <div className="field">
              <label>工具调用超时（秒）</label>
              <input
                type="number"
                min={0}
                step={5}
                value={draft.timeout_secs ?? 0}
                placeholder="60"
                onChange={(e) => set({ timeout_secs: Number(e.target.value) || 0 })}
              />
              <div className="fld-hint">单个工具调用的最长等待；0 或留空 = 默认 60 秒。</div>
            </div>
            {draft.transport === "http" && (
              <div className="field">
                <label>请求头</label>
                <div className="fld-hint" style={{ marginBottom: 8 }}>
                  随每个 JSON-RPC 请求发送，例如 Authorization 携带远程端点的令牌。
                </div>
                {hdrRows.map((r, i) => (
                  <div className="kv-row" key={i}>
                    <input
                      value={r.key}
                      placeholder="Header 名，如 Authorization"
                      spellCheck={false}
                      onChange={(e) =>
                        setHdrRows((rows) => rows.map((x, j) => (j === i ? { ...x, key: e.target.value } : x)))
                      }
                    />
                    <input
                      type="password"
                      value={r.value}
                      placeholder="值（隐藏），如 Bearer …"
                      autoComplete="off"
                      onChange={(e) =>
                        setHdrRows((rows) => rows.map((x, j) => (j === i ? { ...x, value: e.target.value } : x)))
                      }
                    />
                    <button
                      className="kv-del"
                      title="删除请求头"
                      onClick={() => setHdrRows((rows) => rows.filter((_, j) => j !== i))}
                    >
                      <Icon name="trash" size={13} />
                    </button>
                  </div>
                ))}
                <button className="kv-add" onClick={() => setHdrRows((rows) => [...rows, { key: "", value: "" }])}>
                  <Icon name="plus" size={12} /> 添加请求头
                </button>
              </div>
            )}
          </div>
        </details>

        <div className="row" style={{ gap: 6 }}>
          <button
            className={`switch ${draft.trusted ? "on" : ""}`}
            role="switch"
            aria-checked={draft.trusted}
            onClick={() => set({ trusted: !draft.trusted })}
          />
          <span className="fld-hint" style={{ margin: 0 }}>信任此服务（工具调用免审批）</span>
        </div>

        <div className="dlg-foot">
          <div className="d-actions">
            <button className="btn small" onClick={onCancel}>
              取消
            </button>
            <button className="btn small primary" disabled={!valid} onClick={save}>
              {isNew ? "保存" : "保存修改"}
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}

/* ------------------------------------------------------------------ */
/* list card — display only, editing goes through the dialog           */
/* ------------------------------------------------------------------ */

const KIND_ICON: Record<string, IconName> = { stdio: "terminal", http: "globe" };

function ServerCard({
  server,
  config,
  status,
  testing,
  onTest,
  onEdit,
  onDelete,
}: {
  server: McpServer;
  /** non-null by the time McpView renders any card (it early-returns otherwise) */
  config: NonNullable<ReturnType<typeof useApp.getState>["config"]>;
  status: McpStatusEntry | undefined;
  testing: boolean;
  onTest: () => void;
  onEdit: () => void;
  onDelete: () => void;
}) {
  const persistConfig = useApp((s) => s.persistConfig);
  const stateOk = status?.state === "ok";
  const metas: string[] = [];
  if (server.cwd) metas.push(`cwd ${server.cwd}`);
  if ((server.timeout_secs ?? 0) > 0) metas.push(`超时 ${server.timeout_secs}s`);
  if (Object.keys(server.headers ?? {}).length > 0) metas.push(`${Object.keys(server.headers!).length} 个请求头`);
  if (Object.keys(server.env ?? {}).length > 0) metas.push(`${Object.keys(server.env).length} 个环境变量`);

  return (
    <div className="provider-card">
      <div className="pc-head">
        <span className="mcp-dot" data-ok={stateOk ? 1 : 0} title={status?.state ?? "未连接"} />
        <div style={{ flex: 1, minWidth: 0 }}>
          <div className="pc-name">
            {server.name || "（未命名服务）"}
            {server.trusted && (
              <span className="chip" title="已信任：工具调用免审批">
                <Icon name="shield" size={11} /> 信任
              </span>
            )}
          </div>
          <div className="pc-url">
            {server.transport === "stdio"
              ? `${server.command} ${server.args.join(" ")}`.trim() || "（未配置命令）"
              : server.url || "（未配置端点）"}
          </div>
        </div>
        <span className="pc-kind">
          <Icon name={KIND_ICON[server.transport] ?? "terminal"} size={12} /> {server.transport}
        </span>
        {stateOk && <span className="chip good">{status?.tools ?? 0} 工具</span>}
        <button
          className={`switch ${server.enabled ? "on" : ""}`}
          role="switch"
          aria-checked={server.enabled}
          title={server.enabled ? "点击停用" : "点击启用"}
          onClick={() =>
            void persistConfig({
              ...config,
              mcp_servers: config.mcp_servers.map((x) => (x.id === server.id ? { ...x, enabled: !x.enabled } : x)),
            })
          }
        />
      </div>
      {(server.description || metas.length > 0) && (
        <div className="mcp-desc">
          {server.description && <span>{server.description}</span>}
          {metas.length > 0 && <span className="mcp-meta">{metas.join(" · ")}</span>}
        </div>
      )}
      <div className="pc-body row" style={{ marginBottom: 0 }}>
        <button className="btn small" disabled={testing} onClick={onTest}>
          {testing ? "连接中…" : "测试连接"}
        </button>
        <button className="btn small" onClick={onEdit}>
          <Icon name="edit" size={13} /> 编辑
        </button>
        <button className="btn small danger" style={{ marginLeft: "auto" }} onClick={onDelete}>
          <Icon name="trash" size={13} /> 删除
        </button>
      </div>
    </div>
  );
}

/* ------------------------------------------------------------------ */
/* view                                                                */
/* ------------------------------------------------------------------ */

export function McpView() {
  // per-field selectors — a bare useApp() re-renders on every stream delta
  const config = useApp((s) => s.config);
  const persistConfig = useApp((s) => s.persistConfig);
  const toast = useApp((s) => s.toast);
  const [status, setStatus] = useState<McpStatusEntry[]>([]);
  const [testingId, setTestingId] = useState<string | null>(null);
  /** Draft being added/edited in the dialog; null = closed. */
  const [dlg, setDlg] = useState<{ draft: McpServer; isNew: boolean } | null>(null);
  // delete confirmation: a server's env/headers carry credentials and the
  // action is instant — it must not be one accidental click away
  const [confirmDelId, setConfirmDelId] = useState<string | null>(null);

  const refresh = () => {
    void api
      .mcpStatus()
      .then(setStatus)
      .catch(() => {});
  };

  useEffect(() => {
    refresh();
  }, [config]);

  if (!config) return null;

  const saveAll = async (servers: McpServer[]) => {
    await persistConfig({ ...config, mcp_servers: servers });
    setTimeout(refresh, 100);
  };

  const test = async (s: McpServer) => {
    setTestingId(s.id);
    try {
      const r = await api.mcpTest(s);
      if (r.ok) toast("success", `${s.name}: ${r.message}`);
      else toast("error", `${s.name}: ${r.message}`);
    } catch (e) {
      toast("error", String(e));
    } finally {
      setTestingId(null);
      refresh();
    }
  };

  const saveDialog = async (s: McpServer, isNew: boolean) => {
    const servers = isNew
      ? [...config.mcp_servers, s]
      : config.mcp_servers.map((x) => (x.id === s.id ? s : x));
    await saveAll(servers);
    setDlg(null);
    toast("success", isNew ? "已保存 —— 点「测试连接」验证" : "已保存修改");
  };

  return (
    <>
      <div className="view-header">
        <div>
          <div className="view-title">MCP 服务</div>
          <div className="view-sub">
            连接外部 MCP 工具服务器 —— 工具以 mcp__服务__工具 命名进入对话；未信任服务的每次调用需审批
          </div>
        </div>
        <div className="spacer" />
        <button className="btn primary" onClick={() => setDlg({ draft: blankServer(), isNew: true })}>
          <Icon name="plus" size={13} /> 添加服务
        </button>
      </div>
      <div className="view-body">
        {dlg && (
          <ServerDialog
            initial={dlg.draft}
            isNew={dlg.isNew}
            existingIds={config.mcp_servers.map((s) => s.id).filter((id) => !dlg.isNew || id !== dlg.draft.id)}
            onDone={(s) => void saveDialog(s, dlg.isNew)}
            onCancel={() => setDlg(null)}
          />
        )}

        {config.mcp_servers.length === 0 && !dlg && (
          <div className="empty-state">
            <div className="big"><Icon name="plug" size={40} /></div>
            <h3>还没有 MCP 服务</h3>
            <p>
              添加 stdio 服务（如 npx -y @modelcontextprotocol/server-filesystem）或 HTTP 端点，
              它们的工具会自动出现在模型对话的工具面中。可以从「Context7」等模板一键开始。
            </p>
            <div style={{ marginTop: 14 }}>
              <button className="btn primary" onClick={() => setDlg({ draft: blankServer(), isNew: true })}>
                <Icon name="plus" size={13} /> 添加第一个服务
              </button>
            </div>
          </div>
        )}

        <div className="provider-grid">
          {config.mcp_servers.map((s) => (
            <ServerCard
              key={s.id}
              server={s}
              config={config}
              status={status.find((x) => x.id === s.id)}
              testing={testingId === s.id}
              onTest={() => void test(s)}
              onEdit={() => setDlg({ draft: s, isNew: false })}
              onDelete={() => setConfirmDelId(s.id)}
            />
          ))}
        </div>
      </div>
      {confirmDelId && (
        <ConfirmDialog
          title="删除 MCP 服务"
          description={`确定删除「${
            config.mcp_servers.find((x) => x.id === confirmDelId)?.name || confirmDelId
          }」？其环境变量与请求头配置会一并移除。`}
          confirmText="删除"
          danger
          onConfirm={() => {
            void saveAll(config.mcp_servers.filter((x) => x.id !== confirmDelId));
            setConfirmDelId(null);
          }}
          onCancel={() => setConfirmDelId(null)}
        />
      )}
    </>
  );
}
