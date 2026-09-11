// Provider & model management: presets, CRUD, connection test, model fetch,
// per-model pricing (drives the cost column in telemetry).
import { useState } from "react";
import { useApp } from "../store";
import { PromptDialog } from "../components/Dialog";
import { BrandMark, ModelMark } from "../lib/lobeIcon";
import { Icon } from "../lib/icons";
import { fmtUsd } from "../lib/format";
import * as api from "../lib/api";
import type { Provider, ProviderKind, CacheTier } from "../types";

const EMPTY_BEHAVIOR = { max_output: null, temperature: null, reasoning: null };

interface Preset {
  name: string;
  kind: ProviderKind;
  base_url: string;
  allow_local: boolean;
  note: string;
}

const PRESETS: Preset[] = [
  { name: "DeepSeek", kind: "openai_compatible", base_url: "https://api.deepseek.com/v1", allow_local: false, note: "前缀缓存计费约为输入价 1/10" },
  { name: "智谱 GLM", kind: "openai_compatible", base_url: "https://open.bigmodel.cn/api/paas/v4", allow_local: false, note: "GLM-4 系列" },
  { name: "Moonshot Kimi", kind: "openai_compatible", base_url: "https://api.moonshot.cn/v1", allow_local: false, note: "Kimi 系列" },
  { name: "OpenAI", kind: "openai_compatible", base_url: "https://api.openai.com/v1", allow_local: false, note: "GPT 系列（chat/completions 协议）" },
  { name: "OpenAI Responses", kind: "openai_responses", base_url: "https://api.openai.com/v1", allow_local: false, note: "Responses 协议（GPT-5.x / o 系列原生接口）" },
  { name: "Anthropic", kind: "anthropic", base_url: "https://api.anthropic.com", allow_local: false, note: "Claude 系列" },
  { name: "Azure OpenAI (Responses)", kind: "azure_responses", base_url: "", allow_local: false, note: "v1 数据面：https://<资源名>.openai.azure.com/openai/v1，api-key 认证" },
  { name: "Ollama 本地", kind: "openai_compatible", base_url: "http://localhost:11434/v1", allow_local: true, note: "本地端点，需显式开启本地访问" },
  { name: "自定义 OpenAI 兼容", kind: "openai_compatible", base_url: "", allow_local: false, note: "任意 OpenAI 兼容网关" },
];

const KIND_LABEL: Record<ProviderKind, string> = {
  openai_compatible: "openai-compat",
  openai_responses: "openai-responses",
  azure_responses: "azure-responses",
  anthropic: "anthropic",
};

function newId(): string {
  return `p_${Math.random().toString(36).slice(2, 9)}`;
}

