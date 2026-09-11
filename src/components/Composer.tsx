// Composer: input + attachments + permission mode + model slot + thinking
// level + context-usage meter. All controls are functional; the layout is
// our own design-system styling.
import { useEffect, useRef, useState } from "react";
import { useApp } from "../store";
import { Dropdown } from "./Dropdown";
import { BrandMark, ModelMark } from "../lib/lobeIcon";
import { Icon } from "../lib/icons";
import { fmtBytes, fmtHit, fmtTokens } from "../lib/format";
import * as api from "../lib/api";
import type { ChatImage, EnhanceOutcome, SessionBinding, SessionMeta, SessionTelemetry, SkillInfo } from "../types";

interface SlashItem {
  name: string;
  desc: string;
  kind: "builtin" | "skill";
  skill?: SkillInfo;
}

const BUILTINS: SlashItem[] = [
  { name: "compact", desc: "压缩上下文 —— 此前对话折叠为摘要", kind: "builtin" },
  { name: "export", desc: "导出会话为 Markdown", kind: "builtin" },
  { name: "clear", desc: "清空会话全部消息（不可恢复）", kind: "builtin" },
  { name: "workspace", desc: "绑定工作区：/workspace <路径>", kind: "builtin" },
  { name: "skills", desc: "列出可用技能", kind: "builtin" },
  {
    name: "goal",
    desc: "目标管理：/goal <目标> 创建 · /goal 查看 · /goal pause|resume|clear",
    kind: "builtin",
  },
  { name: "wiki", desc: "生成仓库导读 —— 子智能体扫描工作区，写入 .ccharness/wiki.md（新会话自动注入）", kind: "builtin" },
];

interface Attachment {
  id: string;
  name: string;
  size: number;
  text: string;
}

const PERM_OPTIONS = [
  { value: "readonly", label: <><Icon name="shield" size={13} /> 只读</> },
  { value: "approve", label: <><Icon name="shield" size={13} /> 需审批</> },
  {
    value: "auto",
    label: (
      <span style={{ color: "var(--bad)" }}>
        <Icon name="shieldAlert" size={13} /> 自动写入
      </span>
    ),
  },
];

const WORKFLOW_OPTIONS = [
  { value: "agent", label: <><Icon name="robot" size={13} /> 智能体</> },
  { value: "plan", label: <><Icon name="clipboard" size={13} /> 规划</> },
  { value: "goal", label: <><Icon name="target" size={13} /> 目标</> },
  { value: "deep", label: <><Icon name="cpu" size={13} /> 深度</> },
  { value: "review", label: <><Icon name="scan" size={13} /> 审阅</> },
];

/** An image staged for the next send (base64 + instant preview URI). */
interface PendingImage extends ChatImage {
  id: string;
  uri: string;
}

/** ArrayBuffer → base64 in 0x8000 chunks (avoids call-stack limits). */
function bufToB64(buf: ArrayBuffer): string {
  const bytes = new Uint8Array(buf);
  let bin = "";
  const CHUNK = 0x8000;
  for (let i = 0; i < bytes.length; i += CHUNK) {
    bin += String.fromCharCode(...bytes.subarray(i, i + CHUNK));
  }
  return btoa(bin);
}

const THINK_OPTIONS = [
  { value: "default", label: <><Icon name="cpu" size={13} /> 思考: 默认</> },
  { value: "low", label: <><Icon name="cpu" size={13} /> 思考: 低</> },
  { value: "medium", label: <><Icon name="cpu" size={13} /> 思考: 中</> },
  { value: "high", label: <><Icon name="cpu" size={13} /> 思考: 高</> },
];

