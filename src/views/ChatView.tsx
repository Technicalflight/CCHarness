import { useCallback, useEffect, useLayoutEffect, useRef, useState, Fragment } from "react";
import { useApp } from "../store";
import { Markdown } from "../components/Markdown";
import { Composer, ModelPicker } from "../components/Composer";
import { Logo } from "../components/Logo";
import { ToolItems } from "../components/ToolCard";
import { ConfirmDialog, PromptDialog } from "../components/Dialog";
import { fmtHit, fmtTime, fmtTokens, fmtUsd } from "../lib/format";
import { isChatModel, isImageModel } from "../lib/models";
import { BrandMark } from "../lib/lobeIcon";
import { Icon, type IconName } from "../lib/icons";
import { ComposerPet } from "../components/ComposerPet";
import {
  groupTranscript,
  groupAssistants,
  aggregateUsage,
  aggregateCost,
  aggregateConfidence,
  aggregateStatus,
} from "../lib/messages";
import * as api from "../lib/api";
import type { ChatImage, CompactEstimate, CompactionInfo, GoalInfo, MessageRecord, SessionBinding, TelemetrySummary, WtInfo } from "../types";

/** Last path segment, for workspace chips. */
const baseName = (p: string) => p.split(/[\\/]/).filter(Boolean).pop() ?? p;

/** One image inside a user bubble: attachment file names resolve to data
 *  URIs on demand (attachment_data); `data:` URIs (optimistic records)
 *  render as-is without a round-trip. */
function BubbleImg({ sessionId, src }: { sessionId: string; src: string }) {
  const [uri, setUri] = useState<string | null>(src.startsWith("data:") ? src : null);
  const [failed, setFailed] = useState(false);
  useEffect(() => {
    if (src.startsWith("data:")) return;
    let alive = true;
    api
      .attachmentData(sessionId, src)
      .then((u) => {
        if (alive) setUri(u);
      })
      .catch(() => {
        if (alive) setFailed(true);
      });
    return () => {
      alive = false;
    };
  }, [sessionId, src]);
  if (failed) return <span className="img-missing">图片缺失</span>;
  if (!uri) return <span className="img-loading">…</span>;
  return <img className="msg-img" src={uri} alt="附件图片" loading="lazy" />;
}

/** The image strip on a user bubble (m.images = attachment file names). */
function RecordImages({ sessionId, images }: { sessionId: string; images?: string[] }) {
  if (!images || images.length === 0) return null;
  return (
    <div className="msg-imgs">
      {images.map((f, i) => (
        <BubbleImg key={`${f}-${i}`} sessionId={sessionId} src={f} />
      ))}
    </div>
  );
}

function UserItem({
  m,
  sessionId,
  busy,
  editing,
  onEditStart,
  onEditCancel,
  onResend,
  onBranch,
}: {
  m: MessageRecord;
  sessionId: string;
  busy: boolean;
  editing: boolean;
  onEditStart: () => void;
  onEditCancel: () => void;
  onResend: (text: string) => void;
  onBranch: () => void;
}) {
  const [copied, setCopied] = useState(false);
  const [draft, setDraft] = useState("");

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(m.content);
      setCopied(true);
      setTimeout(() => setCopied(false), 1400);
    } catch {
      /* clipboard unavailable */
    }
  };

  if (editing) {
    return (
      <div className="msg user">
        <div className="m-head">
          <span className="m-role">
            <span className="dot" />
            编辑并重发
          </span>
        </div>
        <div className="edit-box">
          <textarea
            autoFocus
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
                e.preventDefault();
                if (draft.trim()) onResend(draft.trim());
              }
              if (e.key === "Escape") onEditCancel();
            }}
          />
          <div className="edit-actions">
            <button className="btn small" onClick={onEditCancel}>
              取消
            </button>
            <button
              className="btn small primary"
              disabled={!draft.trim() || busy}
              title="回滚此消息之后的所有内容，并以新文本重新发送"
              onClick={() => draft.trim() && onResend(draft.trim())}
            >
              重发
            </button>
          </div>
          <div className="edit-hint">重发会回滚这条消息及其后的全部回复 · Ctrl+Enter 发送</div>
        </div>
      </div>
    );
  }

  return (
    <div className="msg user">
      <div className="m-head">
        <span className="m-role">
          <span className="dot" />
          你
        </span>
        <span>{fmtTime(m.ts)}</span>
      </div>
      <div className="m-content">
        {m.skill_calls && m.skill_calls.length > 0 && (
          <div className="skill-chip-row">
            {m.skill_calls.map((n) => (
              <span key={n} className="skill-chip">
                <Icon name="bolt" size={12} /> /{n}
              </span>
            ))}
          </div>
        )}
        <RecordImages sessionId={sessionId} images={m.images} />
        <div style={{ whiteSpace: "pre-wrap", wordBreak: "break-word" }}>{m.content}</div>
      </div>
      <div className="msg-actions">
        <button title="复制" onClick={() => void copy()}>
          {copied ? "已复制" : "复制"}
        </button>
        <button
          title="编辑并重发"
          disabled={busy}
          onClick={() => {
            setDraft(m.content);
            onEditStart();
          }}
        >
          <Icon name="edit" size={13} /> 编辑重发
        </button>
        <button title="从此消息分支一个新会话（原会话不动）" onClick={onBranch}>
          <Icon name="branch" size={13} /> 分支
        </button>
      </div>
    </div>
  );
}

function HitChip({ cached, input }: { cached: number | null; input: number | null }) {
  if (cached == null || input == null || input === 0) return null;
  const ratio = cached / input;
  const tip = "本轮聚合 = Σ缓存命中 / Σ输入（含全部轮次）——首轮带入新消息与工具结果通常未命中，会拉低本轮值；头部徽标是会话累计口径";
  if (ratio >= 0.9) {
    return (
      <span className="chip good" title={tip}>
        前缀命中 {fmtHit(cached, input)}
      </span>
    );
  }
  if (ratio > 0) {
    return (
      <span className="chip warn" title={tip}>
        前缀命中 {fmtHit(cached, input)}
      </span>
    );
  }
  // OpenAI-family providers only cache prompts ≥ 1024 tokens; relays usually
  // inherit that threshold. Below it, cached=0 is the honest expectation.
  if (input < 1024) {
    return (
      <span
        className="chip"
        title={`本次输入 ${input} token，低于 OpenAI 系前缀缓存的最低门槛（≥1024 token），未建缓存属预期。系统提示或对话越长，命中率越高。`}
      >
        前缀 {input} token · 未达缓存门槛
      </span>
    );
  }
  return (
    <span
      className="chip warn"
      title="输入已达缓存门槛但仍 0% 命中：可能是中转网关不转发/不路由上游缓存（多后端轮询天然无法命中），建议直连官方端点对比"
    >
      缓存未命中 0%
    </span>
  );
}

/** Extract the (last) ```plan fenced block from assistant content, if any. */
export function extractPlanBlock(text: string): string | null {
  const matches = [...text.matchAll(/```plan[^\S\n]*\n([\s\S]*?)```/g)];
  const last = matches[matches.length - 1];
  return last ? last[1].trim() : null;
}

/** Extract the (last) ```goal acceptance-checklist block and its done state. */
export function extractGoalBlock(text: string): { body: string; done: boolean } | null {
  const matches = [...text.matchAll(/```goal[^\S\n]*\n([\s\S]*?)```/g)];
  const last = matches[matches.length - 1];
  if (!last) return null;
  return { body: last[1].trim(), done: last[1].includes("GOAL_DONE") };
}

function GoalCard({ body, done }: { body: string; done: boolean }) {
  return (
    <div className={`goal-card ${done ? "done" : ""}`}>
      <div className="goal-head">
        <Icon name="target" size={14} /> 验收清单{done ? " —— 全部达成" : " —— 推进中"}
      </div>
      <div className="goal-body">
        {body
          .replace(/^GOAL_DONE\s*$/m, "")
          .split("\n")
          .filter((l) => l.trim())
          .map((l, i) => {
            const ok = l.trimStart().startsWith("✅");
            return (
              <div key={i} className={`goal-line ${ok ? "ok" : l.trimStart().startsWith("⬜") ? "todo" : ""}`}>
                {l}
              </div>
            );
          })}
      </div>
    </div>
  );
}