function ProviderCard({ provider: p }: { provider: Provider }) {
  const { config, persistConfig, toast } = useApp();
  const [draft, setDraft] = useState<Provider>(p);
  const [testing, setTesting] = useState(false);
  const [pricingFor, setPricingFor] = useState<string | null>(null);
  const [addingModel, setAddingModel] = useState(false);
  const dirty = JSON.stringify(draft) !== JSON.stringify(p);

  if (!config) return null;

  const save = async () => {
    const ok = await persistConfig({
      ...config,
      providers: config.providers.map((x) => (x.id === p.id ? draft : x)),
    });
    if (ok) toast("success", `已保存 ${draft.name}`);
  };

  const setBehavior = (
    model: string,
    patch: Partial<{ max_output: number | null; temperature: number | null; reasoning: string | null }>,
  ) => {
    const cur = draft.behavior[model] ?? EMPTY_BEHAVIOR;
    setDraft({ ...draft, behavior: { ...draft.behavior, [model]: { ...cur, ...patch } } });
  };

  const remove = async () => {
    const ok = await persistConfig({ ...config, providers: config.providers.filter((x) => x.id !== p.id) });
    if (ok) toast("info", `已删除 ${p.name}`);
  };

  const test = async () => {
    setTesting(true);
    try {
      const r = await api.testProvider(draft);
      if (r.ok) toast("success", `${draft.name}: 连接成功，${r.models.length} 个模型`);
      else toast("error", `${draft.name}: ${r.message}`);
    } catch (e) {
      toast("error", String(e));
    } finally {
      setTesting(false);
    }
  };

  const fetchList = async () => {
    setTesting(true);
    try {
      const models = await api.fetchModels(draft);
      if (models.length === 0) {
        toast("info", "端点未返回任何模型");
      } else {
        setDraft({ ...draft, models: Array.from(new Set([...models])) });
        toast("success", `获取到 ${models.length} 个模型 —— 点击保存写入配置`);
      }
    } catch (e) {
      toast("error", `获取失败: ${String(e)}`);
    } finally {
      setTesting(false);
    }
  };

  const setModelPricing = (model: string, field: "input_per_m" | "cached_per_m" | "output_per_m", v: number) => {
    const cur = draft.pricing[model] ?? { input_per_m: 0, cached_per_m: 0, output_per_m: 0 };
    setDraft({
      ...draft,
      pricing: { ...draft.pricing, [model]: { ...cur, [field]: v } },
    });
  };

  return (
    <div className="provider-card">
      <div className="pc-head">
        <BrandMark provider={p} size={18} />
        <div style={{ flex: 1, minWidth: 0 }}>
          <div className="pc-name">{draft.name}</div>
          <div className="pc-url">{draft.base_url || "（未配置地址）"}</div>
        </div>
        <span className="pc-kind">{draft.kind === "anthropic" ? "anthropic" : "openai-compat"}</span>
        <button
          className={`switch ${draft.enabled ? "on" : ""}`}
          role="switch"
          aria-checked={draft.enabled}
          title={draft.enabled ? "点击停用" : "点击启用"}
          onClick={() => setDraft({ ...draft, enabled: !draft.enabled })}
          style={{ marginLeft: 8 }}
        />
      </div>
      <div className="pc-body">
        <div className="grid-2">
          <div className="field">
            <label>名称</label>
            <input
              value={draft.name}
              placeholder="自定义显示名称"
              title="自定义供应商名称 —— 仅用于界面显示，可随时修改"
              onChange={(e) => setDraft({ ...draft, name: e.target.value })}
              spellCheck={false}
            />
          </div>
          <div className="field">
            <label>Base URL</label>
            <input value={draft.base_url} onChange={(e) => setDraft({ ...draft, base_url: e.target.value })} spellCheck={false} />
          </div>
          <div className="field">
            <label>API Key</label>
            <input
              type="password"
              value={draft.api_key}
              placeholder={
                draft.kind === "anthropic" ? "sk-ant-…" : draft.kind === "azure_responses" ? "Azure 门户密钥" : "sk-…"
              }
              onChange={(e) => setDraft({ ...draft, api_key: e.target.value })}
              spellCheck={false}
            />
          </div>
        </div>

        <div className="row" style={{ marginBottom: 10 }}>
          <span className="row" style={{ gap: 6 }}>
            <button
              className={`switch ${draft.allow_local ? "on" : ""}`}
              role="switch"
              aria-checked={draft.allow_local}
              onClick={() => setDraft({ ...draft, allow_local: !draft.allow_local })}
            />
            <span style={{ fontSize: 12.5, color: "var(--text-dim)" }}>允许本地/内网地址</span>
          </span>
          <span className="row" style={{ gap: 6, marginLeft: 12 }}>
            <span style={{ fontSize: 12.5, color: "var(--text-dim)" }}>上下文窗口</span>
            <input
              style={{ width: 110 }}
              type="number"
              min={0}
              step={1000}
              title="该 Provider 各模型的上下文窗口（token），用于输入区的用量监控；0 或留空按 128k"
              value={draft.context_window ?? 0}
              onChange={(e) =>
                setDraft({ ...draft, context_window: Number(e.target.value) > 0 ? Number(e.target.value) : null })
              }
            />
          </span>
          <span className="hint" style={{ fontSize: 11 }}>
            （默认拒绝环回与内网端点；本地模型如 Ollama 需显式开启）
          </span>
        </div>

        <div className="row" style={{ marginBottom: 10 }}>
          <span className="row" style={{ gap: 6 }}>
            <span style={{ fontSize: 12.5, color: "var(--text-dim)" }}>缓存档位</span>
            <select
              style={{ width: 150 }}
              value={draft.cache_tier ?? "auto"}
              title="缓存 TTL 档位（对齐 pi 的 retention 矩阵）。auto = 协议默认（Anthropic 1h 标记 / OpenAI 仅缓存路由键）；long = 延长窗口（Anthropic ttl:1h / OpenAI 24h 保留）；short = 协议默认窗口；none = 不发任何缓存标记（严格网关最安全）"
              onChange={(e) =>
                setDraft({
                  ...draft,
                  cache_tier: e.target.value === "auto" ? null : (e.target.value as CacheTier),
                })
              }
            >
              <option value="auto">默认（{draft.kind === "anthropic" ? "1h 标记" : "short"}）</option>
              <option value="short">short（5-10 分钟）</option>
              <option value="long">long（Anthropic 1h / OpenAI 24h）</option>
              <option value="none">none（禁用缓存标记）</option>
            </select>
          </span>
          <span className="hint" style={{ fontSize: 11 }}>
            （none 时不发 prompt_cache_key / cache_control；改动档位会重建该 Provider 各会话的缓存纪元）
          </span>
        </div>

        {draft.kind === "openai_compatible" && (
          <div className="row" style={{ marginBottom: 10 }}>
            <span className="row" style={{ gap: 6 }}>
              <span style={{ fontSize: 12.5, color: "var(--text-dim)" }}>图片走 Files API</span>
              <select
                style={{ width: 150 }}
                value={draft.images_via_files ? "on" : "off"}
                title="附件图片一次性上传到 {base}/files（purpose=user_data），后续请求按 file_id 引用，base64 载荷不再重复进请求。上传失败自动回退内联发送"
                onChange={(e) => setDraft({ ...draft, images_via_files: e.target.value === "on" })}
              >
                <option value="off">关闭（内联发送）</option>
                <option value="on">开启（file_id 引用）</option>
              </select>
            </span>
            <span className="hint" style={{ fontSize: 11 }}>
              （仅 OpenAI 兼容接口且服务端实现 /files 时可用，如 DeepSeek；同图同 id，跨轮次与重启保持引用稳定）
            </span>
          </div>
        )}

        <div className="row" style={{ marginBottom: 12 }}>
          <button className="btn small" disabled={testing || !draft.base_url} onClick={test}>
            {testing ? "测试中…" : "测试连接"}
          </button>
          <button className="btn small" disabled={testing || !draft.api_key} onClick={fetchList}>
            获取模型列表
          </button>
          <button className="btn small primary" disabled={!dirty} onClick={save}>
            保存
          </button>
          <button className="btn small danger" onClick={remove} disabled={config.providers.length <= 1}>
            删除
          </button>
        </div>

        <div className="model-chip-row">
          {draft.models.length === 0 && (
            <span className="hint" style={{ fontSize: 11.5 }}>
              暂无模型 —— 填好 Key 后「获取模型列表」，或手动添加
            </span>
          )}
          {draft.models.map((m) => {
            const b = draft.behavior[m];
            const hasBehavior = b && (b.max_output != null || b.temperature != null || (b.reasoning && b.reasoning !== "default"));
            return (
              <span
                key={m}
                className="model-chip"
                title="点击配置定价与行为参数"
                onClick={() => setPricingFor(pricingFor === m ? null : m)}
              >
                <ModelMark model={m} size={13} />
                {m}
                {draft.pricing[m] && draft.pricing[m].input_per_m > 0 ? <Icon name="tag" size={11} /> : ""}
                {hasBehavior ? <Icon name="gear" size={11} /> : ""}
              </span>
            );
          })}
          <button
            className="model-chip"
            style={{ color: "var(--accent)", cursor: "pointer" }}
            onClick={() => setAddingModel(true)}
          >
            ＋ 手动添加
          </button>
        </div>

        {pricingFor && (
          <div className="card" style={{ padding: 12, background: "var(--bg1)" }}>
            <div style={{ fontSize: 12.5, fontWeight: 600, marginBottom: 8, display: "flex", alignItems: "center", gap: 6 }}>
              <ModelMark model={pricingFor} size={14} /> 定价与参数 · {pricingFor}
              <span style={{ color: "var(--text-faint)", fontWeight: 400 }}>（USD / 百万 token，0 表示未知）</span>
            </div>
            <div className="row">
              {(
                [
                  ["input_per_m", "输入"],
                  ["cached_per_m", "缓存命中"],
                  ["output_per_m", "输出"],
                ] as const
              ).map(([field, label]) => (
                <span key={field} className="row" style={{ gap: 6 }}>
                  <span style={{ fontSize: 12, color: "var(--text-dim)" }}>{label}</span>
                  <input
                    style={{ width: 90 }}
                    type="number"
                    min={0}
                    step="0.01"
                    value={draft.pricing[pricingFor]?.[field] ?? 0}
                    onChange={(e) => setModelPricing(pricingFor, field, Number(e.target.value) || 0)}
                  />
                </span>
              ))}
            </div>
            {draft.pricing[pricingFor] && draft.pricing[pricingFor].input_per_m > 0 && (
              <div className="hint" style={{ marginTop: 6, fontSize: 11 }}>
                当前：输入 {fmtUsd(draft.pricing[pricingFor].input_per_m)} / 缓存 {fmtUsd(draft.pricing[pricingFor].cached_per_m)} / 输出{" "}
                {fmtUsd(draft.pricing[pricingFor].output_per_m)} 每百万 token
              </div>
            )}

            <div style={{ fontSize: 12.5, fontWeight: 600, margin: "14px 0 8px" }}>
              行为参数 <span style={{ color: "var(--text-faint)", fontWeight: 400 }}>（留空 = 默认；改动会重建该模型的前缀缓存纪元）</span>
            </div>
            <div className="row" style={{ flexWrap: "wrap" }}>
              <span className="row" style={{ gap: 6 }}>
                <span style={{ fontSize: 12, color: "var(--text-dim)" }}>温度</span>
                <input
                  style={{ width: 80 }}
                  type="number"
                  min={0}
                  max={2}
                  step="0.1"
                  placeholder="默认"
                  title="采样温度；留空使用 Provider 默认"
                  value={draft.behavior[pricingFor]?.temperature ?? ""}
                  onChange={(e) =>
                    setBehavior(pricingFor, {
                      temperature: e.target.value === "" ? null : Math.min(2, Math.max(0, Number(e.target.value))),
                    })
                  }
                />
              </span>
              <span className="row" style={{ gap: 6 }}>
                <span style={{ fontSize: 12, color: "var(--text-dim)" }}>输出上限</span>
                <input
                  style={{ width: 100 }}
                  type="number"
                  min={0}
                  step={256}
                  placeholder="默认"
                  title="max_tokens（输出 token 上限）；Anthropic 路径留空时为 8192"
                  value={draft.behavior[pricingFor]?.max_output ?? ""}
                  onChange={(e) =>
                    setBehavior(pricingFor, {
                      max_output: e.target.value === "" ? null : Math.max(0, Number(e.target.value)),
                    })
                  }
                />
              </span>
              <span className="row" style={{ gap: 6 }}>
                <span style={{ fontSize: 12, color: "var(--text-dim)" }}>推理级别</span>
                <select
                  style={{ width: 100 }}
                  value={draft.behavior[pricingFor]?.reasoning ?? "default"}
                  title="覆盖全局推理级别；default = 跟随全局设置"
                  onChange={(e) => setBehavior(pricingFor, { reasoning: e.target.value })}
                >
                  <option value="default">跟随全局</option>
                  <option value="low">low</option>
                  <option value="medium">medium</option>
                  <option value="high">high</option>
                </select>
              </span>
            </div>
          </div>
        )}

        {addingModel && (
          <PromptDialog
            title={`添加模型 · ${draft.name}`}
            description="输入模型名称，如 deepseek-chat、glm-4-plus"
            placeholder="模型名称"
            validate={(v) =>
              !v ? "模型名称不能为空" : draft.models.includes(v) ? "该模型已存在" : null
            }
            onConfirm={(v) => {
              setDraft({ ...draft, models: [...draft.models, v] });
              setAddingModel(false);
            }}
            onCancel={() => setAddingModel(false)}
          />
        )}
      </div>
    </div>
  );
}