function ContextMeter({ sessionId, contextWindow }: { sessionId: string; contextWindow: number }) {
  const [open, setOpen] = useState(false);
  const [tel, setTel] = useState<SessionTelemetry | null>(null);
  const last = useApp((s) => s.lastRequest[sessionId]);
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const close = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", close);
    void api.getTelemetry(sessionId).then(setTel).catch(() => {});
    return () => document.removeEventListener("mousedown", close);
  }, [open, sessionId]);

  const inputTok = last?.input_tokens ?? null;
  const cached = last?.cached_tokens ?? null;
  const pct = inputTok != null && contextWindow > 0 ? Math.min(100, (inputTok / contextWindow) * 100) : null;

  return (
    <div className="ctx-wrap" ref={ref}>
      <button
        className="token-pill"
        title="上下文用量"
        onClick={() => setOpen((o) => !o)}
      >
        <span className="tp-ring" style={{ "--pct": Math.round(pct ?? 0) } as React.CSSProperties} />
        {inputTok != null ? (
          <span className="mono">
            {fmtTokens(inputTok)} · {pct?.toFixed(0)}%
          </span>
        ) : (
          <span className="mono dim">token</span>
        )}
      </button>
      {open && (
        <div className="ctx-pop">
          <div className="cp-head">
            <span>上下文用量</span>
            <span className="mono">
              {fmtTokens(inputTok)} / {fmtTokens(contextWindow)}
              {pct != null ? `（${pct.toFixed(1)}%）` : ""}
            </span>
          </div>
          <div className="cp-bar">
            <div className="cp-bar-fill" style={{ width: `${pct ?? 0}%` }} />
          </div>
          <div className="cp-row">
            <span>稳定前缀</span>
            <span className="mono">{last ? fmtBytes(last.prefix_bytes) : "—"}</span>
          </div>
          <div className="cp-row">
            <span>本轮新增</span>
            <span className="mono">{last ? fmtBytes(last.added_bytes) : "—"}</span>
          </div>
          <div className="cp-row">
            <span>最近请求命中</span>
            <span className="mono">
              {last ? fmtHit(last.cached_tokens, last.input_tokens) : "—"}
            </span>
          </div>
          <div className="cp-row">
            <span>会话平均命中率</span>
            <span className="mono">
              {tel?.summary.avg_hit_rate != null ? `${tel.summary.avg_hit_rate.toFixed(1)}%` : "—"}
            </span>
          </div>
          <div className="cp-row">
            <span>累计请求</span>
            <span className="mono">{tel?.summary.requests ?? 0}</span>
          </div>
          <div className="cp-foot">窗口在「模型管理 → 上下文窗口」中按 Provider 配置</div>
        </div>
      )}
    </div>
  );
}

