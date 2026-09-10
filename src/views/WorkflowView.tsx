// Declarative workflows (state machines): an ordered list of states, each
// with its own directive (injected ahead of user messages), tool surface
// (none/readonly/full) and auto-advance target. states[0] is the entry state.
// Sessions select a workflow as gate "sm:<id>" and the runtime advances the
// position after each successful lane-0 turn until a terminal state.
import { useState } from "react";
import { useApp } from "../store";
import { Dropdown } from "../components/Dropdown";
import { Icon } from "../lib/icons";
import type { SmState, WorkflowDef } from "../types";

function newId(): string {
  return `wf_${Math.random().toString(36).slice(2, 9)}`;
}

function blankState(): SmState {
  return {
    name: "",
    directive: "",
    tools: "full",
    next: null,
    terminal: false,
    branches: [],
    parallel: [],
  };
}

function blank(): WorkflowDef {
  return {
    id: newId(),
    name: "",
    description: "",
    enabled: true,
    states: [
      { name: "调研", directive: "先用只读工具调研现状，再输出简要发现。", tools: "readonly", next: "实施", terminal: false, branches: [], parallel: [] },
      { name: "实施", directive: "按调研结论实施改动，每步说明改了什么。", tools: "full", next: null, terminal: true, branches: [], parallel: [] },
    ],
  };
}

const TOOLS_OPTIONS = [
  { value: "none", label: "无工具" },
  { value: "readonly", label: "只读工具" },
  { value: "full", label: "全部工具" },
];

const TOOLS_LABEL: Record<string, string> = { none: "无工具", readonly: "只读", full: "全部" };