export function ModelsView() {
  const { config, persistConfig, toast } = useApp();
  const [adding, setAdding] = useState(false);
  const [importing, setImporting] = useState(false);

  if (!config) return null;

  const importCcSwitch = async () => {
    setImporting(true);
    try {
      const added = await api.importCcSwitch();
      // the Rust side already persisted the merged config — re-sync the
      // store snapshot so the new cards render
      useApp.setState({ config: await api.getConfig() });
      if (added.length === 0) {
        toast("info", "cc-switch 中没有可导入的新供应商（均已存在或缺少 API Key）");
        return;
      }
      const names = added.map((p) => p.name).join("、");
      toast("success", `从 cc-switch 导入 ${added.length} 个供应商：${names}`);
    } catch (e) {
      toast("error", `导入失败：${String(e)}`);
    } finally {
      setImporting(false);
    }
  };

  const addPreset = async (preset: Preset) => {
    const id = newId();
    const ok = await persistConfig({
      ...config,
      providers: [
        ...config.providers,
        {
          id,
          name: preset.name,
          kind: preset.kind,
          base_url: preset.base_url,
          api_key: "",
          models: [],
          enabled: true,
          allow_local: preset.allow_local,
          context_window: null,
          pricing: {},
          behavior: {},
        },
      ],
    });
    if (!ok) return;
    setAdding(false);
    toast("success", `已添加 ${preset.name} —— 填入 API Key 后即可使用`);
  };

  return (
    <>
      <div className="view-header">
        <div>
          <div className="view-title">模型管理</div>
          <div className="view-sub">{config.providers.length} 个 Provider · API Key 仅存储在本机</div>
        </div>
        <div className="spacer" />
        <button
          className="btn"
          disabled={importing}
          title="从本机 cc-switch 数据库一键导入 Claude / Codex 供应商配置"
          onClick={() => void importCcSwitch()}
        >
          {importing ? "导入中…" : "⇩ 从 cc-switch 导入"}
        </button>
        <button className="btn primary" onClick={() => setAdding((a) => !a)}>
          ＋ 添加 Provider
        </button>
      </div>
      <div className="view-body">
        {adding && (
          <div className="card" style={{ marginBottom: 14 }}>
            <h3>选择预设</h3>
            <div className="desc">预设只填地址与协议类型，Key 一律留空。</div>
            <div className="row">
              {PRESETS.map((p) => (
                <button key={p.name} className="btn small" onClick={() => void addPreset(p)} title={p.note}>
                  {p.name}
                </button>
              ))}
            </div>
          </div>
        )}
        <div className="provider-grid">
          {config.providers.map((p) => (
            <ProviderCard key={p.id} provider={p} />
          ))}
        </div>
      </div>
    </>
  );
}