export function Composer({
  placeholder,
  disabled,
  busy,
  onSend,
  onStop,
  sessionId,
  contextWindow = 131072,
  workspace,
  onCommand,
  workflow,
  extraWorkflowOptions,
  defaultPerm,
  onPermChange,
  imageMode,
  topSlot,
  injected,
  children,
}: {
  placeholder: string;
  disabled: boolean;
  busy: boolean;
  onSend: (text: string, skills: string[], images: ChatImage[]) => void;
  onStop: () => void;
  sessionId?: string;
  contextWindow?: number;
  workspace?: string | null;
  onCommand?: (cmd: string, args: string) => void;
  /** Workflow gate (agent/plan); omit to hide the switcher (arena). */
  workflow?: { value: string; onChange: (m: string) => void };
  /** Declarative workflows appended after the builtin gate options
   *  (value = "sm:<id>", selecting one initializes the session at the
   *  workflow's entry state). */
  extraWorkflowOptions?: { value: string; label: React.ReactNode }[];
  /** Initial permission mode (hero composer pre-session pick). */
  defaultPerm?: string;
  /** Fired on permission change; with no sessionId the caller applies it
   *  to the session it is about to create (welcome hero). */
  onPermChange?: (mode: string) => void;
  /** Image-generation session: violet accent, chat-only controls (perm /
   *  thinking / enhance) hidden. Mode switching itself lives in the
   *  floating 对话|生图 pill above the composer (ChatView). */
  imageMode?: boolean;
  /** Floating chrome rendered inside the composer box (position:relative)
   *  — e.g. the mode-switch pill anchored to its top-right edge. */
  topSlot?: React.ReactNode;
  /** Selection-ask (划选追问): ChatView pushes a quoted snippet here with a
   *  fresh nonce; the composer appends it to the draft and focuses. */
  injected?: { text: string; nonce: number } | null;
  children?: React.ReactNode;
}) {
  const sendOnEnter = useApp((s) => s.config?.settings.send_on_enter ?? true);
  const thinking = useApp((s) => s.config?.settings.thinking_level ?? "default");
  const persistConfig = useApp((s) => s.persistConfig);
  const toast = useApp((s) => s.toast);
  const [text, setText] = useState("");
  const [attachments, setAttachments] = useState<Attachment[]>([]);
  const [pendingImages, setPendingImages] = useState<PendingImage[]>([]);
  const [skillChips, setSkillChips] = useState<SkillInfo[]>([]);
  const [permMode, setPermMode] = useState(defaultPerm ?? "approve");
  const [enhancing, setEnhancing] = useState(false);
  const [enhPanel, setEnhPanel] = useState<EnhanceOutcome | null>(null);
  const ref = useRef<HTMLTextAreaElement>(null);
  const fileRef = useRef<HTMLInputElement>(null);
  const [slash, setSlash] = useState<{ items: SlashItem[]; hl: number } | null>(null);
  const [atMenu, setAtMenu] = useState<{ items: string[]; hl: number; start: number; end: number } | null>(null);
  // # 会话引用菜单：列出最近的 chat 会话，选中后插入确定性摘要
  // （session_digest 命令：目标 + 用户要求 + 最近结论，无模型调用）
  const [hashMenu, setHashMenu] = useState<null | { sessions: SessionMeta[]; q: string; loading: boolean }>(null);
  const skillsCache = useRef<Map<string, SkillInfo[]>>(new Map());

  // permission mode is per-session, in-memory, default "approve"
  useEffect(() => {
    setPermMode("approve");
  }, [sessionId]);

  // selection-ask injection: append the quoted snippet to the draft
  useEffect(() => {
    if (!injected?.nonce) return;
    setText((cur) => (cur.trim() ? cur + "\n\n" : "") + injected.text);
    requestAnimationFrame(() => ref.current?.focus());
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [injected?.nonce]);

  // slash menu: active while the text starts with "/"; items = builtin
  // commands + project/global skills, filtered by the token after "/"
  useEffect(() => {
    if (!text.startsWith("/") || !onCommand) {
      setSlash(null);
      return;
    }
    const token = text.slice(1).split(/\s/, 1)[0]?.toLowerCase() ?? "";
    const key = workspace ?? "";
    const build = (skills: SkillInfo[]) => {
      const all: SlashItem[] = [
        ...BUILTINS,
        ...skills.map((s) => ({
          name: s.name,
          desc: s.description || (s.auto_inject ? "自动注入技能" : "技能"),
          kind: "skill" as const,
          skill: s,
        })),
      ];
      const items = token ? all.filter((it) => it.name.toLowerCase().includes(token)) : all;
      setSlash({ items, hl: 0 });
    };
    if (skillsCache.current.has(key)) {
      build(skillsCache.current.get(key)!);
    } else {
      void api
        .getSkills(workspace ?? null)
        .then((skills) => {
          skillsCache.current.set(key, skills);
          if (text.startsWith("/")) build(skills);
        })
        .catch(() => skillsCache.current.set(key, []));
    }
  }, [text, workspace, onCommand]);

  // @-reference menu: active while the caret sits right after "@token"
  // (workspace-bound sessions only). Items are workspace-relative paths.
  useEffect(() => {
    if (!workspace) {
      setAtMenu(null);
      return;
    }
    const m = /(^|\s)@([^\s@]*)$/.exec(text);
    if (!m) {
      setAtMenu(null);
      return;
    }
    const token = m[2];
    const start = m.index + m[1].length;
    let cancelled = false;
    void api
      .searchWorkspaceFiles(workspace, token)
      .then((items) => {
        if (cancelled) return;
        setAtMenu(items.length > 0 ? { items, hl: 0, start, end: start + 1 + token.length } : null);
      })
      .catch(() => setAtMenu(null));
    return () => {
      cancelled = true;
    };
  }, [text, workspace]);

  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.style.height = "0px";
    el.style.height = Math.min(el.scrollHeight, 220) + "px";
  }, [text]);

  const addFiles = async (files: FileList | File[] | null) => {
    if (!files || files.length === 0) return;
    const next: Attachment[] = [];
    for (const f of Array.from(files)) {
      if (f.size > 200 * 1024) {
        toast("error", `${f.name} 超过单文件 200KB 上限`);
        continue;
      }
      const buf = await f.arrayBuffer();
      if (new Uint8Array(buf.slice(0, 8)).includes(0)) {
        toast("error", `${f.name} 是二进制文件，仅支持文本附件`);
        continue;
      }
      next.push({
        id: `${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
        name: f.name,
        size: f.size,
        text: new TextDecoder().decode(buf),
      });
    }
    setAttachments((cur) => {
      const merged = [...cur, ...next];
      if (merged.reduce((s, a) => s + a.size, 0) > 500 * 1024) {
        toast("error", "附件总大小超过 500KB");
        return cur;
      }
      return merged;
    });
  };

  // Stage images for the next send: ≤4 per message, ≤6MB each. The bytes
  // are base64'd here and shipped as ChatImage[]; the backend persists
  // them under the session's attachments dir.
  const addImages = async (files: FileList | File[] | null) => {
    if (!files || files.length === 0) return;
    if (imageMode) {
      toast("error", "生图模式不支持附加图片");
      return;
    }
    const picks: PendingImage[] = [];
    for (const f of Array.from(files)) {
      if (!f.type.startsWith("image/")) {
        toast("error", `${f.name} 不是图片文件`);
        continue;
      }
      if (f.size > 6 * 1024 * 1024) {
        toast("error", `${f.name} 超过单图 6MB 上限`);
        continue;
      }
      const buf = await f.arrayBuffer();
      const b64 = bufToB64(buf);
      picks.push({
        id: `${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
        mime: f.type || "image/png",
        b64,
        uri: `data:${f.type || "image/png"};base64,${b64}`,
      });
    }
    if (picks.length === 0) return;
    setPendingImages((cur) => {
      const merged = [...cur, ...picks];
      if (merged.length > 4) {
        toast("error", "单条消息最多附带 4 张图片");
        return cur;
      }
      return merged;
    });
  };

  // expand @path references into quoted blocks (max 5, failures stay as-is)
  const expandAtRefs = async (t: string): Promise<string> => {
    if (!workspace) return t;
    const tokens = [...new Set([...t.matchAll(/@([^\s@\\][^\s@]*)/g)].map((m) => m[1]))].slice(0, 5);
    let out = t;
    for (const p of tokens) {
      try {
        const content = await api.readWorkspaceFile(workspace, p);
        out += `\n\n---\n📎 引用文件 @${p}\n\`\`\`\n${content}\n\`\`\``;
      } catch {
        /* not a readable workspace file — leave the token untouched */
      }
    }
    return out;
  };

  // in-flight lock: the busy flag only flips when onSend lands in the store,
  // so the await inside submit (expandAtRefs) is a re-entry window where a
  // second Enter would double-send before text state clears
  const submittingRef = useRef(false);
  const submit = async () => {
    const t = text.trim();
    if (submittingRef.current) return;
    if ((!t && pendingImages.length === 0) || busy || disabled) return;
    submittingRef.current = true;
    try {
      const expanded = await expandAtRefs(t);
      let content = "";
      if (skillChips.length > 0) {
        content +=
          skillChips
            .map((s) => `[调用技能 /${s.name}${s.description ? ` — ${s.description}` : ""}]\n\n${s.body}`)
            .join("\n\n") + "\n\n";
      }
      content += expanded;
      if (attachments.length > 0) {
        const blocks = attachments
          .map(
            (a, i) =>
              `\n\n---\n📎 附件 ${i + 1}/${attachments.length}: ${a.name}（${a.size} B）\n\`\`\`\n${a.text}\n\`\`\``
          )
          .join("");
        content += blocks;
      }
      const images: ChatImage[] = pendingImages.map(({ mime, b64 }) => ({ mime, b64 }));
      setText("");
      setAttachments([]);
      setPendingImages([]);
      setAtMenu(null);
      const names = skillChips.map((c) => c.name);
      setSkillChips([]);
      onSend(content, names, images);
    } finally {
      submittingRef.current = false;
    }
  };

  const setPerm = (mode: string) => {
    setPermMode(mode);
    if (sessionId) void api.setPermissionMode(sessionId, mode);
    onPermChange?.(mode);
    const labels: Record<string, string> = {
      readonly: "只读——写工具已从工具面移除",
      approve: "写入需审批——每次写操作弹出审批卡",
      auto: "自动写入——本会话内写工具免审批（仍有工作区边界）",
    };
    toast("info", labels[mode] ?? "权限档位已更新");
  };

  // switching to auto-write is gated behind an explicit risk-acknowledgement
  // dialog: the user must tick the liability checkbox before it applies
  const [permWarn, setPermWarn] = useState(false);
  const [permAck, setPermAck] = useState(false);
  const requestPerm = (mode: string) => {
    if (mode === "auto" && permMode !== "auto") {
      setPermAck(false);
      setPermWarn(true);
      return;
    }
    setPerm(mode);
  };

  // AuxMemo whitelisted call: enhance the draft with the session's model.
  // Same draft bytes ⇒ exact cache hit (no API call, no billing).
  const runEnhance = async () => {
    if (!sessionId || !text.trim() || enhancing) return;
    setEnhancing(true);
    try {
      const out = await api.enhancePrompt(sessionId, text);
      setEnhPanel(out);
    } catch (e) {
      toast("error", `提示词增强失败: ${String(e)}`);
    } finally {
      setEnhancing(false);
    }
  };

  // ---- # 历史会话引用（ZCode parity）：打开菜单 → 选中会话 → 插入摘要 ----
  const openHashMenu = async () => {
    setHashMenu({ sessions: [], q: "", loading: true });
    try {
      const all = await api.listSessions();
      const sessions = all
        .filter((s) => s.kind === "chat" && !s.archived)
        .sort((a, b) => b.updated_at - a.updated_at)
        .slice(0, 40);
      setHashMenu({ sessions, q: "", loading: false });
    } catch (e) {
      setHashMenu(null);
      toast("error", `读取会话列表失败: ${String(e)}`);
    }
  };
  const pickSession = async (s: SessionMeta) => {
    setHashMenu(null);
    try {
      const digest = await api.sessionDigest(s.id);
      const block = `#历史会话引用\n${digest}`;
      setText((cur) => (cur.trim() ? cur + "\n\n" : "") + block);
      requestAnimationFrame(() => ref.current?.focus());
    } catch (e) {
      toast("error", `生成会话摘要失败: ${String(e)}`);
    }
  };
  const relTime = (ts: number) => {
    const diff = Date.now() - ts;
    if (diff < 60_000) return "刚刚";
    if (diff < 3_600_000) return `${Math.floor(diff / 60_000)} 分钟前`;
    if (diff < 86_400_000) return `${Math.floor(diff / 3_600_000)} 小时前`;
    return `${Math.floor(diff / 86_400_000)} 天前`;
  };

  const setThinking = (level: string) => {
    if (!useApp.getState().config) return;
    void persistConfig({
      ...useApp.getState().config!,
      settings: { ...useApp.getState().config!.settings, thinking_level: level },
    });
    const levelNames: Record<string, string> = { low: "低", medium: "中", high: "高" };
    toast(
      "info",
      level === "default"
        ? "思考级别: 默认（不发送 reasoning_effort）——切换会使缓存纪元重建"
        : `思考级别: ${levelNames[level] ?? level}——已随请求头发送，缓存纪元重建`
    );
  };

  // execute the highlighted slash item: builtin → onCommand; skill → show a
  // colored invocation chip (full body is injected at send time, never typed
  // into the textarea)
  const chooseSlash = () => {
    if (!slash || slash.items.length === 0) return;
    const it = slash.items[Math.min(slash.hl, slash.items.length - 1)];
    const rest = text
      .slice(1)
      .split(/\s/)
      .slice(1)
      .join(" ");
    setSlash(null);
    if (it.kind === "builtin") {
      setText("");
      onCommand?.(it.name, rest);
    } else if (it.skill) {
      setSkillChips((cur) => (cur.some((c) => c.name === it.skill!.name) ? cur : [...cur, it.skill!]));
      setText(rest);
    }
  };

  return (
    <div className="composer-wrap">
      <div className={`composer${imageMode ? " image-mode" : ""}`}>
        {topSlot}
        {atMenu && atMenu.items.length > 0 && (
          <div className="slash-menu">
            <div className="at-head">@ 引用工作区文件（Tab/Enter 选择，Esc 关闭）</div>
            {atMenu.items.map((p, i) => (
              <button
                key={p}
                className={`slash-item ${i === atMenu.hl ? "hl" : ""}`}
                onMouseEnter={() => setAtMenu((s) => (s ? { ...s, hl: i } : s))}
                onMouseDown={(e) => e.preventDefault()}
                onClick={() => {
                  setText((cur) => cur.slice(0, atMenu.start) + "@" + p + " " + cur.slice(atMenu.end));
                  setAtMenu(null);
                }}
              >
                <span className="mono slash-name"><Icon name="folder" size={13} /> {p}</span>
              </button>
            ))}
          </div>
        )}
        {slash && slash.items.length > 0 && (
          <div className="slash-menu">
            {slash.items.map((it, i) => (
              <button
                key={`${it.kind}-${it.name}`}
                className={`slash-item ${i === slash.hl ? "hl" : ""}`}
                onMouseEnter={() => setSlash((s) => (s ? { ...s, hl: i } : s))}
                onMouseDown={(e) => e.preventDefault()}
                onClick={() => {
                  setSlash((s) => (s ? { ...s, hl: i } : s));
                  chooseSlash();
                }}
              >
                <span className="mono slash-name">/{it.name}</span>
                <span className="slash-badge">{it.kind === "builtin" ? "命令" : "技能"}</span>
                <span className="slash-desc">{it.desc}</span>
              </button>
            ))}
          </div>
        )}
        {(attachments.length > 0 || skillChips.length > 0 || pendingImages.length > 0) && (
          <div className="attach-row">
            {pendingImages.map((p) => (
              <span key={p.id} className="img-chip" title="已附加图片 —— 将随消息发送给模型">
                <img src={p.uri} alt="" />
                <button
                  onClick={() => setPendingImages((cur) => cur.filter((x) => x.id !== p.id))}
                  aria-label="移除图片"
                >
                  ×
                </button>
              </span>
            ))}
            {skillChips.map((s) => (
              <span key={`sk-${s.name}`} className="skill-chip" title={`已调用技能 /${s.name} —— 正文将在发送时注入`}>
                <Icon name="bolt" size={12} /> /{s.name}
                <button
                  onClick={() => setSkillChips((cur) => cur.filter((x) => x.name !== s.name))}
                  aria-label={`移除技能 ${s.name}`}
                >
                  ×
                </button>
              </span>
            ))}
            {attachments.map((a) => (
              <span key={a.id} className="attach-chip" title={`${a.name} · ${a.size} B`}>
                <Icon name="paperclip" size={12} /> {a.name}
                <button
                  onClick={() => setAttachments((cur) => cur.filter((x) => x.id !== a.id))}
                  aria-label={`移除附件 ${a.name}`}
                >
                  ×
                </button>
              </span>
            ))}
          </div>
        )}
        {enhPanel && (
          <div className="enhance-panel">
            <div className="enh-head">
              <span className="enh-title"><Icon name="spark" size={13} /> 优化后的提示词</span>
              <span
                className="enh-origin"
                style={{ color: enhPanel.origin === "miss" ? "var(--text-faint)" : "var(--good)" }}
              >
                {enhPanel.origin === "miss"
                  ? `由 ${enhPanel.model} 生成`
                  : "来自本地缓存（未产生 API 调用）"}
              </span>
            </div>
            <textarea className="enh-text" readOnly value={enhPanel.text} rows={4} />
            <div className="enh-actions">
              <button className="enh-btn ghost" onClick={() => setEnhPanel(null)}>
                放弃
              </button>
              <button
                className="enh-btn primary"
                onClick={() => {
                  setText(enhPanel.text);
                  setEnhPanel(null);
                  requestAnimationFrame(() => ref.current?.focus());
                }}
              >
                采用并替换草稿
              </button>
            </div>
          </div>
        )}
        {hashMenu && (
          <div className="slash-menu">
            <div className="at-head"># 引用历史会话（选中后插入其摘要，Esc 关闭）</div>
            <input
              className="hash-search"
              autoFocus
              placeholder="搜索会话标题…"
              value={hashMenu.q}
              onChange={(e) => setHashMenu((s) => (s ? { ...s, q: e.target.value } : s))}
              onKeyDown={(e) => {
                if (e.key === "Escape") {
                  e.preventDefault();
                  setHashMenu(null);
                }
                if (e.key === "Enter" && hashMenu.sessions.length > 0) {
                  e.preventDefault();
                  void pickSession(hashMenu.sessions[0]);
                }
              }}
            />
            {hashMenu.loading && <div className="slash-desc" style={{ padding: "6px 10px" }}>读取中…</div>}
            {hashMenu.sessions
              .filter((s) => !hashMenu.q || s.title.toLowerCase().includes(hashMenu.q.toLowerCase()))
              .map((s) => (
                <button
                  key={s.id}
                  className="slash-item"
                  onMouseDown={(e) => e.preventDefault()}
                  onClick={() => void pickSession(s)}
                >
                  <span className="mono slash-name" style={{ maxWidth: 340, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>
                    <Icon name="chat" size={13} /> {s.title}
                  </span>
                  <span className="slash-desc">{relTime(s.updated_at)}</span>
                </button>
              ))}
          </div>
        )}
        <textarea
          ref={ref}
          rows={1}
          value={text}
          placeholder={placeholder}
          disabled={disabled}
          onChange={(e) => setText(e.target.value)}
          onPaste={(e) => {
            // paste-image support: intercept only when the clipboard
            // carries image files; text pastes stay untouched
            const files = e.clipboardData?.files;
            if (!imageMode && files && files.length > 0) {
              const imgs = Array.from(files).filter((f) => f.type.startsWith("image/"));
              if (imgs.length > 0) {
                e.preventDefault();
                void addImages(imgs);
                return;
              }
            }
            // overlong text paste → auto-attach (ZCode parity): a huge dump
            // would wreck the draft box; as an attachment it rides as a
            // fenced block appended at send time, removable via its chip
            if (!imageMode) {
              const txt = e.clipboardData?.getData("text/plain") ?? "";
              if (txt.length > 12_000) {
                e.preventDefault();
                const size = new Blob([txt]).size;
                setAttachments((cur) => [
                  ...cur,
                  {
                    id: `${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
                    name: `粘贴文本 ${txt.length} 字`,
                    size,
                    text: txt,
                  },
                ]);
                toast("info", `粘贴内容过长（${txt.length} 字符）—— 已自动转为附件注入，可点标签移除`);
                return;
              }
            }
          }}
          onKeyDown={(e) => {
            if (hashMenu && e.key === "Escape") {
              setHashMenu(null);
              return;
            }
            if (atMenu && atMenu.items.length > 0) {
              if (e.key === "ArrowDown" || e.key === "ArrowUp") {
                e.preventDefault();
                setAtMenu((s) =>
                  s
                    ? {
                        ...s,
                        hl:
                          e.key === "ArrowDown"
                            ? Math.min(s.hl + 1, s.items.length - 1)
                            : Math.max(s.hl - 1, 0),
                      }
                    : s
                );
                return;
              }
              if (e.key === "Tab" || (e.key === "Enter" && !e.shiftKey)) {
                e.preventDefault();
                const path = atMenu.items[Math.min(atMenu.hl, atMenu.items.length - 1)];
                setText((cur) => cur.slice(0, atMenu.start) + "@" + path + " " + cur.slice(atMenu.end));
                setAtMenu(null);
                return;
              }
              if (e.key === "Escape") {
                setAtMenu(null);
                return;
              }
            }
            if (e.key === "Escape" && enhPanel) {
              setEnhPanel(null);
              return;
            }
            if (slash && slash.items.length > 0) {
              if (e.key === "ArrowDown" || e.key === "ArrowUp") {
                e.preventDefault();
                setSlash((s) =>
                  s
                    ? {
                        ...s,
                        hl:
                          e.key === "ArrowDown"
                            ? Math.min(s.hl + 1, s.items.length - 1)
                            : Math.max(s.hl - 1, 0),
                      }
                    : s
                );
                return;
              }
              if (e.key === "Tab") {
                e.preventDefault();
                chooseSlash();
                return;
              }
              if (e.key === "Escape") {
                setSlash(null);
                return;
              }
              if (e.key === "Enter" && !e.shiftKey) {
                e.preventDefault();
                chooseSlash();
                return;
              }
            }
            // plain Enter (sendOnEnter) must exclude Ctrl/Meta — otherwise a
            // Ctrl+Enter keydown matches both branches below and fires
            // submit() twice (one direct send + one queued)
            if (e.key === "Enter" && !e.shiftKey && !e.ctrlKey && !e.metaKey && sendOnEnter) {
              e.preventDefault();
              submit();
              return;
            }
            if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
              e.preventDefault();
              submit();
            }
          }}
        />
        <div className="composer-foot">
          {/* foot tools render with or without a session: with no sessionId
              the choices are held locally and applied when the caller creates
              the session (welcome hero) */}
          <>
            {/* single attach entry: picked files are routed by type —
                images go to the multimodal pipeline (chips + ChatImage[]),
                everything else becomes a text attachment injected into the
                message body */}
            <button
              className="foot-btn"
              title="添加附件 —— 图片随消息发送（≤4 张、单张 ≤6MB），文本文件注入正文（≤200KB/个）；也可直接粘贴图片"
              onClick={() => fileRef.current?.click()}
            >
              <Icon name="plus" size={14} />
            </button>
            <input
              ref={fileRef}
              type="file"
              multiple
              style={{ display: "none" }}
              onChange={(e) => {
                const files = Array.from(e.target.files ?? []);
                const imgs = files.filter((f) => f.type.startsWith("image/"));
                const rest = files.filter((f) => !f.type.startsWith("image/"));
                if (imgs.length > 0) void addImages(imgs);
                if (rest.length > 0) void addFiles(rest);
                e.currentTarget.value = "";
              }}
            />
            {sessionId && !imageMode && (
              <button
                className="foot-btn"
                title="# 引用历史会话 —— 把某个旧会话的摘要（目标 / 用户要求 / 最近结论）插入当前消息"
                onClick={() => (hashMenu ? setHashMenu(null) : void openHashMenu())}
              >
                <span className="mono" style={{ fontSize: 14, fontWeight: 700, lineHeight: 1 }}>#</span>
              </button>
            )}
            {imageMode && (
              <span className="img-mode-tag" title="生图模式 —— 仅图像模型，不使用工具与工作区">
                <Icon name="image" size={12} /> 生图模式
              </span>
            )}
            {!imageMode && (
              <Dropdown
                compact
                value={permMode}
                options={PERM_OPTIONS}
                onChange={requestPerm}
                minWidth={132}
              />
            )}
            {workflow && (
              <Dropdown
                compact
                value={workflow.value}
                options={[...WORKFLOW_OPTIONS, ...(extraWorkflowOptions ?? [])]}
                onChange={(m) => workflow.onChange(m)}
                minWidth={104}
              />
            )}
            {!imageMode && (
              <button
                className="foot-btn"
                title={
                  sessionId
                    ? "优化提示词 —— 同一草稿重复优化会命中本地精确缓存，不产生 API 调用"
                    : "发送首条消息创建会话后可用"
                }
                disabled={enhancing || !text.trim() || !sessionId}
                onClick={() => void runEnhance()}
              >
                {enhancing ? "…" : <Icon name="spark" size={15} />}
              </button>
            )}
          </>
          <div className="foot-spacer" />
          {sessionId && <ContextMeter sessionId={sessionId} contextWindow={contextWindow} />}
          {children}
          {!imageMode && (
            <Dropdown
              compact
              value={thinking}
              options={THINK_OPTIONS}
              onChange={setThinking}
              minWidth={122}
            />
          )}
          {busy ? (
            <button className="send-btn stop" title="停止生成" onClick={onStop}>
              <svg viewBox="0 0 16 16" fill="currentColor">
                <rect x="3" y="3" width="10" height="10" rx="2" />
              </svg>
            </button>
          ) : (
            <button className="send-btn" title="发送" disabled={disabled || (!text.trim() && pendingImages.length === 0)} onClick={submit}>
              <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
                <path d="M2.5 8 L13 2.5 L10.5 13.5 L8 9 Z" />
              </svg>
            </button>
          )}
        </div>
      </div>
      {permWarn && (
        <div
          className="dialog-overlay"
          onMouseDown={(e) => e.target === e.currentTarget && setPermWarn(false)}
        >
          <div className="dialog perm-warn-dialog" role="dialog" aria-label="启用自动写入模式">
            <h3>启用「自动写入」模式？</h3>
            <p style={{ fontSize: 13, marginBottom: 8 }}>
              切换后，模型在本会话内执行写入工具时将<strong style={{ color: "var(--bad)" }}>不再弹出逐条审批</strong>。
              这意味着模型可能自动做出以下行为：
            </p>
            <ul className="perm-risk">
              <li>新建、修改、<strong>覆盖或删除</strong>工作区内的任意文件（包括源码、配置与文档）</li>
              <li>批量重命名、移动文件，或清空 / 重写数据文件——<strong>包括数据库文件、.env 密钥文件等</strong></li>
              <li>连续执行多步写操作而中途不需要你确认，误操作<strong>不经过回收站、无法撤销</strong></li>
            </ul>
            <p style={{ fontSize: 12, color: "var(--text-faint)", margin: "2px 0 10px" }}>
              工作区边界外的系统文件仍受保护；建议先提交或备份当前工作区（git），或开启 Worktree 隔离后再使用本模式。
            </p>
            <label className="perm-ack">
              <input
                type="checkbox"
                checked={permAck}
                onChange={(e) => setPermAck(e.target.checked)}
              />
              <span>
                我已阅读并理解上述风险。因自动写入导致的<strong>文件删除、数据损坏、数据库清空等问题，由我自行承担</strong>。
              </span>
            </label>
            <div className="d-actions">
              <button className="btn small" onClick={() => setPermWarn(false)}>
                取消
              </button>
              <button
                className="btn small danger"
                disabled={!permAck}
                onClick={() => {
                  setPermWarn(false);
                  setPerm("auto");
                }}
              >
                启用自动写入
              </button>
            </div>
          </div>
        </div>
      )}
      {/* AI-generated content disclaimer under the composer (chat + hero) */}
      <div className="ai-disclaimer">内容由 AI 生成，请核实重要信息</div>
    </div>
  );
}

// Model picker button + grouped menu; onPick reports the chosen pair.
export function ModelPicker({
  binding,
  onPick,
  compact,
  filter,
}: {
  binding: SessionBinding | undefined;
  onPick: (b: SessionBinding) => void;
  compact?: boolean;
  /** Optional model-name filter (e.g. image-only mode). */
  filter?: (model: string) => boolean;
}) {
  const config = useApp((s) => s.config);
  const [open, setOpen] = useState(false);
  const [dropUp, setDropUp] = useState(true);
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const close = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", close);
    return () => document.removeEventListener("mousedown", close);
  }, [open]);

  const toggle = () => {
    if (!open) {
      const rect = ref.current?.getBoundingClientRect();
      if (rect) {
        const spaceBelow = window.innerHeight - rect.bottom;
        setDropUp(rect.top >= spaceBelow);
      }
    }
    setOpen((o) => !o);
  };

  const providers = (config?.providers ?? []).filter(
    (p) => p.enabled && (!filter || p.models.some(filter))
  );

  return (
    <div className="model-pick" ref={ref}>
      <button className="mp-btn" onClick={toggle} title="切换模型">
        {binding?.model ? (
          <ModelMark model={binding.model} size={13} />
        ) : (
          <BrandMark provider={providers.find((x) => x.id === binding?.provider_id)} size={13} />
        )}
        {binding ? (
          <span className="mono" style={{ fontSize: compact ? 11 : 12 }}>
            {binding.model}
          </span>
        ) : (
          <span>选择模型…</span>
        )}
        <span style={{ fontSize: 9, opacity: 0.6 }}>▾</span>
      </button>
      {open && (
        <div className={`mp-menu ${dropUp ? "up" : "down"}`}>
          {providers.length === 0 && (
            <div style={{ padding: "10px 12px", fontSize: 12.5, color: "var(--text-faint)" }}>
              没有可用的 Provider —— 请先到「模型管理」添加并填入 API Key。
            </div>
          )}
          {providers.map((p) => (
            <div key={p.id}>
              <div className="mp-group">
                <span style={{ display: "inline-flex", alignItems: "center", marginRight: 5 }}>
                  <BrandMark provider={p} size={12} />
                </span>
                {p.name}
              </div>
              {p.models.filter((m) => !filter || filter(m)).map((m) => (
                <button
                  key={m}
                  className={`mp-item ${binding?.provider_id === p.id && binding?.model === m ? "selected" : ""}`}
                  onClick={() => {
                    onPick({ provider_id: p.id, model: m });
                    setOpen(false);
                  }}
                >
                  <ModelMark model={m} size={13} />
                  <span className="mono">{m}</span>
                </button>
              ))}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
