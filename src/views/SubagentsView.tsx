// Subagent profiles: named roles for delegate_subagent — dedicated
// provider/model binding + role system prompt. Enabled profiles are
// advertised to the parent model in the delegate tool description.
import { useState } from "react";
import { useApp } from "../store";
import { Dropdown } from "../components/Dropdown";
import { Icon } from "../lib/icons";
import type { SubagentProfile } from "../types";

function newId(): string {
  return `sa_${Math.random().toString(36).slice(2, 9)}`;
}

function blank(): SubagentProfile {
  return {
    id: newId(),
    name: "",
    description: "",
    provider_id: "",
    model: "",
    system_prompt: "",
    enabled: true,
  };
}

/** One editable profile card. `draft` mode = inline creation/edit form. */
function SubagentEditor({
  initial,
  onDone,
  onCancel,
}: {
  initial: SubagentProfile;
  onDone: (p: SubagentProfile) => void;
  onCancel: () => void;
}) {
  const { config } = useApp();
  const [d, setD] = useState<SubagentProfile>(initial);
  if (!config) return null;

  const providers = config.providers.filter((p) => p.enabled && p.api_key);
  const provider = providers.find((p) => p.id === d.provider_id);
  const nameTaken = config.subagents.some((x) => x.id !== d.id && x.name === d.name.trim());
  const valid =
    d.name.trim().length > 0 &&
    !nameTaken &&
    d.provider_id.length > 0 &&
    d.model.trim().length > 0;

  return (
    <div className="card" style={{ marginBottom: 14 }}>
      <h3>{initial.name ? `编辑 · ${initial.name}` : "新建子智能体"}</h3>
      <div className="desc">
        具名子智能体会出现在 delegate_subagent 工具说明中，主智能体用 agent
        参数指定角色；不指定时使用主会话的模型配置。子智能体一律只读工具、独立上下文。
      </div>
      <div className="row" style={{ gap: 10, flexWrap: "wrap" }}>
        <label style={{ fontSize: 12, color: "var(--text-dim)" }}>
          名称
          <input
            className="input mono"
            style={{ display: "block", marginTop: 4, width: 180 }}
            value={d.name}
            placeholder="explorer"
            onChange={(e) => setD({ ...d, name: e.target.value })}
          />
        </label>
        <label style={{ fontSize: 12, color: "var(--text-dim)", flex: 1, minWidth: 220 }}>
          一句话说明（会展示给主智能体）
          <input
            className="input"
            style={{ display: "block", marginTop: 4, width: "100%" }}
            value={d.description}
            placeholder="代码库结构探索与接口盘点"
            onChange={(e) => setD({ ...d, description: e.target.value })}
          />
        </label>
      </div>
      <div className="row" style={{ gap: 10, marginTop: 10, flexWrap: "wrap" }}>
        <label style={{ fontSize: 12, color: "var(--text-dim)", minWidth: 220 }}>
          Provider
          <div style={{ marginTop: 4 }}>
            <Dropdown
              value={d.provider_id}
              placeholder="选择 Provider…"
              options={providers.map((p) => ({ value: p.id, label: p.name }))}
              onChange={(v) => {
                const p = providers.find((x) => x.id === v);
                setD({ ...d, provider_id: v, model: p?.models[0] ?? d.model });
              }}
            />
          </div>
        </label>
        <label style={{ fontSize: 12, color: "var(--text-dim)", flex: 1, minWidth: 220 }}>
          模型
          <input
            className="input mono"
            style={{ display: "block", marginTop: 4, width: "100%" }}
            value={d.model}
            placeholder={provider?.models[0] ?? "model-name"}
            onChange={(e) => setD({ ...d, model: e.target.value })}
          />
        </label>
      </div>
      {provider && provider.models.length > 0 && (
        <div className="model-chip-row" style={{ marginTop: 8 }}>
          {provider.models.map((m) => (
            <span
              key={m}
              className="model-chip"
              style={{ cursor: "pointer", color: d.model === m ? "var(--accent)" : undefined }}
              onClick={() => setD({ ...d, model: m })}
            >
              {m}
            </span>
          ))}
        </div>
      )}
      <label style={{ display: "block", fontSize: 12, color: "var(--text-dim)", marginTop: 10 }}>
        角色系统提示词（追加在全局提示词之后）
        <textarea
          className="input"
          style={{ display: "block", marginTop: 4, width: "100%", minHeight: 90, resize: "vertical" }}
          value={d.system_prompt}
          placeholder={"例：你是代码库探索专家。优先用 glob/grep 定位相关文件，输出：1) 关键文件清单 2) 接口与数据流摘要 3) 风险点。不要贴大段代码。"}
          onChange={(e) => setD({ ...d, system_prompt: e.target.value })}
        />
      </label>
      {nameTaken && (
        <div className="hint" style={{ color: "var(--bad)", marginTop: 6 }}>
          名称已存在 —— 子智能体名称需唯一
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

export function SubagentsView() {
  const { config, persistConfig, toast } = useApp();
  const [editing, setEditing] = useState<SubagentProfile | null>(null);

  if (!config) return null;
  const subs = config.subagents ?? [];

  const persist = async (next: SubagentProfile[]) => {
    return persistConfig({ ...config, subagents: next });
  };

  const toggle = (p: SubagentProfile) => {
    void persist(subs.map((x) => (x.id === p.id ? { ...x, enabled: !x.enabled } : x)));
  };

  const remove = (p: SubagentProfile) => {
    void persist(subs.filter((x) => x.id !== p.id));
    toast("info", `已删除 ${p.name}`);
  };

  const save = (p: SubagentProfile) => {
    const trimmed: SubagentProfile = {
      ...p,
      name: p.name.trim(),
      description: p.description.trim(),
      model: p.model.trim(),
    };
    const exists = subs.some((x) => x.id === trimmed.id);
    void persist(exists ? subs.map((x) => (x.id === trimmed.id ? trimmed : x)) : [...subs, trimmed]);
    toast("success", `已保存子智能体 ${trimmed.name}`);
    setEditing(null);
  };

  return (
    <>
      <div className="view-header">
        <div>
          <div className="view-title">子智能体</div>
          <div className="view-sub">
            {subs.length} 个具名角色 · 供 delegate_subagent 工具按名字调用 · 只读工具、独立上下文
          </div>
        </div>
        <div className="spacer" />
        <button className="btn primary" onClick={() => setEditing(blank())}>
          ＋ 新建子智能体
        </button>
      </div>
      <div className="view-body">
        {editing && (
          <SubagentEditor
            key={editing.id}
            initial={editing}
            onDone={save}
            onCancel={() => setEditing(null)}
          />
        )}
        {subs.length === 0 && !editing ? (
          <div className="card" style={{ textAlign: "center", padding: "36px 20px" }}>
            <Icon name="robot" size={26} />
            <div style={{ marginTop: 10, fontSize: 13.5, fontWeight: 600 }}>还没有子智能体</div>
            <div className="desc" style={{ marginTop: 6 }}>
              创建具名角色（如 explorer / reviewer），主智能体委派任务时会使用它的专属模型与提示词。
              未创建时委派沿用主会话配置，功能不受影响。
            </div>
          </div>
        ) : (
          <div className="provider-grid">
            {subs.map((p) => {
              const provider = config.providers.find((x) => x.id === p.provider_id);
              return (
                <div key={p.id} className="card">
                  <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
                    <span className="tc-icon"><Icon name="robot" size={14} /></span>
                    <span className="mono" style={{ fontWeight: 600, fontSize: 13.5 }}>
                      {p.name}
                    </span>
                    {!p.enabled && <span className="chip">已停用</span>}
                    <div className="spacer" />
                    <button
                      className={`switch ${p.enabled ? "on" : ""}`}
                      role="switch"
                      aria-checked={p.enabled}
                      title={p.enabled ? "停用" : "启用"}
                      onClick={() => toggle(p)}
                    />
                  </div>
                  <div className="hint mono" style={{ marginTop: 8 }}>
                    {provider ? provider.name : "⚠ Provider 已失效"} · {p.model || "未设置模型"}
                  </div>
                  {p.description && (
                    <div style={{ marginTop: 8, fontSize: 12.5, color: "var(--text-dim)" }}>
                      {p.description}
                    </div>
                  )}
                  {p.system_prompt && (
                    <details style={{ marginTop: 8 }}>
                      <summary className="hint" style={{ cursor: "pointer" }}>
                        角色提示词 · {p.system_prompt.length} 字
                      </summary>
                      <pre className="mono" style={{ whiteSpace: "pre-wrap", fontSize: 11.5, color: "var(--text-dim)" }}>
                        {p.system_prompt}
                      </pre>
                    </details>
                  )}
                  <div className="row" style={{ gap: 8, marginTop: 12 }}>
                    <button className="btn small" onClick={() => setEditing({ ...p })}>
                      编辑
                    </button>
                    <button className="btn small danger" onClick={() => remove(p)}>
                      删除
                    </button>
                  </div>
                </div>
              );
            })}
          </div>
        )}
      </div>
    </>
  );
}
