// Arena: one prompt, N models streaming in parallel lanes — or a sequential
// round-table where each member sees the earlier replies (group chat mode).
import { useLayoutEffect, useRef, useState } from "react";
import { useApp } from "../store";
import { Composer, ModelPicker } from "../components/Composer";
import { Markdown } from "../components/Markdown";
import { fmtHit, fmtTokens } from "../lib/format";
import { isChatModel } from "../lib/models";
import { ModelMark } from "../lib/lobeIcon";
import { Icon } from "../lib/icons";
import type { MessageRecord } from "../types";

type ArenaMode = "arena" | "roundtable";

function loadMode(): ArenaMode {
  try {
    return localStorage.getItem("ccharness-arena-mode") === "roundtable" ? "roundtable" : "arena";
  } catch {
    return "arena";
  }
}

/** Round-table host moderation (LLM picks the next speaker each round). */
function loadModerated(): boolean {
  try {
    return localStorage.getItem("ccharness-arena-moderated") === "1";
  } catch {
    return false;
  }
}

/** 泳道的历史记录。lane 号是「发送时绑定的下标」，删除/重排泳道后
 *  数字移位，按当前下标匹配会把历史答复挂到别的模型头上 —— 记录自带
 *  模型名，优先按模型身份匹配；模型已不在任何泳道（被换掉）时退回
 *  下标匹配，让旧记录仍显示在该泳道里。 */
function laneHistory(msgs: MessageRecord[], model: string, lane: number) {
  const byModel = msgs.filter((m) => m.role !== "user" && m.model === model);
  if (byModel.length > 0) return byModel;
  return msgs.filter((m) => m.lane === lane && m.role !== "user");
}

