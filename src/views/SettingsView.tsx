// App settings: theme, keybindings, system prompt.
import { useEffect, useState } from "react";
import { useApp } from "../store";
import { Dropdown } from "../components/Dropdown";
import type { ImportCandidate, SkillInfo } from "../types";
import * as api from "../lib/api";
import { Icon } from "../lib/icons";

const IMPORT_SOURCES = [
  { value: "claude-code", label: "Claude Code" },
  { value: "codex", label: "Codex CLI" },
  { value: "opencode", label: "OpenCode" },
];

export function SettingsView() {
  const { config, persistConfig, toast, refreshSessions, selectSession, runUpdateCheck } = useApp();
  const [skills, setSkills] = useState<SkillInfo[]>([]);
  // update check preference lives in localStorage (client-side only);
  // "off" = disabled, anything else / missing = enabled
  const [autoUpdate, setAutoUpdate] = useState(() => localStorage.getItem("cc.autoUpdateCheck") !== "off");
  const [impSource, setImpSource] = useState("claude-code");
  const [impCands, setImpCands] = useState<ImportCandidate[] | null>(null);
  const [impScanning, setImpScanning] = useState(false);
  const [impCustom, setImpCustom] = useState("");
  const [impBusy, setImpBusy] = useState<string | null>(null);
  useEffect(() => {
    void api.getSkills(null).then(setSkills).catch(() => {});
  }, []);
  if (!config) return null;

  const update = (patch: Partial<typeof config.settings>) => {
    void persistConfig({ ...config, settings: { ...config.settings, ...patch } });
  };

  const scanImport = async () => {
    setImpScanning(true);
    try {
      const cands = await api.importScan(impSource);
      setImpCands(cands);
      toast(cands.length ? "success" : "info", cands.length ? `发现 ${cands.length} 个可导入会话` : "没有找到可导入的会话文件");
    } catch (e) {
      toast("error", `扫描失败: ${String(e)}`);
    } finally {
      setImpScanning(false);
    }
  };

  const importOne = async (source: string, path: string) => {
    setImpBusy(path);
    try {
      const meta = await api.importSession(source, path);
      await refreshSessions();
      await selectSession(meta.id);
      toast("success", `已导入「${meta.title}」—— 在输入框选择模型即可继续`);
    } catch (e) {
      toast("error", `导入失败: ${String(e)}`);
    } finally {
      setImpBusy(null);
    }
  };

  return (
    <>
      <div className="view-header">
        <div>
          <div className="view-title">设置</div>
          <div className="view-sub">即时生效，存储于本机应用数据目录</div>
        </div>
        <div className="spacer" />
        <button
          className="btn small"
          onClick={async () => {
            try {
              await api.openDataDir();
            } catch (e) {
              try {
                const dir = await api.appDataDir();
                toast("info", `请在文件管理器中打开: ${dir}`);
              } catch {
                toast("error", String(e));
              }
            }
          }}
        >
          打开配置目录
        </button>
      </div>

      <div className="view-body">
        <div className="card">
          <h3>外观</h3>
          <div className="row" style={{ marginTop: 10 }}>
            <button
              className={`btn small ${config.settings.theme === "dark" ? "primary" : ""}`}
              onClick={() => update({ theme: "dark" })}
            >
              <Icon name="moon" size={13} /> 深色
            </button>
            <button
              className={`btn small ${config.settings.theme === "light" ? "primary" : ""}`}
              onClick={() => update({ theme: "light" })}
            >
              <Icon name="sun" size={13} /> 浅色
            </button>
          </div>
        </div>

        <div className="card">
          <h3>输入</h3>
          <div className="row" style={{ marginTop: 10 }}>
            <button
              className={`switch ${config.settings.send_on_enter ? "on" : ""}`}
              role="switch"
              aria-checked={config.settings.send_on_enter}
              onClick={() => update({ send_on_enter: !config.settings.send_on_enter })}
            />
            <span style={{ fontSize: 13 }}>Enter 直接发送（关闭后用 Ctrl+Enter 发送）</span>
          </div>
        </div>

        <div className="card">
          <h3>Agent 工具</h3>
          <div className="row" style={{ marginTop: 10, marginBottom: 6 }}>
            <button
              className={`switch ${config.settings.agent_tools ? "on" : ""}`}
              role="switch"
              aria-checked={config.settings.agent_tools}
              onClick={() => update({ agent_tools: !config.settings.agent_tools })}
            />
            <span style={{ fontSize: 13 }}>在已绑定工作区的会话中注入工具定义</span>
          </div>
          <div className="hint" style={{ marginBottom: 6 }}>
            共 16 个工具，路径严格限制在会话绑定的工作区内：
          </div>
          <div className="row" style={{ gap: 6 }}>
            <span className="chip mono">list_dir</span>
            <span className="chip mono">read_file</span>
            <span className="chip mono">glob_files</span>
            <span className="chip mono">grep_files</span>
            <span className="chip mono" title="只读；SSRF 防护拒绝本机/内网地址">
              web_fetch
            </span>
            <span className="chip mono" title="会话任务清单，同步到功能区「任务」面板">
              todo_write
            </span>
            <span className="chip warn mono" title="需要逐次审批">
              <Icon name="edit" size={12} /> write_file
            </span>
            <span className="chip warn mono" title="需要逐次审批">
              <Icon name="edit" size={12} /> edit_file
            </span>
            <span className="chip warn mono" title="需要逐次审批；多处精确替换">
              <Icon name="edit" size={12} /> apply_patch
            </span>
            <span className="chip warn mono" title="需要逐次审批；仅文件，不可恢复">
              <Icon name="trash" size={12} /> delete_file
            </span>
            <span className="chip warn mono" title="需要逐次审批；目标已存在时拒绝">
              <Icon name="branch" size={12} /> move_path
            </span>
            <span className="chip warn mono" title="需要逐次审批；工作区内执行，超时上限 120 秒">
              <Icon name="terminal" size={12} /> run_command
            </span>
          </div>
          <div className="hint" style={{ marginTop: 8 }}>
            只读工具直接执行；<b>写工具每次执行前弹出审批卡</b>（显示变更预览），120 秒未响应自动拒绝——
            拒绝、超时、会话中断一律不落盘。可勾选「本次会话内记住」，授权只记到会话级、重启即失效。
            系统提示随之分层装配：身份 → 全局指令 → AGENTS.md → 运行环境。
          </div>
        </div>

        <div className="card">
          <h3>应用更新</h3>
          <div className="row" style={{ marginTop: 10, marginBottom: 6 }}>
            <button
              className={`switch ${autoUpdate ? "on" : ""}`}
              role="switch"
              aria-checked={autoUpdate}
              onClick={() => {
                const next = !autoUpdate;
                setAutoUpdate(next);
                localStorage.setItem("cc.autoUpdateCheck", next ? "on" : "off");
              }}
            />
            <span style={{ fontSize: 13 }}>
              启动时自动检查更新（每 24 小时最多一次，仅提示、不自动下载）
            </span>
          </div>
          <div className="hint" style={{ marginBottom: 10 }}>
            更新数据来自公开仓库的 GitHub Releases，无需任何配置。发现新版本后会提示前往
            Releases 页面手动下载安装。
          </div>
          <div className="row" style={{ gap: 8 }}>
            <button
              className="btn small"
              onClick={() => void runUpdateCheck(true)}
            >
              立即检查更新
            </button>
            <span style={{ fontSize: 11, color: "var(--text-faint)" }}>
              也可以随时点击侧边栏底部的版本号
            </span>
          </div>
        </div>

        <div className="card">
          <h3>向量长期记忆</h3>
          <div className="row" style={{ marginTop: 10, marginBottom: 6 }}>
            <button
              className={`switch ${config.settings.vector_memory ? "on" : ""}`}
              role="switch"
              aria-checked={config.settings.vector_memory}
              onClick={() => update({ vector_memory: !config.settings.vector_memory })}
            />
            <span style={{ fontSize: 13 }}>
              启用跨会话向量记忆（memory_save / memory_search 工具 + 每轮自动召回注入）
            </span>
          </div>
          <div className="row" style={{ gap: 10, flexWrap: "wrap", marginBottom: 6 }}>
            <label style={{ fontSize: 12, color: "var(--text-dim)", flex: 1, minWidth: 260 }}>
              Embeddings URL（OpenAI 兼容，缺省自动补 /embeddings）
              <input
                className="input mono"
                style={{ display: "block", marginTop: 4, width: "100%" }}
                value={config.settings.embeddings_url ?? ""}
                placeholder="https://api.openai.com/v1/embeddings"
                onChange={(e) => update({ embeddings_url: e.target.value })}
              />
            </label>
            <label style={{ fontSize: 12, color: "var(--text-dim)", minWidth: 200 }}>
              模型
              <input
                className="input mono"
                style={{ display: "block", marginTop: 4, width: "100%" }}
                value={config.settings.embeddings_model ?? ""}
                placeholder="text-embedding-3-small"
                onChange={(e) => update({ embeddings_model: e.target.value })}
              />
            </label>
          </div>
          <label style={{ fontSize: 12, color: "var(--text-dim)", display: "block", marginBottom: 6 }}>
            API Key
            <input
              className="input mono"
              type="password"
              style={{ display: "block", marginTop: 4, width: 320 }}
              value={config.settings.embeddings_key ?? ""}
              placeholder="sk-…（留空则不带 Authorization 头）"
              onChange={(e) => update({ embeddings_key: e.target.value })}
            />
          </label>
          <div className="row" style={{ marginTop: 10, marginBottom: 6 }}>
            <button
              className={`switch ${config.settings.auto_reflect ? "on" : ""}`}
              role="switch"
              aria-checked={config.settings.auto_reflect}
              onClick={() => update({ auto_reflect: !config.settings.auto_reflect })}
            />
            <span style={{ fontSize: 13 }}>回合结束后自动反思（把值得记住的经验写入长期记忆）</span>
          </div>
          <div className="hint" style={{ marginTop: 8 }}>
            记忆按工作区隔离存储（data_dir/memvector.json，每工作区上限 200 条、先进先出）。每次发送消息前会检索最相关的
            3 条记忆附加到请求中；API Key 仅保存在本机配置文件。关闭总开关后工具不再注册、召回注入停止，已存记忆保留。
          </div>
        </div>

        <div className="card">
          <h3>Guardrails 注入防护</h3>
          <div className="row" style={{ marginTop: 10, marginBottom: 6 }}>
            <button
              className={`switch ${config.settings.guardrails ? "on" : ""}`}
              role="switch"
              aria-checked={config.settings.guardrails}
              onClick={() => update({ guardrails: !config.settings.guardrails })}
            />
            <span style={{ fontSize: 13 }}>
              将外部内容（网页抓取、MCP 工具结果）围栏为“数据”，并扫描疑似提示注入用语
            </span>
          </div>
          <label style={{ fontSize: 12, color: "var(--text-dim)", display: "block", marginTop: 8 }}>
            自定义检测模式（每行一个，命中时不区分大小写）
            <textarea
              className="input mono"
              style={{ display: "block", marginTop: 4, width: "100%", minHeight: 56, resize: "vertical" }}
              value={(config.settings.guardrails_extra ?? []).join("\n")}
              placeholder={"每行一条，例如：公司内部代号\n泄露即终止"}
              onChange={(e) =>
                update({
                  guardrails_extra: e.target.value
                    .split("\n")
                    .map((l) => l.trim())
                    .filter((l) => l.length > 0),
                })
              }
            />
          </label>
          <div className="hint" style={{ marginTop: 8 }}>
            开启后，web_fetch 与 MCP 工具的结果会包上「数据围栏」标注来源，即使内容中出现类似指令的文本也只作为资料；
            命中内置中英文注入模式库或自定义模式时追加 GUARDRAIL 警告。围栏会转义内容中伪造的围栏结束标记。
          </div>
        </div>

        <div className="card">
          <h3>目标模式</h3>
          <div className="row" style={{ marginTop: 10, gap: 10, alignItems: "flex-end", flexWrap: "wrap" }}>
            <label style={{ fontSize: 12, color: "var(--text-dim)", minWidth: 240 }}>
              成本软上限（USD / 会话，留空不限）
              <input
                className="input mono"
                type="number"
                min="0"
                step="0.5"
                style={{ display: "block", marginTop: 4, width: "100%" }}
                value={config.settings.goal_budget_usd ?? ""}
                placeholder="例如 5 = 每个目标会话最多 5 美元"
                onChange={(e) => {
                  const v = e.target.value.trim();
                  update({ goal_budget_usd: v === "" ? null : Math.max(0, Number(v) || 0) });
                }}
              />
            </label>
          </div>
          <div className="hint" style={{ marginTop: 8 }}>
            目标模式自动继续推进时，若该会话累计成本达到上限，下一次自动继续会改为发送「收尾」指令——让模型完成手头原子操作、输出最终验收清单并总结进度与剩余工作，然后停止（软停止，不硬中断当前轮次）。
            成本按消息记录中的 cost_usd 累计；Provider 未返回费用数据时该上限不生效。
          </div>
        </div>

        <div className="card">
          <h3>系统通知</h3>
          <div className="row" style={{ gap: 8 }}>
            <button
              className={`switch ${config.settings.notify_done !== false ? "on" : ""}`}
              role="switch"
              aria-checked={config.settings.notify_done !== false}
              onClick={() => update({ notify_done: !(config.settings.notify_done !== false) })}
            />
            <span style={{ fontSize: 13 }}>窗口在后台时发送系统通知</span>
          </div>
          <div className="hint" style={{ marginTop: 8 }}>
            触发时机：长回复（&gt;5 秒）完成、竞技场完成（&gt;8 秒）、工具操作等待审批。窗口在前台时永不打扰。
          </div>
        </div>

        <div className="card">
          <h3>关闭行为</h3>
          <div className="row" style={{ gap: 6 }}>
            {(
              [
                ["ask", "每次询问"],
                ["tray", "最小化到托盘"],
                ["quit", "直接退出"],
              ] as const
            ).map(([value, label]) => (
              <button
                key={value}
                className={`btn small ${config.settings.close_action === value ? "primary" : ""}`}
                onClick={() => update({ close_action: value })}
              >
                {label}
              </button>
            ))}
          </div>
          <div className="hint" style={{ marginTop: 8 }}>
            点击标题栏 ✕ 时的行为。「每次询问」弹出对话框二选一；「最小化到托盘」窗口隐藏、任务继续执行，
            左键托盘图标恢复主界面，右键菜单可显示或真正退出；「直接退出」立即关闭应用。
          </div>
        </div>

        <div className="card">
          <h3>全局 System Prompt</h3>
          <div className="desc">
            作为每个会话冻结区（Zone S）的起点。会话开始后它就进入稳定前缀 —— 中途修改只对新会话生效，这是缓存纪律的刻意取舍。
          </div>
          <textarea
            style={{ width: "100%", minHeight: 110, resize: "vertical" }}
            value={config.settings.system_prompt}
            placeholder="例如：你是一位严谨的编程助手，回答使用中文，代码使用英文……"
            onChange={(e) => update({ system_prompt: e.target.value })}
          />
        </div>

        <div className="card">
          <h3>会话导入</h3>
          <div className="desc">
            把其他智能体 CLI 的本机会话（JSONL 转录）导入为普通 CCHarness 会话。只读取源文件；
            工具调用块会被展开为纯文本，导入后在输入框选择模型即可继续对话。
          </div>
          <div className="row" style={{ gap: 8, marginBottom: 10, flexWrap: "wrap" }}>
            <Dropdown
              compact
              value={impSource}
              options={IMPORT_SOURCES}
              onChange={setImpSource}
              minWidth={150}
            />
            <button className="btn small" disabled={impScanning} onClick={() => void scanImport()}>
              {impScanning ? "扫描中…" : <><Icon name="scan" size={13} /> 扫描</>}
            </button>
          </div>
          {impCands != null && (
            <div style={{ display: "flex", flexDirection: "column", gap: 6, marginBottom: 12 }}>
              {impCands.length === 0 && <div className="hint">未发现 .jsonl 会话文件。</div>}
              {impCands.map((c) => (
                <div key={c.path} className="review-row" style={{ cursor: "default" }}>
                  <span className="path" title={c.path}>{c.title || "(无标题)"}</span>
                  <span className="chip">{c.messages} 条</span>
                  <button
                    className="btn small ghost"
                    disabled={impBusy === c.path}
                    onClick={() => void importOne(c.source, c.path)}
                  >
                    {impBusy === c.path ? "导入中…" : "导入"}
                  </button>
                </div>
              ))}
            </div>
          )}
          <div className="hint" style={{ marginBottom: 6 }}>
            或导入任意 JSONL 转录文件（通用适配器，支持 {'{"message":{role,content}}'} 与 {'{role,content}'} 两种行结构）：
          </div>
          <div className="row" style={{ gap: 8 }}>
            <input
              style={{ flex: 1, minWidth: 260 }}
              className="mono"
              placeholder="例如 C:\Users\me\.claude\projects\xxx\session.jsonl"
              value={impCustom}
              onChange={(e) => setImpCustom(e.target.value)}
            />
            <button
              className="btn small"
              disabled={!impCustom.trim() || impBusy === "custom"}
              onClick={() => void importOne("generic", impCustom.trim())}
            >
              {impBusy === "custom" ? "导入中…" : "解析并导入"}
            </button>
          </div>
        </div>

        <div className="card">
          <div className="row" style={{ justifyContent: "space-between", marginBottom: 4 }}>
            <h3>Skills 技能</h3>
            <button className="btn small ghost" onClick={() => void api.getSkills(null).then(setSkills)}>
              刷新
            </button>
          </div>
          <div className="desc">全局技能目录：<code>~/.ccharness/skills/*.md</code>。项目技能在绑定工作区后出现在输入框的 <code>/</code> 补全面板。</div>
          {skills.length === 0 ? (
            <div className="hint">尚未发现全局技能。创建一个带 frontmatter 的 .md 文件即可：</div>
          ) : (
            <div className="model-chip-row">
              {skills.map((s) => (
                <span key={s.name} className={`chip ${s.auto_inject ? "accent" : ""}`} title={s.description || s.body.slice(0, 160)}>
                  /{s.name} · {s.auto_inject ? "自动注入" : "按需调用"}
                </span>
              ))}
            </div>
          )}
          <pre className="skill-example">{`---\nname: review\ndescription: 代码审查清单\ninject: command\n---\n检查变更的正确性、安全性与回归风险。`}</pre>
        </div>

        <div className="card">
          <h3>关于缓存机制</h3>
          <div className="desc" style={{ marginBottom: 0 }}>
            <p style={{ marginBottom: 6 }}>
              CCHarness 的缓存策略是<b>请求侧前缀对齐</b>：system prompt 冻结、历史 append-only、动态状态只追加在尾部，
              并随请求携带稳定的 <code>prompt_cache_key</code> 做路由亲和——让 provider 的前缀缓存在每个请求上命中
              （缓存 token 约为输入价 1/10）。
            </p>
            <p style={{ marginBottom: 6 }}>
              <b>验证手段</b>：缓存遥测页的请求账本带「链」列——✓ 表示本地字节链连续（append-only 完好）；
              ✓ 而命中仍低时，问题在上游：OpenAI 系缓存有 ≥1024 token 门槛；部分中转网关只缓存提示词头部
              512–1023 token、或不按 key 路由（多后端轮询）。命中量恒等于小常数（如 512/1023）且输入增长而命中不增，
              即网关侧特征。
            </p>
            <p>
              <b>金标准对照</b>：DeepSeek 官方端点（无门槛、单后端、如实上报），同一会话第二条请求起命中应 ≥90%。
              这里没有响应缓存、语义缓存、也没有命中率调参旋钮——机制永远在线，关掉等于主动多花钱。
            </p>
          </div>
        </div>

        <div className="card">
          <h3>功能速览</h3>
          <div className="desc" style={{ marginBottom: 0 }}>
            <p style={{ marginBottom: 6 }}>
              · <b>会话</b>：流式输出、思考过程折叠、置信度解析、工具调用卡片、自动跟随滚动、Markdown 导出（<code>Ctrl+K</code> 命令面板直达一切）。
            </p>
            <p style={{ marginBottom: 6 }}>
              · <b>竞技场</b>：一条提示词同时发给多个模型并行对比，各泳道独立上下文与缓存纪元。
            </p>
            <p style={{ marginBottom: 6 }}>
              · <b>项目接入</b>：会话绑定工作区后自动加载 AGENTS.md 为项目说明，工具全部以它为根目录。
            </p>
            <p>
              · <b>遥测</b>：命中率曲线（含纪元标记）、逐请求账本（链连续性 / token / 成本）、全局汇总；会话可导出 Markdown。
            </p>
          </div>
        </div>
      </div>
    </>
  );
}