/** One editable workflow card. `draft` mode = inline creation/edit form. */
function WorkflowEditor({
  initial,
  onDone,
  onCancel,
}: {
  initial: WorkflowDef;
  onDone: (w: WorkflowDef) => void;
  onCancel: () => void;
}) {
  const { config } = useApp();
  const [d, setD] = useState<WorkflowDef>(initial);
  if (!config) return null;

  const nameTaken = (config.workflows ?? []).some((x) => x.id !== d.id && x.name === d.name.trim());
  const stateNames = d.states.map((s) => s.name.trim());
  const dupState = new Set(stateNames).size !== stateNames.length;
  const emptyState = stateNames.some((n) => n.length === 0);
  const danglingNext = d.states.some(
    (s) => s.next != null && !stateNames.includes(s.next)
  );
  // empty goto = rule kept but inert (never matches a state, safely ignored)
  const danglingBranch = d.states.some((s) =>
    (s.branches ?? []).some(
      (b) => b.goto.trim() !== "" && !stateNames.includes(b.goto.trim())
    )
  );
  const danglingParallel = d.states.some((s) =>
    (s.parallel ?? []).some((n) => !stateNames.includes(n))
  );
  const valid =
    d.name.trim().length > 0 &&
    !nameTaken &&
    d.states.length > 0 &&
    !emptyState &&
    !dupState &&
    !danglingNext &&
    !danglingBranch &&
    !danglingParallel;

  const setState = (i: number, patch: Partial<SmState>) => {
    setD({ ...d, states: d.states.map((s, j) => (j === i ? { ...s, ...patch } : s)) });
  };

  return (
    <div className="card" style={{ marginBottom: 14 }}>
      <h3>{initial.name ? `编辑 · ${initial.name}` : "新建工作流"}</h3>
      <div className="desc">
        状态机工作流：会话进入后停在首个状态；每个状态把自己的指令注入每轮请求、并限制工具面；回合成功后先评估条件分支（首个命中生效），未命中再按
        next 推进，终止状态停止。配置了并行扇出的状态会在回合干净结束后以子代理并发执行所选状态的任务，再综合一轮。可在会话进度条上手动跳转。
      </div>
      <div className="row" style={{ gap: 10, marginTop: 10, flexWrap: "wrap" }}>
        <label style={{ fontSize: 12, color: "var(--text-dim)" }}>
          名称
          <input
            className="input mono"
            style={{ display: "block", marginTop: 4, width: 200 }}
            value={d.name}
            placeholder="修复流水线"
            onChange={(e) => setD({ ...d, name: e.target.value })}
          />
        </label>
        <label style={{ fontSize: 12, color: "var(--text-dim)", flex: 1, minWidth: 240 }}>
          一句话说明
          <input
            className="input"
            style={{ display: "block", marginTop: 4, width: "100%" }}
            value={d.description}
            placeholder="调研 → 修复 → 验证 → 总结"
            onChange={(e) => setD({ ...d, description: e.target.value })}
          />
        </label>
      </div>

      <div style={{ marginTop: 14, display: "flex", flexDirection: "column", gap: 10 }}>
        {d.states.map((s, i) => (
          <div
            key={i}
            style={{
              border: "1px solid var(--border)",
              borderRadius: "var(--r-md, 10px)",
              padding: "10px 12px",
            }}
          >
            <div className="row" style={{ gap: 10, flexWrap: "wrap", alignItems: "flex-end" }}>
              <span className="chip mono" title="列表顺序即推进顺序，首个为入口状态">
                {i === 0 ? "入口" : `#${i + 1}`}
              </span>
              <label style={{ fontSize: 12, color: "var(--text-dim)" }}>
                状态名
                <input
                  className="input mono"
                  style={{ display: "block", marginTop: 4, width: 140 }}
                  value={s.name}
                  placeholder="调研"
                  onChange={(e) => setState(i, { name: e.target.value })}
                />
              </label>
              <label style={{ fontSize: 12, color: "var(--text-dim)", minWidth: 140 }}>
                工具面
                <div style={{ marginTop: 4 }}>
                  <Dropdown
                    value={s.tools}
                    options={TOOLS_OPTIONS}
                    onChange={(v) => setState(i, { tools: v as SmState["tools"] })}
                    minWidth={128}
                  />
                </div>
              </label>
              <label style={{ fontSize: 12, color: "var(--text-dim)", minWidth: 150 }}>
                成功后推进到
                <div style={{ marginTop: 4 }}>
                  <Dropdown
                    value={s.terminal || s.next == null ? "" : s.next}
                    options={[
                      { value: "", label: s.terminal ? "（终止状态）" : "（保持不变）" },
                      ...d.states
                        .filter((_, j) => j !== i)
                        .map((x) => ({ value: x.name.trim(), label: x.name.trim() || "（未命名）" })),
                    ]}
                    onChange={(v) =>
                      setState(i, { next: v === "" ? null : v, terminal: false })
                    }
                    minWidth={132}
                  />
                </div>
              </label>
              <label
                style={{
                  fontSize: 12,
                  color: "var(--text-dim)",
                  display: "flex",
                  alignItems: "center",
                  gap: 6,
                  paddingBottom: 6,
                }}
                title="终止状态不再自动推进；会话可手动跳转或切回其他模式"
              >
                <input
                  type="checkbox"
                  checked={s.terminal}
                  onChange={(e) => setState(i, { terminal: e.target.checked })}
                />
                终止状态
              </label>
              <div className="spacer" />
              <button
                className="btn small danger"
                disabled={d.states.length <= 1}
                title={d.states.length <= 1 ? "至少保留一个状态" : "删除该状态"}
                onClick={() => setD({ ...d, states: d.states.filter((_, j) => j !== i) })}
              >
                删除
              </button>
            </div>
            <label
              style={{ display: "block", fontSize: 12, color: "var(--text-dim)", marginTop: 8 }}
            >
              状态指令（注入在该状态下发出的每条用户消息之前）
              <textarea
                className="input"
                style={{ display: "block", marginTop: 4, width: "100%", minHeight: 56, resize: "vertical" }}
                value={s.directive}
                placeholder="例：只调研不修改，输出问题清单与根因分析。"
                onChange={(e) => setState(i, { directive: e.target.value })}
              />
            </label>
            {/* parallel fan-out picker */}
            <div style={{ marginTop: 10, fontSize: 12, color: "var(--text-dim)" }}>
              并行扇出
              <span title="本状态回合干净结束后，所选状态会以子代理并发执行各自任务，结果回填后由模型综合一轮；适合无依赖的并行调研或多方案对比" style={{ marginLeft: 6, opacity: 0.7 }}>
                ⓘ
              </span>
              <div className="row" style={{ gap: 6, flexWrap: "wrap", marginTop: 6 }}>
                {d.states
                  .filter((_, j) => j !== i)
                  .map((x) => {
                    const nm = x.name.trim();
                    const on = (s.parallel ?? []).includes(nm);
                    return (
                      <button
                        key={nm}
                        type="button"
                        className="chip mono"
                        style={{
                          cursor: "pointer",
                          opacity: on ? 1 : 0.55,
                          borderColor: on ? "var(--accent, #4a7dff)" : undefined,
                        }}
                        title={on ? "点击移出并行扇出" : "点击加入并行扇出"}
                        onClick={() =>
                          setState(i, {
                            parallel: on
                              ? (s.parallel ?? []).filter((n) => n !== nm)
                              : [...(s.parallel ?? []), nm],
                          })
                        }
                      >
                        {nm || "（未命名）"}
                      </button>
                    );
                  })}
                {(s.parallel ?? []).length === 0 && (
                  <span style={{ opacity: 0.6 }}>未选择 —— 正常推进，不并行</span>
                )}
              </div>
            </div>
            {/* conditional branches */}
            <div style={{ marginTop: 10, fontSize: 12, color: "var(--text-dim)" }}>
              条件分支（按顺序评估，首个命中的跳转生效，全部未命中回落到"成功后推进到"）
              <span
                title="语法：contains:文本 / not_contains:文本（对回复文本大小写不敏感匹配）· tool_used:工具名（本回合调用过该工具）· ok / error · always 或留空（恒命中）"
                style={{ marginLeft: 6, opacity: 0.7, cursor: "help" }}
              >
                ⓘ
              </span>
              {(s.branches ?? []).map((b, bi) => (
                <div key={bi} className="row" style={{ gap: 6, marginTop: 6, flexWrap: "wrap" }}>
                  <input
                    className="input mono"
                    style={{ flex: 1, minWidth: 200 }}
                    value={b.when}
                    placeholder="例：contains:失败 · tool_used:write_file · ok"
                    onChange={(e) =>
                      setState(i, {
                        branches: (s.branches ?? []).map((x, j) =>
                          j === bi ? { ...x, when: e.target.value } : x
                        ),
                      })
                    }
                  />
                  <Dropdown
                    value={b.goto}
                    options={d.states
                      .filter((_, j) => j !== i)
                      .map((x) => ({ value: x.name.trim(), label: x.name.trim() || "（未命名）" }))}
                    onChange={(v) =>
                      setState(i, {
                        branches: (s.branches ?? []).map((x, j) => (j === bi ? { ...x, goto: v } : x)),
                      })
                    }
                    minWidth={120}
                  />
                  <button
                    className="btn small danger"
                    title="删除该分支"
                    onClick={() =>
                      setState(i, { branches: (s.branches ?? []).filter((_, j) => j !== bi) })
                    }
                  >
                    ×
                  </button>
                </div>
              ))}
              <button
                className="btn small"
                style={{ marginTop: 6 }}
                onClick={() =>
                  setState(i, {
                    branches: [
                      ...(s.branches ?? []),
                      { when: "", goto: d.states.find((_, j) => j !== i)?.name.trim() ?? "" },
                    ],
                  })
                }
              >
                ＋ 添加分支
              </button>
            </div>
          </div>
        ))}
      </div>

      <button
        className="btn small"
        style={{ marginTop: 10 }}
        onClick={() => setD({ ...d, states: [...d.states, blankState()] })}
      >
        ＋ 添加状态
      </button>

      {(nameTaken || emptyState || dupState || danglingNext || danglingBranch || danglingParallel) && (
        <div className="hint" style={{ color: "var(--bad)", marginTop: 8 }}>
          {nameTaken && "名称已存在 —— 工作流名称需唯一；"}
          {emptyState && "存在未命名的状态；"}
          {dupState && "状态名重复；"}
          {danglingNext && "有状态推进到不存在的状态名；"}
          {danglingBranch && "有条件分支指向不存在的状态名；"}
          {danglingParallel && "有并行扇出指向不存在的状态名。"}
        </div>
      )}
      <div className="row" style={{ gap: 8, marginTop: 12 }}>
        <button className="btn small primary" disabled={!valid} onClick={() => onDone(d)}>
          保存
        </button>
        <button className="btn small" onClick={onCancel}>
          取消
        </button>
      </div>
    </div>
  );
}