function PlanActionBar({
  onApprove,
  onRevise,
}: {
  onApprove: () => void;
  onRevise: () => void;
}) {
  return (
    <div className="plan-card">
      <span className="plan-label"><Icon name="clipboard" size={14} /> 实施方案已就绪 —— 批准前不会修改任何文件</span>
      <div className="plan-actions">
        <button className="btn small ghost" onClick={onRevise}>
          <Icon name="edit" size={13} /> 继续修改
        </button>
        <button className="btn small primary" onClick={onApprove}>
          <Icon name="check" size={13} /> 批准并执行
        </button>
      </div>
    </div>
  );
}

/** Goal-mode summary strip above the composer: renders the PERSISTED goal
 *  state machine (Codex-style five states) with checklist progress x/y, the
 *  session cost against the soft budget, and per-state actions. Pause /
 *  resume / clear are user-only surfaces — the model can never reach them. */
const GOAL_STATUS_META: Record<string, { label: string; cls: string }> = {
  active: { label: "推进中", cls: "" },
  paused: { label: "已暂停", cls: "paused" },
  achieved: { label: "已达成", cls: "done" },
  unmet: { label: "未达成", cls: "failed" },
  budget_limited: { label: "预算收尾", cls: "budget" },
};

function GoalSummaryBar({
  info,
  costUsd,
  budgetUsd,
  gateIsGoal,
  onPause,
  onResume,
  onClear,
}: {
  info: GoalInfo;
  costUsd: number | null;
  budgetUsd: number | null;
  /** current workflow gate is "goal" */
  gateIsGoal: boolean;
  onPause: () => void;
  onResume: () => void;
  onClear: () => void;
}) {
  const st = info.goal?.status ?? "active";
  const s = GOAL_STATUS_META[st] ?? GOAL_STATUS_META.active;
  const overBudget = budgetUsd != null && costUsd != null && costUsd >= budgetUsd;
  const running = st === "active" || st === "budget_limited";
  const progress =
    running && info.checklist_total > 0
      ? ` ${info.checklist_done}/${info.checklist_total}`
      : "";
  const claimed = info.checklist_claimed ?? 0;
  const [tlOpen, setTlOpen] = useState(false);
  const rounds = info.goal_rounds ?? [];
  return (
    <div className={`goal-summary ${s.cls}`}>
      <Icon name="target" size={13} />
      <span className="gs-label">
        目标{s.label}
        {progress}
      </span>
      {claimed > 0 && (
        <span
          className="gs-budget over"
          title="这些 ✅ 行内缺少可核验证据（反引号路径/命令/测试名或括注），按完成审计规则视为未验证声明"
        >
          {claimed} 项证据待补
        </span>
      )}
      {budgetUsd != null && costUsd != null && (
        <span
          className={`gs-budget ${overBudget ? "over" : ""}`}
          title="会话累计成本 / 目标模式成本软上限（设置页可修改）"
        >
          ${costUsd.toFixed(2)} / ${budgetUsd.toFixed(2)}
          {overBudget ? " · 已达上限" : ""}
        </span>
      )}
      {st === "paused" && !gateIsGoal && <span className="gs-flag">目标模式未激活</span>}
      {rounds.length > 0 && (
        <button
          className="btn small ghost"
          onClick={() => setTlOpen((o) => !o)}
          title="按轮次查看目标推进过程（每轮标题 = 当时最靠前的待办）"
        >
          时间线 · {rounds.length} 轮
        </button>
      )}
      <span className="gs-actions" style={{ marginLeft: "auto" }}>
        {running && (
          <button className="btn small ghost" onClick={onPause} title="暂停后不再自动继续，模型也无权恢复">
            暂停
          </button>
        )}
        {(st === "paused" || st === "budget_limited") && (
          <button
            className="btn small ghost"
            onClick={onResume}
            title={st === "paused" && !gateIsGoal ? "切回目标模式并恢复自动继续" : "恢复自动继续"}
          >
            {st === "paused" && !gateIsGoal ? "切回目标模式继续" : "恢复"}
          </button>
        )}
        {(st === "achieved" || st === "unmet") && (
          <button className="btn small ghost" onClick={onResume}>
            重新开启
          </button>
        )}
        <button className="btn small ghost" onClick={onClear} title="移除目标（验收记录保留在消息中）">
          清除
        </button>
      </span>
      {tlOpen && rounds.length > 0 && (
        <div className="goal-timeline" role="list">
          {[...rounds].reverse().map((r) => (
            <div key={r.round} className="gt-row" role="listitem">
              <span className="gt-round">#{r.round}</span>
              <span className="gt-title" title={r.title}>
                {r.title}
              </span>
              <span className="gt-counts">
                {r.done}/{r.total}
                {r.claimed > 0 ? ` · ${r.claimed} 待补` : ""}
              </span>
              <span className="gt-time">{new Date(r.ts).toLocaleTimeString()}</span>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

function AssistantGroup({
  records,
  toolIndex,
  planActions,
  showGoal,
  onBranch,
}: {
  records: MessageRecord[];
  toolIndex: Map<string, string>;
  planActions?: { onApprove: () => void; onRevise: () => void } | null;
  showGoal?: boolean;
  /** Offered only for fully-completed groups (ZCode-style fork anchor). */
  onBranch?: () => void;
}) {
  const assistants = groupAssistants(records);
  if (assistants.length === 0) return null;
  const model = assistants[0].model;
  const usage = aggregateUsage(assistants);
  const cost = aggregateCost(assistants);
  const confidence = aggregateConfidence(assistants);
  const status = aggregateStatus(assistants);
  const rounds = assistants.length;
  const planBlock = planActions
    ? assistants.map((a) => extractPlanBlock(a.content)).reduce<string | null>((acc, x) => x ?? acc, null)
    : null;
  const goal = showGoal
    ? assistants.map((a) => extractGoalBlock(a.content)).reduce<{ body: string; done: boolean } | null>(
        (acc, x) => x ?? acc,
        null
      )
    : null;
  return (
    <div className="msg assistant">
      <div className="m-head">
        <span className="m-role">
          <span className="dot" />
          助手
        </span>
        {model && <span className="chip mono">{model}</span>}
        {rounds > 1 && <span className="chip">{rounds} 轮</span>}
      </div>
      <div className="m-content">
        {assistants.map((a) => (
          <Fragment key={a.id}>
            {a.reasoning && (
              <details className="reasoning-box">
                <summary>思考过程（{a.reasoning.length} 字符）</summary>
                <div className="r-body">{a.reasoning}</div>
              </details>
            )}
            {a.content ? <Markdown text={a.content} /> : null}
            {a.tool_calls && a.tool_calls.length > 0 && (
              <div className="tool-batch">
                <ToolItems
                  items={a.tool_calls.map((tc) => ({
                    name: tc.name,
                    args: (() => {
                      try {
                        return JSON.stringify(JSON.parse(tc.arguments));
                      } catch {
                        return tc.arguments;
                      }
                    })(),
                    result: toolIndex.get(tc.id) ?? undefined,
                  }))}
                />
              </div>
            )}
          </Fragment>
        ))}
        {planBlock && planActions && (
          <PlanActionBar onApprove={planActions.onApprove} onRevise={planActions.onRevise} />
        )}
        {goal && <GoalCard body={goal.body} done={goal.done} />}
      </div>
      <div className="m-meta">
        {onBranch && (
          <button
            className="branch-chip"
            title="从这条回复分叉一个新会话：历史与工作区原样带过去，原会话不动（ZCode 式会话分叉）"
            onClick={onBranch}
          >
            <Icon name="branch" size={12} /> 分叉
          </button>
        )}
        {confidence != null && (
          <span className="chip" title="上游返回的置信度标记（已从正文中剥离）">
            置信度 {confidence}%
          </span>
        )}
        {usage?.input != null && <span>in {fmtTokens(usage.input)}</span>}
        {usage?.output != null && <span>out {fmtTokens(usage.output)}</span>}
        <HitChip cached={usage?.cached ?? null} input={usage?.input ?? null} />
        {cost != null && cost > 0 && <span>{fmtUsd(cost)}</span>}
        {status === "stopped" && <span className="chip warn">已停止</span>}
        {status === "error" && <span className="chip bad">出错</span>}
      </div>
    </div>
  );
}

/** tool_call_id → result text lookup built once per transcript render. */
function buildToolResultIndex(msgs: MessageRecord[]): Map<string, string> {
  const map = new Map<string, string>();
  for (const m of msgs) {
    if (m.role === "tool" && m.tool_call_id) map.set(m.tool_call_id, m.content);
  }
  return map;
}

/** Provider for a model name, shown on the switch divider. */
function providerFor(
  config: ReturnType<typeof useApp.getState>["config"],
  model: string
) {
  return config?.providers.find((pr) => pr.models.includes(model)) ?? null;
}

function ModelDivider({ model }: { model: string }) {
  const p = providerFor(useApp.getState().config, model);
  return (
    <div className="model-divider">
      <span className="md-line" />
      <span className="md-label">
        <BrandMark provider={p} size={12} />
        已切换到 {model}
      </span>
      <span className="md-line" />
    </div>
  );
}

/** Builtin workflow-gate display names for the floating capsule. */
const WF_LABEL: Record<string, string> = { agent: "智能体", plan: "规划", goal: "目标", deep: "深度", review: "审阅", image: "生图" };
/** wf-btn icon follows the active gate (custom SM workflows keep "branch"). */
const WF_ICON: Record<string, IconName> = {
  agent: "robot",
  plan: "clipboard",
  goal: "target",
  deep: "cpu",
  review: "scan",
  image: "image",
};

export function ChatView() {
  const {
    activeSessionId,
    sessions,
    messages,
    streaming,
    busy,
    queue,
    send,
    stop,
    updateBindings,
    setWorkspace,
    ensureFreshMessages,
    newSession,
    setView,
    toast,
    lastRequest,
    refreshSessions,
    selectSession,
    config,
    persistConfig,
  } = useApp();

  const [editingMsg, setEditingMsg] = useState<string | null>(null);
  const [compaction, setCompaction] = useState<CompactionInfo | null>(null);
  const [confirmClear, setConfirmClear] = useState(false);
  const [compactEst, setCompactEst] = useState<CompactEstimate | null>(null);
  const [wfMode, setWfMode] = useState("agent");
  // welcome-hero: the real Composer (full feature parity with the chat page).
  // The user can pre-pick a model; Enter creates the session, then sends.
  const [heroBinding, setHeroBinding] = useState<SessionBinding | null>(null);
  const [heroBusy, setHeroBusy] = useState(false);
  const [heroPerm, setHeroPerm] = useState("approve");
  const [heroWf, setHeroWf] = useState("agent");
  // hero work mode: 对话 (default) | 生图 (image generation — restricted to
  // image-capable models, no workspace/tools)
  const [heroMode, setHeroMode] = useState<"chat" | "image">("chat");
  // keep the pre-picked hero binding compatible with the hero work mode:
  // 对话 lists chat models only, 生图 lists image models only — on a mode
  // swap, auto-pick the first fitting model when the current pick doesn't
  // qualify (no-op when it already fits)
  useEffect(() => {
    const st = useApp.getState();
    const fit = (m: string) => (heroMode === "image" ? isImageModel(m) : isChatModel(m));
    const cur =
      heroBinding ??
      st.sessions.find((s) => s.kind === "chat" && s.bindings[0])?.bindings[0] ??
      null;
    if (cur && fit(cur.model)) return;
    const pick = st.config?.providers.flatMap((p) =>
      p.enabled ? p.models.filter(fit).map((m) => ({ provider_id: p.id, model: m })) : []
    )[0];
    setHeroBinding(pick ?? null);
  }, [heroMode, heroBinding]);
  // welcome-hero workspace pick: held locally, bound to the session the
  // moment the first send creates it (before the message goes out, so
  // AGENTS.md loading and @ refs already apply to that first request)
  const [heroWs, setHeroWs] = useState<string | null>(null);
  const [heroWsMenu, setHeroWsMenu] = useState(false);

  const meta = sessions.find((s) => s.id === activeSessionId && s.kind === "chat");
  // image-generation session (workflow gate persists per session on the
  // backend and is loaded by the effect below) — drives the image-mode
  // composer treatment in the chat view
  const isImageSession = wfMode === "image";
  const msgs = activeSessionId ? messages[activeSessionId] ?? [] : [];
  const lane0 = streaming[activeSessionId ?? ""]?.find((l) => l.lane === 0);
  const scrollRef = useRef<HTMLDivElement>(null);
  const stick = useRef(true);
  const lastSessionRef = useRef<string | null>(null);
  // 划选追问 (selection-ask): mouseup over the transcript with a live
  // selection floats a "追问" button near the selection; picking it pushes
  // a quoted snippet into the composer via the `injected` prop (nonce-keyed)
  const [selAsk, setSelAsk] = useState<{ text: string; x: number; y: number } | null>(null);
  const [composerInject, setComposerInject] = useState<{ text: string; nonce: number } | null>(null);
  // ZCode-style change meter: aggregate +/- lines over the session's writes
  const [changeLines, setChangeLines] = useState<[number, number] | null>(null);
  // compaction state follows the selected session (hook BEFORE any early
  // return — conditional hooks are a Rules-of-Hooks violation)
  const compactionSessionId = activeSessionId;
  useEffect(() => {
    setCompaction(null);
    if (compactionSessionId) {
      void api.getSessionCompaction(compactionSessionId).then(setCompaction).catch(() => {});
    }
  }, [compactionSessionId]);
  // refresh the +/- badge when the transcript grows (a turn that wrote
  // files just ended) or the session switches
  useEffect(() => {
    setChangeLines(null);
    if (!activeSessionId) return;
    void api
      .sessionChangeLines(activeSessionId)
      .then(setChangeLines)
      .catch(() => {});
  }, [activeSessionId, msgs.length]);

  // session-cumulative prefix hit rate for the header badge (Σcached/Σinput
  // over the whole request ledger — same scope as the telemetry page; the
  // per-turn chips are turn-scoped aggregates, so the two don't match by
  // design). Refetched as messages land so it tracks the ongoing turn.
  const [sessHit, setSessHit] = useState<TelemetrySummary | null>(null);
  useEffect(() => {
    if (!activeSessionId) {
      setSessHit(null);
      return;
    }
    let alive = true;
    void api
      .getTelemetry(activeSessionId)
      .then((t) => alive && setSessHit(t.summary))
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [activeSessionId, msgs.length]);

  // workflow gate follows the selected session (in-memory on the backend).
  // Refetched as messages land so a state-machine auto-advance after a
  // successful turn shows up without a manual reload.
  useEffect(() => {
    if (!activeSessionId) return;
    void api
      .getWorkflowMode(activeSessionId)
      .then(setWfMode)
      .catch(() => setWfMode("agent"));
  }, [activeSessionId, msgs.length]);

  // declarative state-machine position: wfMode carries "sm:<def>:<state>"
  const smGate = wfMode.startsWith("sm:") ? wfMode.slice(3) : null;
  const smDefId = smGate ? smGate.split(":")[0] : null;
  const smStateName = smGate ? smGate.split(":").slice(1).join(":") : null;
  const smDef =
    smDefId != null ? (config?.workflows ?? []).find((w) => w.id === smDefId) ?? null : null;
  const smState =
    smDef && smStateName != null ? smDef.states.find((s) => s.name === smStateName) ?? null : null;

  const jumpSmState = async (stateName: string) => {
    if (!activeSessionId || !smDefId) return;
    try {
      await api.smSet(activeSessionId, smDefId, stateName);
      setWfMode(`sm:${smDefId}:${stateName}`);
    } catch (e) {
      toast("error", String(e));
    }
  };

  // floating workflow capsule (top-left of the session composer): popover
  // open state + click-outside close, mirroring the ContextMeter pattern
  const [wfOpen, setWfOpen] = useState(false);
  const wfFloatRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!wfOpen) return;
    const close = (e: MouseEvent) => {
      if (wfFloatRef.current && !wfFloatRef.current.contains(e.target as Node)) setWfOpen(false);
    };
    document.addEventListener("mousedown", close);
    return () => document.removeEventListener("mousedown", close);
  }, [wfOpen]);

  // worktree isolation: status follows the selected session and refreshes as
  // messages land, so the changed-file badge updates right after a turn
  const [wtInfo, setWtInfo] = useState<WtInfo | null>(null);
  const refreshWt = (sid: string) => {
    void api
      .wtInfo(sid)
      .then(setWtInfo)
      .catch(() => {});
  };
  useEffect(() => {
    if (!activeSessionId) {
      setWtInfo(null);
      return;
    }
    refreshWt(activeSessionId);
  }, [activeSessionId, msgs.length]);

  // isolation popover + diff overlay state
  const [wtOpen, setWtOpen] = useState(false);
  const [wtDiffOpen, setWtDiffOpen] = useState(false);
  const [wtDiffText, setWtDiffText] = useState("");
  const modeFloatRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!wtOpen) return;
    const close = (e: MouseEvent) => {
      if (modeFloatRef.current && !modeFloatRef.current.contains(e.target as Node)) setWtOpen(false);
    };
    document.addEventListener("mousedown", close);
    return () => document.removeEventListener("mousedown", close);
  }, [wtOpen]);

  const wtActive = wtInfo?.wt != null;
  const wtCount = wtInfo?.files.length ?? 0;

  const startWt = async () => {
    if (!activeSessionId) return;
    try {
      const st = await api.wtStart(activeSessionId);
      toast("info", `隔离已开启（分支 ${st.branch}）—— 所有工具写入落在 worktree，合并前不动主工作区`);
      refreshWt(activeSessionId);
    } catch (e) {
      toast("error", String(e));
    }
  };
  const mergeWt = async () => {
    if (!activeSessionId) return;
    if (!window.confirm(`将把隔离分支的 ${wtCount} 个文件改动以未提交形式应用回主工作区，并删除 worktree。继续？`)) return;
    setWtOpen(false);
    try {
      const summary = await api.wtMerge(activeSessionId);
      toast("info", summary);
      refreshWt(activeSessionId);
    } catch (e) {
      toast("error", String(e));
    }
  };
  const discardWt = async () => {
    if (!activeSessionId) return;
    if (!window.confirm(`丢弃隔离分支的全部 ${wtCount} 个文件改动？此操作不可恢复。`)) return;
    setWtOpen(false);
    try {
      await api.wtDiscard(activeSessionId);
      toast("info", "已丢弃全部隔离改动，并退出隔离");
      refreshWt(activeSessionId);
    } catch (e) {
      toast("error", String(e));
    }
  };
  const showWtDiff = async () => {
    setWtOpen(false);
    if (!activeSessionId) return;
    try {
      setWtDiffText(await api.wtDiff(activeSessionId));
      setWtDiffOpen(true);
    } catch (e) {
      toast("error", String(e));
    }
  };

  const changeWorkflow = async (mode: string) => {
    if (!activeSessionId) return;
    try {
      await api.setWorkflowMode(activeSessionId, mode);
      setWfMode(mode);
      // keep the binding compatible with the mode: image needs an
      // image-capable model, chat needs a non-image one — auto-pick the
      // first fit when the current pick doesn't qualify
      const wantImage = mode === "image";
      const fit = (m: string) => (wantImage ? isImageModel(m) : isChatModel(m));
      const cur = sessions.find((s) => s.id === activeSessionId)?.bindings[0];
      if (!cur || !fit(cur.model)) {
        const pick = useApp
          .getState()
          .config?.providers.flatMap((p) =>
            p.enabled ? p.models.filter(fit).map((m) => ({ provider_id: p.id, model: m })) : []
          )[0];
        if (pick) {
          await updateBindings(activeSessionId, [pick]);
          toast(
            "info",
            `当前模型不适用于${wantImage ? "生图" : "对话"}模式，已自动切换到 ${pick.model}`
          );
        } else {
          toast("error", `没有可用的${wantImage ? "生图" : "对话"}模型 —— 请先到「模型管理」添加`);
        }
      }
      toast(
        "info",
        mode === "plan"
          ? "已进入规划模式——只读调研 + 输出 ```plan 方案，批准前不会改文件"
          : mode === "goal"
            ? "已进入目标模式——只锁目标与验收标准，路径自选，每轮输出 ```goal 清单"
            : mode === "image"
              ? "已进入生图模式——仅图像模型，不使用工具与工作区"
              : mode.startsWith("sm:")
                ? `已进入工作流「${
                    (useApp.getState().config?.workflows ?? []).find(
                      (w) => w.id === mode.slice(3)
                    )?.name ?? mode.slice(3)
                  }」——会话置于首个状态，回合成功后自动推进`
                : "已切回对话模式——工具与权限档位恢复生效"
      );
    } catch (e) {
      toast("error", String(e));
    }
  };

  const approvePlan = async () => {
    if (!meta) return;
    try {
      await api.setWorkflowMode(meta.id, "agent");
      setWfMode("agent");
      await send(meta.id, "方案已批准，请严格按照上述 ```plan 方案分步执行，每完成一步说明改动内容。");
    } catch (e) {
      toast("error", String(e));
    }
  };

  // goal-mode bounded auto-continue: while the gate is "goal" and the latest
  // finished turn lacks GOAL_DONE, nudge the model onward. Capped at 5
  // nudges; the stop button or any non-ok turn aborts it. When the session
  // cost reaches the configured soft budget, the NEXT nudge becomes a one-
  // shot wrap-up (finish the atomic step + final checklist + progress
  // summary) and auto-continue then stops — a budget_limited soft stop, not
  // a hard cut.
  const autoContRef = useRef(0);
  const goalStoppedRef = useRef(false);
  const [goalPaused, setGoalPaused] = useState(false);
  const [goal, setGoal] = useState<GoalInfo | null>(null);
  const goalBudget = config?.settings.goal_budget_usd ?? null;
  const isBusy = busy[activeSessionId ?? ""] ?? false;
  /** Pull the persisted goal snapshot (state + cost + checklist parse). */
  const refreshGoal = useCallback((sid: string) => {
    if (!sid) return;
    api
      .goalGet(sid)
      .then((g) => setGoal(g))
      .catch(() => setGoal(null));
  }, []);
  useEffect(() => {
    if (!activeSessionId) {
      setGoal(null);
      return;
    }
    refreshGoal(activeSessionId);
  }, [activeSessionId, msgs.length, refreshGoal]);
  useEffect(() => {
    autoContRef.current = 0;
    goalStoppedRef.current = false;
    setGoalPaused(false);
  }, [activeSessionId]);
  useEffect(() => {
    if (wfMode !== "goal" || !activeSessionId || isBusy) return;
    if (goalStoppedRef.current) return;
    const st = goal?.goal?.status;
    // only active/budget_limited sessions auto-continue; paused/achieved/unmet
    // are terminal or user-held states
    if (st !== "active" && st !== "budget_limited") return;
    const assistants = (messages[activeSessionId] ?? []).filter((m) => m.role === "assistant");
    const last = assistants[assistants.length - 1];
    if (!last || last.status !== "ok") return;
    if (extractGoalBlock(last.content)?.done) {
      autoContRef.current = 0;
      // model declared completion → promote the state machine to achieved
      // (the st guard above already excludes achieved, so this fires once)
      void api
        .goalStatus(activeSessionId, "achieved")
        .then(() => refreshGoal(activeSessionId))
        .catch(() => {});
      return;
    }
    const cost = aggregateCost(assistants);
    const overBudget = goalBudget != null && cost != null && cost >= goalBudget;
    // budget reached: exactly one wrap-up nudge (then the status flips to
    // budget_limited, which this branch ignores afterwards)
    if (overBudget && st === "budget_limited") return;
    if (!overBudget && autoContRef.current >= 5) return;
    const budgetNow = overBudget && st === "active";
    if (budgetNow) {
      void api.goalStatus(activeSessionId, "budget_limited").catch(() => {});
    } else {
      autoContRef.current += 1;
    }
    const t = setTimeout(() => {
      if (budgetNow) {
        const b = goalBudget ?? 0;
        const c = cost ?? 0;
        toast("info", `目标模式已达成本预算（$${c.toFixed(2)} / $${b.toFixed(2)}）—— 发送收尾指令`);
        void send(
          activeSessionId,
          "成本预算已达上限。请立即收尾：1) 完成手头正在进行的原子操作（不要中途戛然而止）；2) 输出最终 ```goal 验收清单，如实标注各项状态并附证据；3) 用一段文字总结已完成内容、剩余工作与下一步建议，然后停止——不要再开始新的工作。"
        );
      } else {
        toast("info", `目标模式自动继续（${autoContRef.current}/5）——点击停止可中断`);
        void send(activeSessionId, "继续，按 ```goal 验收清单推进未完成项。");
      }
    }, 800);
    return () => clearTimeout(t);
  }, [isBusy, wfMode, activeSessionId, messages, send, toast, goalBudget, goal, refreshGoal]);

  useLayoutEffect(() => {
    const el = scrollRef.current;
    if (!el) return;
    // switching sessions always snaps to the newest message
    if (lastSessionRef.current !== activeSessionId) {
      lastSessionRef.current = activeSessionId;
      stick.current = true;
    }
    if (stick.current) el.scrollTop = el.scrollHeight;
  }, [
    activeSessionId,
    msgs.length,
    msgs[msgs.length - 1]?.id,
    msgs[msgs.length - 1]?.content.length,
    lane0?.content.length,
    lane0?.reasoning.length,
    lane0?.tools.length,
  ]);

  const heroSend = async (text: string, skillNames: string[], images?: ChatImage[]) => {
    const t = text.trim();
    if (!t || heroBusy) return;
    setHeroBusy(true);
    try {
      let binding: SessionBinding | null;
      if (heroMode === "image") {
        // image mode: the pick must be image-capable; otherwise auto-pick
        // the first image model, and refuse when none exists
        const ok = (b: SessionBinding | null) => !!b && isImageModel(b.model);
        binding = ok(heroBinding) ? heroBinding : null;
        if (!binding) {
          const providers = useApp.getState().config?.providers.filter((p: { enabled: boolean; api_key: string }) => p.enabled && p.api_key) ?? [];
          for (const p of providers) {
            const m = p.models.find(isImageModel);
            if (m) {
              binding = { provider_id: p.id, model: m };
              break;
            }
          }
        }
        if (!binding) {
          toast("error", "未检测到生图模型 —— 请在「模型管理」添加（如 gemini-2.5-flash-image、grok-2-image、seedream）");
          return;
        }
      } else {
        binding =
          heroBinding ?? sessions.find((s) => s.kind === "chat" && s.bindings[0])?.bindings[0] ?? null;
        // chat mode: never let an image-capable pick slip into a chat turn —
        // swap to the first chat model (keep the old pick when none exists;
        // the send then fails with a visible upstream error)
        if (binding && !isChatModel(binding.model)) {
          const pick = useApp
            .getState()
            .config?.providers.flatMap((p) =>
              p.enabled ? p.models.filter(isChatModel).map((m) => ({ provider_id: p.id, model: m })) : []
            )[0];
          binding = pick ?? binding;
        }
      }
      const created = await newSession("chat", binding ? [binding] : [], "新会话");
      // apply the pre-session control picks to the freshly created session
      if (heroMode === "image") {
        // image sessions: no workspace/tools, backend swaps to a minimal
        // image system prompt via the "image" workflow gate
        await api.setWorkflowMode(created.id, "image");
      } else {
        if (heroWs) await setWorkspace(created.id, heroWs);
        if (heroPerm !== "approve") await api.setPermissionMode(created.id, heroPerm);
        if (heroWf !== "agent") await api.setWorkflowMode(created.id, heroWf);
      }
      await send(created.id, t, skillNames, images);
    } catch (e) {
      toast("error", `创建会话失败: ${String(e)}`);
    } finally {
      setHeroBusy(false);
    }
  };

  if (!meta) {
    const firstBinding =
      sessions.find((s) => s.kind === "chat" && s.bindings[0])?.bindings[0] ?? null;
    const recentWs = [
      ...new Set(
        sessions.filter((s) => s.kind === "chat" && s.workspace).map((s) => s.workspace!)
      ),
    ];
    return (
      <div className="hero">
        <Logo size={64} />
        <h2>
          欢迎来到 <span className="cc">CC</span>Harness
        </h2>
        <div className="sub">输入一句话，直接开始新会话</div>

        {/* work mode switch: 对话 | 生图 */}
        <div className="mode-switch" role="tablist" aria-label="工作模式">
          <button
            className={heroMode === "chat" ? "on" : ""}
            role="tab"
            aria-selected={heroMode === "chat"}
            onClick={() => setHeroMode("chat")}
          >
            <Icon name="chat" size={13} /> 对话
          </button>
          <button
            className={heroMode === "image" ? "on" : ""}
            role="tab"
            aria-selected={heroMode === "image"}
            onClick={() => setHeroMode("image")}
          >
            <Icon name="image" size={13} /> 生图
          </button>
        </div>

        {/* workspace strip + composer share one container → guaranteed
            left/right alignment; the strip connects to the composer box */}
        <div className="hero-composer">
          {heroMode === "image" ? (
            <div className="hero-ws-row">
              <span className="hint" style={{ fontSize: 12 }}>
                生图模式 —— 仅显示具备图片生成能力的模型，不绑定工作区与工具
              </span>
            </div>
          ) : (
          <div className="hero-ws-row">
            <button
              className={`ws-chip ${heroWs ? "on" : ""}`}
              title={heroWs ?? "绑定文件夹后启用工作区工具，并加载 AGENTS.md"}
              onClick={() => setHeroWsMenu((o) => !o)}
            >
              <Icon name="folder" size={13} />
              {heroWs ? baseName(heroWs) : "选择工作空间"}
              <span className="tpl-caret">{heroWsMenu ? "▾" : "▸"}</span>
            </button>
            {heroWsMenu && (
              <>
                <div className="ws-overlay" onClick={() => setHeroWsMenu(false)} />
                <div className="ws-menu">
                  {recentWs.length > 0 && (
                    <div className="ws-group">最近使用</div>
                  )}
                  {recentWs.map((w) => (
                    <button
                      key={w}
                      className={`ws-item ${heroWs === w ? "on" : ""}`}
                      title={w}
                      onClick={() => {
                        setHeroWs(w);
                        setHeroWsMenu(false);
                      }}
                    >
                      <Icon name="folder" size={13} /> {baseName(w)}
                    </button>
                  ))}
                  <button
                    className="ws-item"
                    onClick={async () => {
                      setHeroWsMenu(false);
                      const dir = await api.pickDirectory("选择工作区目录");
                      if (dir) setHeroWs(dir);
                    }}
                  >
                    <Icon name="folder" size={13} /> 选择其他目录…
                  </button>
                  {heroWs && (
                    <button
                      className="ws-item"
                      onClick={() => {
                        setHeroWs(null);
                        setHeroWsMenu(false);
                      }}
                    >
                      <Icon name="x" size={13} /> 不绑定工作区
                    </button>
                  )}
                </div>
              </>
            )}
          </div>
          )}
          <Composer
            placeholder={heroMode === "image" ? "描述你想要的图像… Enter 生成" : "问点什么… Enter 发送，Shift+Enter 换行"}
            disabled={false}
            busy={heroBusy}
            onStop={() => {}}
            topSlot={<ComposerPet fallbackRight={12} />}
            onSend={(t, skillNames, images) => {
              void heroSend(t, skillNames, images);
            }}
            workflow={heroMode === "chat" ? { value: heroWf, onChange: setHeroWf } : undefined}
            onPermChange={heroMode === "chat" ? setHeroPerm : undefined}
            imageMode={heroMode === "image"}
          >
            <ModelPicker
              binding={heroBinding ?? firstBinding ?? undefined}
              onPick={setHeroBinding}
              filter={heroMode === "image" ? isImageModel : isChatModel}
            />
          </Composer>
        </div>
        {!firstBinding && (
          <button className="btn ghost small" onClick={() => setView("models")}>
            尚未配置模型 —— 先去添加 Provider →
          </button>
        )}
      </div>
    );
  }

  const binding = meta.bindings[0];
  const hit = lastRequest[meta.id];
  const toolIndex = buildToolResultIndex(msgs);
  const items = groupTranscript(msgs);
  // goal summary inputs: session cost + persisted goal snapshot
  const goalCost = aggregateCost(msgs.filter((m) => m.role === "assistant"));
  const refreshGoalNow = () => refreshGoal(meta.id);
  const pauseGoal = async () => {
    goalStoppedRef.current = true; // guard against an in-flight re-nudge
    setGoalPaused(true);
    try {
      await api.goalStatus(meta.id, "paused");
      toast("info", "目标已暂停 —— 恢复前不再自动继续");
    } catch (e) {
      toast("error", String(e));
    } finally {
      refreshGoalNow();
    }
  };
  const resumeGoal = async () => {
    goalStoppedRef.current = false;
    setGoalPaused(false);
    autoContRef.current = 0;
    try {
      await api.goalStatus(meta.id, "active");
      if (wfMode !== "goal") {
        // gate switch re-arms the auto-continue effect → it nudges on its own
        void changeWorkflow("goal");
      } else {
        void send(meta.id, "继续，按 ```goal 验收清单推进未完成项。");
      }
      toast("success", "目标已恢复推进");
    } catch (e) {
      toast("error", String(e));
    } finally {
      refreshGoalNow();
    }
  };
  const clearGoal = async () => {
    try {
      await api.goalClear(meta.id);
      toast("success", "目标已清除（验收清单保留在消息中）");
    } catch (e) {
      toast("error", String(e));
    } finally {
      refreshGoalNow();
    }
  };

  const resendFrom = async (orig: MessageRecord, newText: string) => {
    setEditingMsg(null);
    try {
      await api.rollbackSession(meta.id, orig.ts);
      await ensureFreshMessages(meta.id);
      await send(meta.id, newText);
    } catch (e) {
      toast("error", `重发失败: ${String(e)}`);
    }
  };

  const branchFrom = async (fromTs: number) => {
    try {
      const branched = await api.branchSession(meta.id, fromTs);
      await refreshSessions();
      await selectSession(branched.id);
      toast("success", `已创建分支会话「${branched.title}」—— 原会话未改动`);
    } catch (e) {
      toast("error", `分支失败: ${String(e)}`);
    }
  };

  const doCompact = async () => {
    try {
      // cost-engineering gate (pi pruning economics): show the rewrite
      // premium vs the per-turn cache-read saving before doing anything
      setCompactEst(await api.compactEstimate(meta.id));
    } catch (e) {
      toast("error", String(e));
    }
  };

  const runCompact = async () => {
    setCompactEst(null);
    try {
      const msg = await api.compactSession(meta.id);
      toast("success", `上下文${msg} —— 可见记录未变，下一条请求从摘要重建（遥测可见新纪元）`);
      setCompaction(await api.getSessionCompaction(meta.id));
    } catch (e) {
      toast("error", String(e));
    }
  };

  const doExport = async () => {
    try {
      const path = await api.exportSession(meta.id);
      toast("success", `已导出: ${path}`);
    } catch (e) {
      toast("error", `导出失败: ${String(e)}`);
    }
  };

  const handleCommand = async (cmd: string, args: string) => {
    switch (cmd) {
      case "compact":
        await doCompact();
        break;
      case "export":
        await doExport();
        break;
      case "clear":
        setConfirmClear(true);
        break;
      case "workspace":
        try {
          await setWorkspace(meta.id, args.trim() || null);
          toast(args.trim() ? "success" : "info", args.trim() ? `已绑定工作区: ${args.trim()}` : "已解除工作区绑定");
        } catch (e) {
          toast("error", String(e));
        }
        break;
      case "skills": {
        const skills = await api.getSkills(meta.workspace);
        toast("info", skills.length ? `可用技能: ${skills.map((s) => "/" + s.name).join("、")}` : "当前没有发现技能");
        break;
      }
      case "wiki": {
        if (!meta.workspace) {
          toast("error", "当前会话未绑定工作区 —— 先用 /workspace <路径> 绑定，再生成仓库导读");
          break;
        }
        toast("info", "正在生成仓库导读 —— 后台子智能体正在扫描工作区（只读，约需十几秒）…");
        try {
          const r = await api.wikiGenerate(meta.id);
          toast("success", `仓库导读已写入 ${r.path}（约 ${r.chars} 字）—— 新会话自动注入上下文`);
        } catch (e) {
          toast("error", `生成失败: ${String(e)}`);
        }
        break;
      }
      case "goal": {
        const a = args.trim();
        if (!a) {
          // no args → status digest
          try {
            const info = await api.goalGet(meta.id);
            if (!info.goal) {
              toast("info", "当前没有目标 —— 用 /goal <目标描述> 创建（建议含范围/约束/完成标准）");
            } else {
              const s = GOAL_STATUS_META[info.goal.status]?.label ?? info.goal.status;
              toast(
                "info",
                `目标【${s}】${info.goal.objective.slice(0, 60)}${
                  info.goal.objective.length > 60 ? "…" : ""
                }（验收 ${info.checklist_done}/${info.checklist_total}）`
              );
            }
          } catch (e) {
            toast("error", String(e));
          }
          break;
        }
        const sub = a.toLowerCase();
        if (sub === "pause") {
          await pauseGoal();
          break;
        }
        if (sub === "resume") {
          await resumeGoal();
          break;
        }
        if (sub === "clear") {
          await clearGoal();
          break;
        }
        if (a.length > 4000) {
          toast("error", "目标描述过长（上限 4000 字符）");
          break;
        }
        try {
          await api.goalSet(meta.id, a);
          toast("success", "目标已创建并激活目标模式 —— 发送消息开始推进");
          if (wfMode !== "goal") void changeWorkflow("goal");
        } catch (e) {
          toast("error", String(e));
        } finally {
          refreshGoalNow();
        }
        break;
      }
      default:
        toast("error", `未知命令 /${cmd}`);
    }
  };

  return (
    <>
      <div className="view-header">
        <div>
          <div className="view-title">{meta.title}</div>
          <div className="view-sub">
            {msgs.length} 条消息 · 工作区 {meta.workspace ? "已绑定" : "未绑定"}
          </div>
        </div>
        <div className="spacer" />
        <button
          className="btn small ghost"
          title="把此前对话折叠为摘要（只影响发给模型的上下文，可见记录不变）；输入达窗口 70% 时也会自动触发"
          onClick={() => void doCompact()}
          disabled={busy[meta.id] ?? false}
        >
          <Icon name="diamond" size={13} /> 压缩上下文
        </button>
        <button
          className="btn small ghost"
          title={`工作区: ${meta.workspace ?? "未绑定"}`}
          onClick={async () => {
            const dir = await api.pickDirectory("选择工作区目录");
            if (!dir) return;
            await setWorkspace(meta.id, dir);
            toast("success", `已绑定工作区: ${dir}`);
          }}
        >
          <Icon name="folder" size={13} /> {meta.workspace ? meta.workspace.split(/[\\/]/).pop() : "绑定工作区"}
        </button>
        {changeLines && (
          <span
            className="chip changes-chip"
            title="本会话写入工具累计改动行数（新建文件整文件计 +；编辑按内容差异估算）"
          >
            <span className="ch-add">+{changeLines[0]}</span> <span className="ch-del">−{changeLines[1]}</span>
          </span>
        )}
        {sessHit && sessHit.total_input > 0 && sessHit.total_cached > 0 ? (
          <span
            className={`chip ${sessHit.total_cached / sessHit.total_input >= 0.9 ? "good" : "warn"}`}
            title="本次会话累计前缀命中率 = Σ缓存命中 / Σ输入（全部请求加权，与遥测页同口径）。各轮气泡上的是单轮聚合值，首轮带新上下文命中较低，两者不同是正常现象"
          >
            前缀命中 {fmtHit(sessHit.total_cached, sessHit.total_input)}
          </span>
        ) : hit && hit.cached_tokens != null && hit.input_tokens ? (
          <HitChip cached={hit.cached_tokens} input={hit.input_tokens} />
        ) : null}
        <button
          className="btn small ghost"
          title="导出为 Markdown"
          onClick={() => void doExport()}
        >
          导出
        </button>
      </div>
      <div
        className="chat-body"
        ref={scrollRef}
        onScroll={(e) => {
          const el = e.currentTarget;
          stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 60;
          setSelAsk(null);
        }}
        onMouseUp={() => {
          const sel = window.getSelection();
          const text = sel?.toString().trim() ?? "";
          if (!sel || sel.isCollapsed || text.length < 2 || text.length > 4000 || sel.rangeCount === 0) {
            setSelAsk(null);
            return;
          }
          const rect = sel.getRangeAt(0).getBoundingClientRect();
          if (!rect || (rect.width === 0 && rect.height === 0)) {
            setSelAsk(null);
            return;
          }
          setSelAsk({ text, x: rect.left + rect.width / 2, y: rect.top });
        }}
      >
        <div className="chat-scroll">
          {items.map((item, i) => {
            const firstTs =
              item.kind === "user" ? item.user!.ts : (item.group![0]?.ts ?? Number.MAX_SAFE_INTEGER);
            const showCompact = compaction != null && firstTs >= compaction.upto_ts && (i === 0 || (() => {
              const prev = items[i - 1];
              const prevTs = prev.kind === "user" ? prev.user!.ts : (prev.group![0]?.ts ?? 0);
              return prevTs < compaction.upto_ts;
            })());
            if (item.kind === "notice") {
              const n = item.notice!;
              return (
                <div
                  key={`n-${n.id}`}
                  className="compact-divider"
                  style={{ color: "var(--warn)" }}
                  title="缓存遥测提示 —— 该记录仅用于展示，永远不会进入模型上下文"
                >
                  <span className="md-line" />
                  <span className="md-label">
                    <Icon name="diamond" size={12} /> {n.content}
                  </span>
                  <span className="md-line" />
                </div>
              );
            }
            if (item.kind === "user") {
              const u = item.user!;
              // divider ABOVE this user message when the reply that follows
              // switched models — so the message and its reply sit together
              // under the new model's banner
              let nextModel: string | null | undefined;
              for (let j = i + 1; j < items.length; j++) {
                if (items[j].kind === "assistantGroup") {
                  nextModel = groupAssistants(items[j].group!)[0]?.model ?? null;
                  break;
                }
              }
              let prevModel: string | null | undefined;
              for (let j = i - 1; j >= 0; j--) {
                if (items[j].kind === "assistantGroup") {
                  prevModel = groupAssistants(items[j].group!)[0]?.model ?? null;
                  break;
                }
              }
              const switched = nextModel != null && prevModel !== undefined && nextModel !== prevModel;
              return (
                <Fragment key={`u-${u.id}`}>
                  {showCompact && compaction && (
                    <div className="compact-divider" title={compaction.summary}>
                      <span className="md-line" />
                      <span className="md-label">
                        <Icon name="diamond" size={12} /> 上下文已压缩 {new Date(compaction.created_at).toLocaleString()} · 此前内容已折叠为摘要（悬停查看）
                      </span>
                      <span className="md-line" />
                    </div>
                  )}
                  {switched && <ModelDivider model={nextModel!} />}
                  <UserItem
                    m={u}
                    sessionId={meta.id}
                    busy={busy[meta.id] ?? false}
                    editing={editingMsg === u.id}
                    onEditStart={() => setEditingMsg(u.id)}
                    onEditCancel={() => setEditingMsg(null)}
                    onResend={(t) => void resendFrom(u, t)}
                    onBranch={() => void branchFrom(u.ts)}
                  />
                </Fragment>
              );
            }
            return (
              <AssistantGroup
                key={`g-${i}`}
                records={item.group!}
                toolIndex={toolIndex}
                showGoal={wfMode === "goal"}
                onBranch={
                  item.group!.every((r) => r.status === "ok")
                    ? () => void branchFrom(Math.max(...item.group!.map((r) => r.ts)))
                    : undefined
                }
                planActions={
                  wfMode === "plan" && !(busy[meta.id] ?? false)
                    ? {
                        onApprove: () => void approvePlan(),
                        onRevise: () =>
                          toast("info", "请直接输入修改意见——规划模式下模型会继续完善方案"),
                      }
                    : null
                }
              />
            );
          })}
          {lane0 && (
            <div className="msg assistant">
              <div className="m-head">
                <span className="m-role">
                  <span className="dot" />
                  助手
                </span>
                <span className="chip mono">{lane0.model}</span>
              </div>
              <div className="m-content">
                {lane0.reasoning && (
                  <details className="reasoning-box" open>
                    <summary>思考中…（{lane0.reasoning.length} 字符）</summary>
                    <div className="r-body">{lane0.reasoning}</div>
                  </details>
                )}
                {lane0.content ? <Markdown text={lane0.content} /> : <span className="stream-caret" />}
                {lane0.content && <span className="stream-caret" />}
              </div>
              {lane0.tools.length > 0 && (
                <div className="tool-batch">
                  <ToolItems
                    items={lane0.tools.map((t) => ({
                      name: t.name,
                      args: t.args,
                      result: t.result,
                      pending: t.result === undefined,
                      progress: t.progress,
                      startedAt: t.startedAt,
                      endedAt: t.endedAt,
                    }))}
                  />
                </div>
              )}
            </div>
          )}
        </div>
      </div>
      {(queue[meta.id]?.length ?? 0) > 0 && (
        <div className="queue-note">
          <Icon name="send" size={13} /> 已排队 {queue[meta.id].length} 条 —— 当前回复结束后依次自动发送
        </div>
      )}
      {smDef && smState && (
        /* declarative state-machine progress bar — chips double as manual
           jump buttons (✓ done · ● current · ○ upcoming) */
        <div className="sm-progress" role="tablist" aria-label="工作流状态">
          <span className="sm-label">
            <Icon name="branch" size={12} /> {smDef.name}
          </span>
          {smDef.states.map((s, i) => {
            const idx = smDef.states.findIndex((x) => x.name === smState.name);
            const done = i < idx;
            const cur = i === idx;
            return (
              <button
                key={s.name}
                role="tab"
                aria-selected={cur}
                className={`sm-chip ${cur ? "cur" : done ? "done" : ""}`}
                title={`${s.name}${s.terminal ? "（终止状态）" : s.next ? ` → ${s.next}` : ""}${
                  s.directive ? `\n${s.directive}` : ""
                }`}
                onClick={() => {
                  if (!cur && !(busy[meta.id] ?? false)) void jumpSmState(s.name);
                }}
                disabled={cur || (busy[meta.id] ?? false)}
              >
                {done ? "✓ " : cur ? "● " : "○ "}
                {s.name}
              </button>
            );
          })}
        </div>
      )}
      {goal?.goal && !isImageSession && (
        <GoalSummaryBar
          info={goal}
          costUsd={goalCost}
          budgetUsd={goalBudget}
          gateIsGoal={wfMode === "goal"}
          onPause={() => void pauseGoal()}
          onResume={() => void resumeGoal()}
          onClear={() => void clearGoal()}
        />
      )}
      {selAsk && (
        <button
          className="sel-ask-btn"
          style={{ left: Math.min(Math.max(selAsk.x - 40, 12), window.innerWidth - 100), top: Math.max(selAsk.y - 40, 12) }}
          title="把选中的内容引用到输入框继续追问"
          onMouseDown={(e) => e.preventDefault()}
          onClick={() => {
            const quote = selAsk.text.length > 600 ? selAsk.text.slice(0, 600) + "…" : selAsk.text;
            setComposerInject({
              text: `针对下面这段内容继续分析：\n> ${quote.replace(/\n/g, "\n> ")}`,
              nonce: Date.now(),
            });
            window.getSelection()?.removeAllRanges();
            setSelAsk(null);
          }}
        >
          <Icon name="spark" size={12} /> 追问
        </button>
      )}
      <Composer
        topSlot={
          <>
            {/* 3D cat lying on the composer's top edge (R3F, procedural) */}
            <ComposerPet />
            {/* floating workflow selector — anchored inside the composer's
                top-left edge; opens a popover listing the builtin gates plus
                every enabled declarative workflow */}
            {!isImageSession && (
              <div className="wf-float" ref={wfFloatRef}>
                <button
                  className="wf-btn"
                  title="工作流 —— 决定每轮对话的行为方式；可切换到自定义状态机工作流"
                  aria-haspopup="menu"
                  aria-expanded={wfOpen}
                  onClick={() => setWfOpen((o) => !o)}
                >
                  <Icon name={smDef ? "branch" : (WF_ICON[wfMode] ?? "branch")} size={13} />
                  {smDef ? smDef.name : WF_LABEL[wfMode] ?? "智能体"}
                  <span className="wf-caret" />
                </button>
                {/* sandbox capsule — lives at the composer's top-left edge,
                    next to the workflow pill; toggles settings.sandbox_mode */}
                <button
                  className={`wf-btn sandbox-pill ${config?.settings.sandbox_mode ? "on" : ""}`}
                  title="沙箱模式 —— 拦截删除类操作与高危命令、禁外网抓取（可在 设置 → 隐私与安全 细化策略）。注意：可能影响任务完成度"
                  aria-pressed={!!config?.settings.sandbox_mode}
                  onClick={() => {
                    if (!config) return;
                    const next = !config.settings.sandbox_mode;
                    void persistConfig({ ...config, settings: { ...config.settings, sandbox_mode: next } });
                    toast(
                      next ? "success" : "info",
                      next
                        ? "沙箱模式已开启 —— 删除/高危命令/外网抓取将被拦截，自动写入降级为逐条审批"
                        : "沙箱模式已关闭 —— 工具操作恢复常规审批策略"
                    );
                  }}
                >
                  <Icon name="shield" size={13} /> 沙箱 {config?.settings.sandbox_mode ? "开" : "关"}
                </button>
                {wfOpen && (
                  <div className="wf-menu" role="menu">
                    {(
                      [
                        { value: "agent", icon: "robot", label: "智能体", hint: "直接执行，工具按权限档位生效" },
                        { value: "plan", icon: "clipboard", label: "规划", hint: "只读调研 + ```plan 方案，批准前不改文件" },
                        { value: "goal", icon: "target", label: "目标", hint: "只锁目标与验收标准，路径自选" },
                        { value: "deep", icon: "cpu", label: "深度推理", hint: "ToT 预演：三方案并行生成 + 评审选优后作答" },
                        { value: "review", icon: "scan", label: "审阅", hint: "三专家并行预审（只读），汇合去重定级输出发现表" },
                      ] as const
                    ).map((o) => {
                      const cur = wfMode === o.value;
                      return (
                        <button
                          key={o.value}
                          role="menuitem"
                          className={`wf-item ${cur ? "on" : ""}`}
                          onClick={() => {
                            setWfOpen(false);
                            if (!cur) void changeWorkflow(o.value);
                          }}
                        >
                          <Icon name={o.icon} size={13} />
                          <span className="wi-label">{o.label}</span>
                          <span className="wi-hint">{o.hint}</span>
                        </button>
                      );
                    })}
                    {(config?.workflows ?? []).some((w) => w.enabled) && (
                      <div className="wf-sep" />
                    )}
                    {(config?.workflows ?? [])
                      .filter((w) => w.enabled)
                      .map((w) => {
                        const cur = smDefId != null && smDefId === w.id;
                        return (
                          <button
                            key={w.id}
                            role="menuitem"
                            className={`wf-item ${cur ? "on" : ""}`}
                            title={w.description || undefined}
                            onClick={() => {
                              setWfOpen(false);
                              if (!cur) void changeWorkflow(`sm:${w.id}`);
                            }}
                          >
                            <Icon name="branch" size={13} />
                            <span className="wi-label">{w.name}</span>
                            <span className="wi-hint">
                              {w.states.map((s) => s.name).join(" → ")}
                            </span>
                          </button>
                        );
                      })}
                  </div>
                )}
              </div>
            )}
            {/* floating work-mode switch — always visible in the chat view so
               an image session can go back to chat AND chat can enter image;
               anchored inside the composer's top-right edge */}
            <div className="mode-float" role="tablist" aria-label="工作模式" ref={modeFloatRef}>
            <button
              className={!isImageSession ? "on" : ""}
              role="tab"
              aria-selected={!isImageSession}
              title="对话模式 —— 工具、工作区与权限档位生效"
              onClick={() => {
                if (isImageSession) void changeWorkflow("agent");
              }}
            >
              <Icon name="chat" size={13} /> 对话
            </button>
            <button
              className={isImageSession ? "on" : ""}
              role="tab"
              aria-selected={isImageSession}
              title="生图模式 —— 仅图像模型，不使用工具与工作区"
              onClick={() => {
                if (!isImageSession) void changeWorkflow("image");
              }}
            >
              <Icon name="image" size={13} /> 生图
            </button>
            {meta?.workspace && !isImageSession && (
              <>
                <span className="mf-sep" />
                <button
                  className={wtActive ? "on" : ""}
                  aria-pressed={wtActive}
                  aria-haspopup={wtActive ? "menu" : undefined}
                  title={
                    wtActive
                      ? "worktree 隔离运行中 —— 点击管理：查看 diff / 合并 / 丢弃"
                      : "开启 worktree 隔离 —— 工具写入落在独立分支工作树，合并前不动主工作区（要求主工作区无未提交改动）"
                  }
                  onClick={() => (wtActive ? setWtOpen((o) => !o) : void startWt())}
                >
                  <Icon name="layers" size={13} /> 隔离
                  {wtCount > 0 && <span className="wt-badge">{wtCount}</span>}
                </button>
              </>
            )}
            {wtActive && wtOpen && wtInfo?.wt && (
              <div className="wf-menu wt-menu" role="menu">
                <div className="wm-head" title={wtInfo.wt.path}>
                  分支 {wtInfo.wt.branch} · 基于 {wtInfo.wt.base_head.slice(0, 7)}
                </div>
                <button className="wf-item" role="menuitem" onClick={() => void showWtDiff()}>
                  <Icon name="diff" size={13} />
                  <span className="wi-label">查看 diff</span>
                  <span className="wi-hint">{wtCount} 个文件有改动</span>
                </button>
                <button className="wf-item" role="menuitem" onClick={() => void mergeWt()}>
                  <Icon name="check" size={13} />
                  <span className="wi-label">合并回主工作区</span>
                  <span className="wi-hint">以未提交改动落盘</span>
                </button>
                <button className="wf-item" role="menuitem" onClick={() => void discardWt()}>
                  <Icon name="trash" size={13} />
                  <span className="wi-label">丢弃并退出隔离</span>
                  <span className="wi-hint">改动不可恢复</span>
                </button>
              </div>
            )}
          </div>
          </>
        }
        placeholder={
          isImageSession
            ? "描述你想要的图像… Enter 生成"
            : binding
              ? `发送到 ${binding.model} …${meta.workspace ? "（@ 可引用工作区文件）" : ""}`
              : "选择模型后开始对话…"
        }
        disabled={!binding}
        busy={busy[meta.id] ?? false}
        injected={composerInject}
        onStop={() => {
          goalStoppedRef.current = true;
          setGoalPaused(true);
          // persist the pause so it survives restarts (state machine)
          void api.goalStatus(meta.id, "paused").then(refreshGoalNow).catch(() => {});
          void stop(meta.id);
        }}
        sessionId={meta.id}
        workspace={meta.workspace}
        onSend={(t, skillNames, images) => {
          autoContRef.current = 0;
          void send(meta.id, t, skillNames, images);
        }}
        onCommand={(cmd, args) => void handleCommand(cmd, args)}
        imageMode={isImageSession}
        contextWindow={
          useApp.getState().config?.providers.find((p) => p.id === binding?.provider_id)
            ?.context_window ?? 131072
        }
      >
        <ModelPicker
          binding={binding}
          onPick={(b) => void updateBindings(meta.id, [b])}
          filter={isImageSession ? isImageModel : isChatModel}
        />
      </Composer>
      {confirmClear && (
        <ConfirmDialog
          title="清空会话"
          description={`将删除本会话的 ${msgs.length} 条消息、遥测记录与压缩摘要（不可恢复）。此操作不影响工作区文件。`}
          confirmText="清空"
          danger
          onCancel={() => setConfirmClear(false)}
          onConfirm={async () => {
            setConfirmClear(false);
            try {
              await api.clearSession(meta.id);
              await ensureFreshMessages(meta.id);
              setCompaction(null);
              toast("info", "会话已清空");
            } catch (e) {
              toast("error", String(e));
            }
          }}
        />
      )}
      {compactEst && (
        <ConfirmDialog
          title="压缩前成本估算"
          description={
            `将折叠 ≈${fmtTokens(compactEst.folded_tokens)} tokens 为 ≈${compactEst.summary_tokens} tokens 的摘要；` +
            (compactEst.rewrite_cost_usd != null && compactEst.save_per_turn_usd != null
              ? `一次性重写成本 ≈ ${fmtUsd(compactEst.rewrite_cost_usd)}（新前缀全价写入），此后每轮节省 ≈ ${fmtUsd(compactEst.save_per_turn_usd)}（被折叠部分不再按缓存价重读）` +
                (compactEst.payback_turns != null ? `，约 ${compactEst.payback_turns} 轮回本。` : "。")
              : "未配置该模型定价，无法折算金额——配置定价后此处会给出成本收益。")
          }
          confirmText="确认压缩"
          onCancel={() => setCompactEst(null)}
          onConfirm={() => void runCompact()}
        />
      )}
      {wtDiffOpen && (
        <div className="wt-diff-overlay" onClick={() => setWtDiffOpen(false)}>
          <div className="wt-diff-panel" onClick={(e) => e.stopPropagation()}>
            <div className="wt-diff-head">
              <Icon name="diff" size={14} />
              <span>隔离分支改动 · {meta?.title}</span>
              <span className="wt-diff-meta">{wtInfo?.wt?.branch}</span>
              <button className="wt-diff-close" aria-label="关闭" onClick={() => setWtDiffOpen(false)}>
                <Icon name="x" size={14} />
              </button>
            </div>
            <pre className="wt-diff-body">
              {wtDiffText.trim()
                ? wtDiffText.split("\n").map((line, i) => (
                    <div
                      key={i}
                      className={
                        line.startsWith("+") && !line.startsWith("+++")
                          ? "dl-add"
                          : line.startsWith("-") && !line.startsWith("---")
                            ? "dl-del"
                            : line.startsWith("@@")
                              ? "dl-hunk"
                              : ""
                      }
                    >
                      {line || " "}
                    </div>
                  ))
                : "（无改动）"}
            </pre>
          </div>
        </div>
      )}
    </>
  );
}