export function ArenaView() {
  // 逐字段订阅：整店解构会让任何状态变化（含流式 delta）重渲染全视图
  const activeSessionId = useApp((s) => s.activeSessionId);
  const sessions = useApp((s) => s.sessions);
  const messages = useApp((s) => s.messages);
  const streaming = useApp((s) => s.streaming);
  const busy = useApp((s) => s.busy);
  const sendArena = useApp((s) => s.sendArena);
  const sendGroup = useApp((s) => s.sendGroup);
  const stop = useApp((s) => s.stop);
  const updateBindings = useApp((s) => s.updateBindings);
  const newSession = useApp((s) => s.newSession);
  const setView = useApp((s) => s.setView);
  const [mode, setMode] = useState<ArenaMode>(loadMode);
  const [moderated, setModerated] = useState<boolean>(loadModerated);

  const switchMode = (m: ArenaMode) => {
    setMode(m);
    try {
      localStorage.setItem("ccharness-arena-mode", m);
    } catch {
      /* private browsing — keep in-memory only */
    }
  };

  const switchModerated = (v: boolean) => {
    setModerated(v);
    try {
      localStorage.setItem("ccharness-arena-moderated", v ? "1" : "0");
    } catch {
      /* keep in-memory only */
    }
  };

  const meta = sessions.find((s) => s.id === activeSessionId && s.kind === "arena");
  const msgs = activeSessionId ? messages[activeSessionId] ?? [] : [];
  const live = streaming[activeSessionId ?? ""] ?? [];
  const gridRef = useRef<HTMLDivElement>(null);

  useLayoutEffect(() => {
    // keep lanes pinned to the bottom while streaming
    gridRef.current?.querySelectorAll<HTMLElement>(".lane-body").forEach((el) => {
      el.scrollTop = el.scrollHeight;
    });
  }, [live.map((l) => l.content.length).join(","), msgs.length]);

  if (!meta) {
    return (
      <div className="empty-state">
        <div className="big"><Icon name="arena" size={40} /></div>
        <h3>竞技场</h3>
        <p>同一条提示词，同时发给多个模型并行作答 —— 并排对比质量、速度与缓存命中。</p>
        <div style={{ marginTop: 16 }}>
          <button
            className="btn primary"
            onClick={async () => {
              const lanes =
                sessions.find((s) => s.kind === "arena" && s.bindings.length)?.bindings ??
                sessions.find((s) => s.bindings[0])?.bindings.slice(0, 1) ??
                [];
              await newSession("arena", lanes, "竞技场");
            }}
          >
            新建竞技场
          </button>
          <button className="btn ghost" style={{ marginLeft: 10 }} onClick={() => setView("models")}>
            先去配置模型 →
          </button>
        </div>
      </div>
    );
  }

  const lanes = meta.bindings;
  const isBusy = busy[meta.id] ?? false;
  const lastPrompt = [...msgs].reverse().find((m) => m.role === "user")?.content ?? "";

  return (
    <>
      <div className="view-header">
        <div>
          <div className="view-title">{meta.title}</div>
          <div className="view-sub">
            {lanes.length} {mode === "roundtable" ? "位成员依次发言（圆桌群聊）" : "条泳道并行"}
          </div>
        </div>
        <div className="spacer" />
        <div className="row" style={{ gap: 6 }}>
          <button
            className={`btn small ${mode === "arena" ? "primary" : ""}`}
            title="所有模型同时作答，并排对比"
            onClick={() => switchMode("arena")}
          >
            并行竞技场
          </button>
          <button
            className={`btn small ${mode === "roundtable" ? "primary" : ""}`}
            title="成员按顺序发言，后发言者能看到先前发言（群聊圆桌）"
            onClick={() => switchMode("roundtable")}
          >
            圆桌群聊
          </button>
          {mode === "roundtable" && (
            <button
              className={`btn small ${moderated ? "primary" : ""}`}
              title={
                moderated
                  ? "主持人调度中 —— 每轮由首位成员的模型担任主持人，挑选下一位发言者，直至其宣布结束"
                  : "开启后由 LLM 主持人逐轮挑选下一位发言者（默认按序发言）"
              }
              onClick={() => switchModerated(!moderated)}
            >
              <Icon name="robot" size={13} /> 主持人调度{moderated ? "已开" : ""}
            </button>
          )}
        </div>
        <span className="chip" style={{ marginLeft: 8 }}>
          {mode === "roundtable" ? (moderated ? "主持人挑选发言者" : "成员可见彼此发言") : "每条泳道独立上下文与缓存纪元"}
        </span>
      </div>

      <div className="arena-grid" ref={gridRef}>
        {lanes.length === 0 && (
          <div className="empty-state" style={{ gridColumn: "1 / -1" }}>
            <p>还没有泳道 —— 在下方输入区用「＋」添加要对比的模型。</p>
          </div>
        )}
        {lanes.map((b, i) => {
          // 流事件按当前下标匹配是对的（本次发送用的就是当前顺序）；
          // 历史记录必须走 laneHistory 的模型身份匹配（F3）
          const laneMsgs = laneHistory(msgs, b.model, i);
          const liveLane = live.find((l) => l.lane === i);
          const usage = [...laneMsgs].reverse().find((m) => m.usage)?.usage ?? null;
          const confidence =
            [...laneMsgs].reverse().find((m) => m.confidence != null)?.confidence ?? null;
          return (
            <div className="lane" key={`${b.provider_id}:${b.model}`}>
              <div className="lane-head">
                <ModelMark model={b.model} size={13} />
                <span className="lane-model">{b.model}</span>
                <span className="l-meta">
                  {usage?.input != null && `in ${fmtTokens(usage.input)}`}
                  {usage?.cached != null && usage.input ? ` · 命中 ${fmtHit(usage.cached, usage.input)}` : ""}
                </span>
                <button
                  className="btn small ghost"
                  title="移除泳道"
                  onClick={() => void updateBindings(meta.id, lanes.filter((_, j) => j !== i))}
                  disabled={isBusy}
                >
                  <Icon name="x" size={13} />
                </button>
              </div>
              <div className="lane-body">
                {lastPrompt && <div className="prompt-preview">{lastPrompt}</div>}
                {laneMsgs.map((m) => (
                  <div key={m.id} style={{ marginBottom: 10 }}>
                    {m.reasoning && (
                      <details className="reasoning-box">
                        <summary>思考过程</summary>
                        <div className="r-body">{m.reasoning}</div>
                      </details>
                    )}
                    <Markdown text={m.content} />
                    {m.status === "stopped" && <div className="chip warn">已停止</div>}
                    {m.status === "error" && <div className="chip bad">出错</div>}
                  </div>
                ))}
                {liveLane && (
                  <div>
                    {liveLane.content ? <Markdown text={liveLane.content} /> : <span className="stream-caret" />}
                    {liveLane.content && <span className="stream-caret" />}
                  </div>
                )}
                {!liveLane && laneMsgs.length === 0 && !lastPrompt && (
                  <div style={{ color: "var(--text-faint)", fontSize: 12 }}>等待第一条提示词…</div>
                )}
              </div>
              <div className="lane-foot">
                {usage?.output != null && <span>out {fmtTokens(usage.output)}</span>}
                {confidence != null && <span className="chip" title="上游返回的置信度标记">置信度 {confidence}%</span>}
                <span style={{ marginLeft: "auto" }}>#{i + 1}</span>
              </div>
            </div>
          );
        })}
      </div>

      <Composer
        placeholder={
          mode === "roundtable"
            ? `圆桌提问：${lanes.length} 位成员按顺序发言…`
            : `同时发送到 ${lanes.length} 个模型…`
        }
        disabled={lanes.length === 0}
        busy={isBusy}
        onSend={(t, skillNames, images) =>
          void (mode === "roundtable"
            ? sendGroup(meta.id, t, lanes, skillNames, images, moderated)
            : sendArena(meta.id, t, lanes, skillNames, images))
        }
        onStop={() => void stop(meta.id)}
        sessionId={meta.id}
        contextWindow={
          Math.max(
            8192,
            ...lanes.map(
              (b) =>
                useApp.getState().config?.providers.find((p) => p.id === b.provider_id)
                  ?.context_window ?? 131072
            )
          )
        }
      >
        <ModelPicker
          binding={undefined}
          compact
          filter={isChatModel}
          onPick={(b) => {
            // lane React keys are provider:model — refuse duplicates
            if (lanes.some((l) => l.provider_id === b.provider_id && l.model === b.model)) return;
            void updateBindings(meta.id, [...lanes, b]);
          }}
        />
      </Composer>
    </>
  );
}