export function WorkflowView() {
  const { config, persistConfig, toast } = useApp();
  const [editing, setEditing] = useState<WorkflowDef | null>(null);

  if (!config) return null;
  const defs = config.workflows ?? [];

  const persist = async (next: WorkflowDef[]) => {
    return persistConfig({ ...config, workflows: next });
  };

  const toggle = (w: WorkflowDef) => {
    void persist(defs.map((x) => (x.id === w.id ? { ...x, enabled: !x.enabled } : x)));
  };

  const remove = (w: WorkflowDef) => {
    void persist(defs.filter((x) => x.id !== w.id));
    toast("info", `已删除 ${w.name}（正在使用它的会话会回退到普通对话模式）`);
  };

  const save = (w: WorkflowDef) => {
    const trimmed: WorkflowDef = {
      ...w,
      name: w.name.trim(),
      description: w.description.trim(),
      states: w.states.map((s) => ({
        ...s,
        name: s.name.trim(),
        directive: s.directive,
        next: s.next != null ? s.next.trim() : null,
        branches: (s.branches ?? []).map((b) => ({ when: b.when.trim(), goto: b.goto.trim() })),
        parallel: (s.parallel ?? []).map((n) => n.trim()),
      })),
    };
    const exists = defs.some((x) => x.id === trimmed.id);
    void persist(
      exists ? defs.map((x) => (x.id === trimmed.id ? trimmed : x)) : [...defs, trimmed]
    );
    toast("success", `已保存工作流 ${trimmed.name}`);
    setEditing(null);
  };

  return (
    <>
      <div className="view-header">
        <div>
          <div className="view-title">工作流</div>
          <div className="view-sub">
            {defs.length} 个声明式状态机 · 会话选择后按状态推进 · 每个状态独立指令与工具面
          </div>
        </div>
        <div className="spacer" />
        <button className="btn primary" onClick={() => setEditing(blank())}>
          ＋ 新建工作流
        </button>
      </div>
      <div className="view-body">
        {editing && (
          <WorkflowEditor
            key={editing.id}
            initial={editing}
            onDone={save}
            onCancel={() => setEditing(null)}
          />
        )}
        {defs.length === 0 && !editing ? (
          <div className="card" style={{ textAlign: "center", padding: "36px 20px" }}>
            <Icon name="branch" size={26} />
            <div style={{ marginTop: 10, fontSize: 13.5, fontWeight: 600 }}>还没有工作流</div>
            <div className="desc" style={{ marginTop: 6 }}>
              创建状态机工作流（如 调研 → 实施 → 验证 → 总结），在会话输入框的工作流下拉中选择启用；
              回合成功后自动推进，进度条可手动跳转。
            </div>
          </div>
        ) : (
          <div className="provider-grid">
            {defs.map((w) => (
              <div key={w.id} className="card">
                <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
                  <span className="tc-icon">
                    <Icon name="branch" size={14} />
                  </span>
                  <span className="mono" style={{ fontWeight: 600, fontSize: 13.5 }}>
                    {w.name}
                  </span>
                  {!w.enabled && <span className="chip">已停用</span>}
                  <div className="spacer" />
                  <button
                    className={`switch ${w.enabled ? "on" : ""}`}
                    role="switch"
                    aria-checked={w.enabled}
                    title={w.enabled ? "停用" : "启用"}
                    onClick={() => toggle(w)}
                  />
                </div>
                {w.description && (
                  <div style={{ marginTop: 8, fontSize: 12.5, color: "var(--text-dim)" }}>
                    {w.description}
                  </div>
                )}
                <div className="model-chip-row" style={{ marginTop: 10 }}>
                  {w.states.map((s, i) => (
                    <span
                      key={i}
                      className="model-chip"
                      title={`${TOOLS_LABEL[s.tools] ?? s.tools}${s.terminal ? " · 终止" : s.next ? ` → ${s.next}` : ""}`}
                    >
                      {i === 0 ? "▶ " : ""}
                      {s.name}
                      {i < w.states.length - 1 ? " →" : ""}
                    </span>
                  ))}
                </div>
                <div className="row" style={{ gap: 8, marginTop: 12 }}>
                  <button className="btn small" onClick={() => setEditing({ ...w, states: w.states.map((s) => ({ ...s })) })}>
                    编辑
                  </button>
                  <button className="btn small danger" onClick={() => remove(w)}>
                    删除
                  </button>
                </div>
              </div>
            ))}
          </div>
        )}
      </div>
    </>
  );
}
