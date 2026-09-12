// Collapsible tool-call cards, used in chat messages and arena lanes.
// Two shapes: persisted (role="tool" record + its call batch) and live
// (streaming tool events with optional pending result).
import { useEffect, useMemo, useState } from "react";
import { Icon } from "../lib/icons";
import { useApp } from "../store";

/** Friendly display names for special tools. */
const TOOL_NAMES: Record<string, string> = {
  delegate_subagent: "委派子智能体",
};

/** File-path tools whose target can be opened in the preview panel. */
const PREVIEWABLE = new Set(["write_file", "edit_file", "read_file"]);

export function ToolCard({
  name,
  args,
  result,
  pending,
  progress,
}: {
  name: string;
  args?: string;
  result?: string;
  pending?: boolean;
  /** Live output streamed from a delegated sub-agent. */
  progress?: string;
}) {
  const setPreview = useApp((s) => s.setPreview);
  const isDelegate = name === "delegate_subagent";
  // extract the workspace-relative path for the preview shortcut
  const previewPath = useMemo(() => {
    if (!PREVIEWABLE.has(name) || !args) return null;
    try {
      const p = JSON.parse(args)?.path;
      return typeof p === "string" && p ? p : null;
    } catch {
      return null;
    }
  }, [name, args]);
  return (
    <details className="tool-card" open={isDelegate && !result && !!progress ? true : undefined}>
      <summary>
        <span className="tc-icon"><Icon name={isDelegate ? "robot" : "wrench"} size={14} /></span>
        <span className="tc-name mono">{TOOL_NAMES[name] ?? name}</span>
        {args && <span className="tc-args mono">{args}</span>}
        <span className={`tc-status ${pending ? "" : result !== undefined ? "ok" : ""}`}>
          {pending ? (progress ? "子任务运行中…" : "执行中…") : result !== undefined ? <Icon name="check" size={13} /> : ""}
        </span>
        {previewPath && result !== undefined && (
          <button
            className="tc-preview"
            title={`在预览区打开 ${previewPath}`}
            onClick={(e) => {
              e.preventDefault(); // don't toggle the <details> card
              e.stopPropagation();
              setPreview({ open: true, tab: "files", file: previewPath });
            }}
          >
            <Icon name="folder" size={12} /> 预览
          </button>
        )}
      </summary>
      {progress && (
        <pre className="tc-progress mono" title="子智能体实时输出">
          {progress}
        </pre>
      )}
      {result && <pre className="tc-result">{result}</pre>}
    </details>
  );
}

/** Renders an assistant batch + its tool records (persisted transcript). */
export function ToolBlock({
  call,
  result,
}: {
  call: { name: string; arguments: string };
  result: string | null;
}) {
  const argsPretty = (() => {
    try {
      return JSON.stringify(JSON.parse(call.arguments));
    } catch {
      return call.arguments;
    }
  })();
  return <ToolCard name={call.name} args={argsPretty} result={result ?? undefined} />;
}

// ---------- subagent progress cards ----------

export interface ToolItemView {
  name: string;
  args?: string;
  result?: string;
  pending?: boolean;
  progress?: string;
  startedAt?: number;
  endedAt?: number;
}

function fmtDuration(ms: number): string {
  const s = Math.round(ms / 1000);
  if (s < 1) return "<1 秒";
  if (s < 60) return `${s} 秒`;
  return `${Math.floor(s / 60)} 分 ${s % 60} 秒`;
}

function taskLine(args?: string): string {
  if (!args) return "子任务";
  try {
    const t = JSON.parse(args)?.task;
    if (typeof t === "string" && t.trim()) {
      return t.trim().split("\n")[0].slice(0, 64);
    }
  } catch {
    /* args not json — fall through */
  }
  return "子任务";
}

