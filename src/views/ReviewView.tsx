// Review view: what the agent actually changed — write diffs with
// before/after snapshots, plus the full tool-call transcript.
// Layout: master-detail two-column (write list → diff detail) with the
// tool-call transcript full-width below; session switcher in the header
// uses the same custom Dropdown as every other page (no native <select>).
import { useEffect, useMemo, useState } from "react";
import { useApp } from "../store";
import { Dropdown } from "../components/Dropdown";
import { lineDiff } from "../lib/diff";
import { fmtTime } from "../lib/format";
import { Icon } from "../lib/icons";
import * as api from "../lib/api";
import type { MessageRecord, WriteLog, WriteLogEntry } from "../types";

const TOOL_LABEL: Record<string, string> = {
  write_file: "写入文件",
  edit_file: "编辑文件",
};

function DiffView({ log }: { log: WriteLog }) {
  const hasBoth = log.before != null && log.after != null;
  const diff = useMemo(
    () => (hasBoth ? lineDiff(log.before!, log.after!) : null),
    [hasBoth, log.before, log.after]
  );
  return (
    <div>
      <div className="desc" style={{ marginBottom: 8 }}>
        {TOOL_LABEL[log.tool] ?? log.tool} · {log.path} ·{" "}
        {log.before == null
          ? "新文件"
          : diff
            ? `+${diff.addCount} / -${diff.delCount} 行`
            : ""}
        {diff?.truncated && " · 文件过大，已降级显示"}
      </div>
      {diff ? (
        <div className="diff-view">
          {diff.lines.map((l, i) => (
            <div key={i} className={`dl ${l.type}`}>
              <span className="ln">{l.type === "same" ? "" : l.type === "add" ? "+" : "-"}</span>
              <span>{l.text || " "}</span>
            </div>
          ))}
        </div>
      ) : (
        <div className="desc">{log.before == null ? "创建的新文件，无 diff。" : "快照不可用。"}</div>
      )}
    </div>
  );
}

/** Tool-call list built from the persisted transcript (newest first). */
function ToolCallList({ msgs }: { msgs: MessageRecord[] }) {
  const argsOf = useMemo(() => {
    const map = new Map<string, string>();
    for (const m of msgs) {
      for (const tc of m.tool_calls ?? []) map.set(tc.id, tc.arguments);
    }
    return map;
  }, [msgs]);

  const calls = useMemo(
    () =>
      msgs
        .filter((m) => m.role === "tool" && m.tool_call_id)
        .slice(-200)
        .reverse(),
    [msgs]
  );

  if (calls.length === 0) {
    return <div className="desc" style={{ marginBottom: 0 }}>本会话还没有工具调用记录。</div>;
  }
  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 8 }}>
      {calls.map((m) => {
        const args = argsOf.get(m.tool_call_id!) ?? "";
        const firstLine = args.split("\n")[0]?.slice(0, 140) ?? "";
        return (
          <details key={m.id} className="review-call">
            <summary>
              <span className="mono">{m.ts && fmtTime(m.ts)}</span>
              <span className="chip mono">{m.content.startsWith("DENIED") ? "被拒绝" : "工具结果"}</span>
              <span className="path">{firstLine || "（无参数）"}</span>
            </summary>
            <div className="rc-body">
              {args && (
                <>
                  <div className="rc-label">参数</div>
                  <pre>{args}</pre>
                </>
              )}
              <div className="rc-label">结果</div>
              <pre>{m.content}</pre>
            </div>
          </details>
        );
      })}
    </div>
  );
}

export function ReviewView() {
  const { sessions, toast } = useApp();
  const chatSessions = sessions.filter((s) => s.kind === "chat");
  const [sel, setSel] = useState<string>("");
  const [writes, setWrites] = useState<WriteLogEntry[]>([]);
  const [picked, setPicked] = useState<WriteLog | null>(null);
  const [msgs, setMsgs] = useState<MessageRecord[]>([]);

  useEffect(() => {
    if (!sel && chatSessions.length > 0) setSel(chatSessions[0].id);
  }, [chatSessions, sel]);

  useEffect(() => {
    if (!sel) return;
    setPicked(null);
    setWrites([]);
    setMsgs([]);
    // alive guard: slow loads for the previous session must not overwrite
    // the (already cleared) state of the newly selected one
    let alive = true;
    void (async () => {
      try {
        const w = await api.listSessionWrites(sel);
        const m = await api.getSessionMessages(sel);
        if (!alive) return;
        setWrites(w);
        setMsgs(m);
      } catch (e) {
        if (alive) toast("error", `审阅数据加载失败: ${String(e)}`);
      }
    })();
    return () => {
      alive = false;
    };
  }, [sel, toast]);

  const pick = async (ts: number) => {
    try {
      setPicked(await api.getWriteDiff(sel, ts));
    } catch (e) {
      toast("error", String(e));
    }
  };

  const selMeta = chatSessions.find((s) => s.id === sel);

  return (
    <>
      <div className="view-header">
        <div>
          <div className="view-title">审阅</div>
          <div className="view-sub">智能体改了什么 —— 写入 diff 与完整工具调用记录，逐项可查</div>
        </div>
        <div className="spacer" />
        <Dropdown
          value={sel}
          placeholder="暂无会话"
          minWidth={220}
          options={chatSessions.map((s) => ({ value: s.id, label: s.title || "未命名" }))}
          onChange={setSel}
        />
      </div>
      <div className="view-body">
        <div className="review-grid">
          <div className="card review-left">
            <h3>写入历史</h3>
            <div className="desc">
              会话{selMeta ? `「${selMeta.title}」` : ""}中所有成功落盘的 write_file / edit_file
              操作{selMeta?.workspace ? `（工作区 ${selMeta.workspace}）` : ""}，点击查看 diff。
            </div>
            {writes.length === 0 ? (
              <div className="desc" style={{ marginBottom: 0 }}>
                暂无写入记录 —— 在会话中绑定工作区并让智能体修改文件后，这里会出现可审阅的 diff。
              </div>
            ) : (
              <div className="review-list">
                {writes.map((w) => (
                  <div
                    key={w.ts}
                    className={`review-row ${picked?.ts === w.ts ? "sel" : ""}`}
                    onClick={() => void pick(w.ts)}
                  >
                    <span className="time">{fmtTime(w.ts)}</span>
                    <span className="chip">{TOOL_LABEL[w.tool] ?? w.tool}</span>
                    <span className="path">{w.path}</span>
                  </div>
                ))}
              </div>
            )}
          </div>

          <div className="card review-detail">
            <h3>Diff 详情</h3>
            {picked ? (
              <DiffView log={picked} />
            ) : (
              <div className="review-detail-empty">
                <Icon name="diff" size={28} />
                <div>从左侧选择一条写入记录，查看前后快照对比。</div>
              </div>
            )}
          </div>
        </div>

        <div className="card">
          <h3>工具调用记录</h3>
          <div className="desc">完整参数与结果（含被权限层拒绝的调用），按时间倒序，最近 200 条。</div>
          <ToolCallList msgs={msgs} />
        </div>
      </div>
    </>
  );
}