/** One delegated sub-agent: status dot + task + timing + live/conclusion. */
function SubagentCard({ t, now }: { t: ToolItemView; now: number }) {
  const running = !!t.pending && t.result === undefined;
  const duration = t.startedAt ? (t.endedAt ?? (running ? now : undefined))! - t.startedAt : null;
  return (
    <div className={`sag-item ${running ? "running" : "done"}`}>
      <div className="sag-item-head">
        <span className="sag-ico">
          <Icon name="robot" size={13} />
          <span className="sag-dot" />
        </span>
        <span className="sag-title mono">子智能体</span>
        <span className="sag-status">
          {running ? "运行中" : `已完成${duration != null ? ` · ${fmtDuration(duration)}` : ""}`}
        </span>
      </div>
      <div className="sag-task" title={taskLine(t.args)}>
        {taskLine(t.args)}
      </div>
      {running ? (
        t.progress ? (
          <details className="sag-live" open>
            <summary>实时输出 · {t.progress.length} 字</summary>
            <pre className="mono">{t.progress}</pre>
          </details>
        ) : (
          <div className="sag-live-empty">等待首个输出…</div>
        )
      ) : t.result ? (
        <details className="sag-result">
          <summary>查看结论 · {t.result.length} 字</summary>
          <pre className="mono">{t.result}</pre>
        </details>
      ) : null}
    </div>
  );
}

/** Main-agent node + connector spine + the delegated sub cards. */
function SubagentGroup({ run }: { run: ToolItemView[] }) {
  const done = run.filter((t) => !(t.pending && t.result === undefined)).length;
  const anyRunning = done < run.length;
  const [, tick] = useState(0);
  useEffect(() => {
    if (!anyRunning) return;
    const h = window.setInterval(() => tick((n) => n + 1), 1000);
    return () => window.clearInterval(h);
  }, [anyRunning]);
  const now = Date.now();
  const starts = run.map((t) => t.startedAt).filter((v): v is number => v != null);
  const ends = run.map((t) => t.endedAt).filter((v): v is number => v != null);
  const total =
    starts.length && Math.max(...ends, anyRunning ? now : 0) > Math.min(...starts)
      ? Math.max(...ends, anyRunning ? now : 0) - Math.min(...starts)
      : null;
  return (
    <details className={`sag-group ${anyRunning ? "running" : "done"}`} open={anyRunning || undefined}>
      <summary>
        <span className="sag-head-title">
          <Icon name="robot" size={14} />
          {anyRunning ? "子智能体执行中" : "子智能体已完成"}
        </span>
        <span className="sag-head-meta mono">
          {run.length} 个 · {done}/{run.length} 完成
          {total != null ? ` · ${fmtDuration(total)}` : ""}
        </span>
      </summary>
      <div className="sag-body">
        <div className="sag-main">
          <span className="sag-main-ico">
            <Icon name="robot" size={16} />
          </span>
          <b>Main agent</b>
          <span className="sag-main-sub">协调 {run.length} 个委派任务</span>
        </div>
        <div className="sag-list">
          {run.map((t, i) => (
            <SubagentCard key={i} t={t} now={now} />
          ))}
        </div>
      </div>
    </details>
  );
}

/** Renders a tool batch, grouping consecutive delegate_subagent calls into
 *  one "Main agent → subagents" progress card (like the reference design). */
export function ToolItems({ items }: { items: ToolItemView[] }) {
  const out: React.ReactNode[] = [];
  let i = 0;
  while (i < items.length) {
    if (items[i].name === "delegate_subagent") {
      let j = i;
      while (j < items.length && items[j].name === "delegate_subagent") j++;
      out.push(<SubagentGroup key={i} run={items.slice(i, j)} />);
      i = j;
    } else {
      const t = items[i];
      out.push(
        <ToolCard
          key={i}
          name={t.name}
          args={t.args}
          result={t.result}
          pending={t.pending}
          progress={t.progress}
        />
      );
      i++;
    }
  }
  return <>{out}</>;
}
