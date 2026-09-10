// SkillHub marketplace + installed-skills manager. Market tab browses
// skillhub.cn; Installed tab lists every loaded skill (global + project)
// with mode/source/body preview and delete (global only).
import { useEffect, useState } from "react";
import { useApp } from "../store";
import { Dropdown } from "../components/Dropdown";
import { ConfirmDialog } from "../components/Dialog";
import { fmtTokens } from "../lib/format";
import { Icon } from "../lib/icons";
import * as api from "../lib/api";
import type { MarketPlugin, MarketSkill, SkillInfo } from "../types";

const SORTS = [
  { value: "score", label: "综合评分" },
  { value: "downloads", label: "下载数" },
  { value: "installs", label: "安装数" },
  { value: "newest", label: "最新" },
];
const PAGE_SIZE = 12;

// plugin categories from the SkillHub plugins registry
const PLUGIN_CATEGORIES = [
  { value: "", label: "全部" },
  { value: "client", label: "客户端" },
  { value: "model-inference", label: "模型推理" },
  { value: "workflow", label: "工作流" },
  { value: "memory", label: "记忆" },
  { value: "security", label: "安全" },
];

function fmtCount(n: number | null): string {
  if (n == null) return "—";
  if (n >= 10000) return `${(n / 10000).toFixed(1)}w`;
  if (n >= 1000) return `${(n / 1000).toFixed(1)}k`;
  return String(n);
}

export function MarketView() {
  const toast = useApp((s) => s.toast);
  const sessions = useApp((s) => s.sessions);
  const activeSessionId = useApp((s) => s.activeSessionId);
  const [tab, setTab] = useState<"market" | "installed" | "plugins">("market");
  const [skills, setSkills] = useState<MarketSkill[]>([]);
  const [total, setTotal] = useState(0);
  const [page, setPage] = useState(1);
  const [sortBy, setSortBy] = useState("score");
  const [kw, setKw] = useState("");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [installing, setInstalling] = useState<string | null>(null);
  const [installed, setInstalled] = useState<Set<string>>(new Set());
  // plugins-tab state
  const [plugins, setPlugins] = useState<MarketPlugin[]>([]);
  const [pTotal, setPTotal] = useState(0);
  const [pPage, setPPage] = useState(1);
  const [category, setCategory] = useState("");
  const [pLoading, setPLoading] = useState(false);
  const [pError, setPError] = useState<string | null>(null);
  const [pInstalling, setPInstalling] = useState<string | null>(null);
  // installed-tab state
  const [allSkills, setAllSkills] = useState<SkillInfo[]>([]);
  const [confirmDel, setConfirmDel] = useState<string | null>(null);
  const [expanded, setExpanded] = useState<string | null>(null);

  // workspace of the active chat session → project skills visibility
  const workspace =
    sessions.find((s) => s.id === activeSessionId && s.kind === "chat")?.workspace ?? null;

  const load = async (p = page, sort = sortBy, keyword = kw) => {
    setLoading(true);
    setError(null);
    try {
      const r = await api.skillhubList(p, PAGE_SIZE, sort, keyword);
      setSkills(r.skills);
      setTotal(r.total);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    void load(1, sortBy, kw);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const loadPlugins = async (p = 1, cat = category) => {
    setPLoading(true);
    setPError(null);
    try {
      const r = await api.skillhubPlugins(p, PAGE_SIZE, cat);
      setPlugins(r.plugins);
      setPTotal(r.total);
      setPPage(p);
    } catch (e) {
      setPError(String(e));
    } finally {
      setPLoading(false);
    }
  };

  useEffect(() => {
    if (tab === "plugins") void loadPlugins(1, category);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tab]);

  const installPlugin = async (p: MarketPlugin) => {
    setPInstalling(p.full_name);
    try {
      const msg = await api.skillhubPluginInstall(
        p.owner,
        p.name,
        p.default_branch || "main",
        p.description || ""
      );
      toast("success", msg);
    } catch (e) {
      toast("error", String(e));
    } finally {
      setPInstalling(null);
    }
  };

  const refreshInstalled = async () => {
    try {
      const list = await api.getSkills(workspace);
      setAllSkills(list);
      setInstalled(new Set(list.map((s) => s.name)));
    } catch {
      /* ignore */
    }
  };

  useEffect(() => {
    void refreshInstalled();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [workspace]);

  useEffect(() => {
    if (tab === "installed") void refreshInstalled();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tab]);

  const install = async (s: MarketSkill) => {
    setInstalling(s.slug);
    try {
      const msg = await api.skillhubInstall(
        s.slug,
        s.namespace?.handle ?? "",
        s.version,
        s.description_zh || s.description || ""
      );
      toast("success", msg);
      setInstalled((cur) => new Set(cur).add(s.slug));
      void refreshInstalled();
    } catch (e) {
      toast("error", String(e));
    } finally {
      setInstalling(null);
    }
  };

  const pages = Math.max(1, Math.ceil(total / PAGE_SIZE));
  const globalCount = allSkills.filter((s) => s.source === "global").length;
  const projectCount = allSkills.length - globalCount;

  return (
    <>
      <div className="view-header">
        <div>
          <div className="view-title">{tab === "plugins" ? "插件" : "技能"}</div>
          <div className="view-sub">
            {tab === "market"
              ? "SkillHub 社区技能 · 安装后以 /slug 斜杠命令使用（仅提示层，不执行包内脚本）"
              : tab === "plugins"
                ? "SkillHub 社区插件 · 安装 = 提取包内 SKILL.md 为全局技能，不执行任何脚本；客户端 UI 类插件无可安装内容"
                : `共 ${allSkills.length} 个技能（全局 ${globalCount} · 项目 ${projectCount}${workspace ? "" : "，绑定工作区后可见项目技能"}）`}
          </div>
        </div>
        <div className="spacer" />
        <div className="seg">
          <button className={`seg-btn ${tab === "market" ? "on" : ""}`} onClick={() => setTab("market")}>
            <Icon name="bag" size={13} /> 市场
          </button>
          <button className={`seg-btn ${tab === "plugins" ? "on" : ""}`} onClick={() => setTab("plugins")}>
            <Icon name="plug" size={13} /> 插件
          </button>
          <button className={`seg-btn ${tab === "installed" ? "on" : ""}`} onClick={() => setTab("installed")}>
            <Icon name="archive" size={13} /> 已安装 {allSkills.length > 0 && <span className="seg-count">{allSkills.length}</span>}
          </button>
        </div>
        {tab === "market" ? (
          <>
            <input
              style={{ width: 220 }}
              placeholder="搜索技能…"
              value={kw}
              onChange={(e) => setKw(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") {
                  setPage(1);
                  void load(1, sortBy, kw);
                }
              }}
            />
            <Dropdown
              value={sortBy}
              options={SORTS}
              minWidth={130}
              onChange={(v) => {
                setSortBy(v);
                setPage(1);
                void load(1, v, kw);
              }}
            />
            <button className="btn small" onClick={() => void load(page, sortBy, kw)} disabled={loading}>
              搜索
            </button>
          </>
        ) : tab === "plugins" ? (
          <>
            <Dropdown
              value={category}
              options={PLUGIN_CATEGORIES}
              minWidth={130}
              onChange={(v) => {
                setCategory(v);
                void loadPlugins(1, v);
              }}
            />
            <button className="btn small" onClick={() => void loadPlugins(pPage, category)} disabled={pLoading}>
              刷新
            </button>
          </>
        ) : (
          <button className="btn small" onClick={() => void refreshInstalled()}>
            刷新
          </button>
        )}
      </div>

      <div className="view-body">
        {tab === "installed" ? (
          <>            {allSkills.length === 0 && (
              <div className="empty-state">
                <div className="big"><Icon name="archive" size={40} /></div>
                <h3>还没有已安装的技能</h3>
                <p>
                  到「市场」标签安装 SkillHub 技能，或在
                  <code> ~/.ccharness/skills/ </code>放置 .md 文件（全局），绑定工作区后还可使用项目技能。
                </p>
              </div>
            )}
            <div className="installed-list">
              {allSkills.map((s) => (
                <div className="installed-row" key={`${s.source}-${s.name}`}>
                  <div className="ir-main">
                    <span className="mono ir-name">/{s.name}</span>
                    <span className={`chip ${s.auto_inject ? "accent" : ""}`}>
                      {s.auto_inject ? "自动注入" : "按需调用"}
                    </span>
                    <span className={`chip ${s.source === "project" ? "good" : ""}`}>
                      {s.source === "project" ? "项目" : "全局"}
                    </span>
                    <span className="ir-desc">{s.description || "（无描述）"}</span>
                  </div>
                  <div className="ir-actions">
                    <button
                      className="btn small ghost"
                      onClick={() => setExpanded(expanded === `${s.source}-${s.name}` ? null : `${s.source}-${s.name}`)}
                    >
                      {expanded === `${s.source}-${s.name}` ? "收起" : "预览"}
                    </button>
                    {s.source === "global" && (
                      confirmDel === s.name ? (
                        <button
                          className="btn small danger"
                          onClick={async () => {
                            try {
                              await api.deleteSkill(s.name);
                              toast("success", `已删除 /${s.name}`);
                            } catch (e) {
                              toast("error", String(e));
                            }
                            setConfirmDel(null);
                            void refreshInstalled();
                          }}
                        >
                          确认删除
                        </button>
                      ) : (
                        <button className="btn small danger" onClick={() => { setConfirmDel(s.name); setTimeout(() => setConfirmDel((c) => (c === s.name ? null : c)), 3000); }}>
                          删除
                        </button>
                      )
                    )}
                  </div>
                  {expanded === `${s.source}-${s.name}` && (
                    <pre className="skill-example">{s.body}</pre>
                  )}
                </div>
              ))}
            </div>
          </>
        ) : tab === "plugins" ? (
          <>
            {pError && <div className="card" style={{ borderColor: "var(--bad)" }}>加载失败: {pError}</div>}
            {pLoading && <div className="empty-state"><div className="big"><Icon name="download" size={36} /></div><h3>加载中…</h3></div>}
            {!pLoading && !pError && plugins.length === 0 && (
              <div className="empty-state"><div className="big"><Icon name="plug" size={40} /></div><h3>没有匹配的插件</h3><p>换个分类试试</p></div>
            )}
            <div className="market-grid">
              {plugins.map((p) => (
                <div className="market-card" key={p.full_name}>
                  <div className="mk-head">
                    {p.avatar_url ? (
                      <img className="mk-icon" src={p.avatar_url} alt="" loading="lazy" />
                    ) : (
                      <span className="mk-icon mk-fallback">P</span>
                    )}
                    <div style={{ minWidth: 0, flex: 1 }}>
                      <div className="mk-name" title={p.full_name}>{p.name}</div>
                      <div className="mk-meta">
                        <span title="星标"><Icon name="star" size={12} /> {fmtCount(p.stars)}</span>
                        <span title="复刻"><Icon name="branch" size={12} /> {fmtCount(p.forks)}</span>
                        {p.license && <span className="chip mono">{p.license}</span>}
                        {p.installability === "verified" && <span className="chip good">已认证</span>}
                      </div>
                    </div>
                  </div>
                  <div className="mk-desc" title={p.description}>
                    {p.description || "（无描述）"}
                  </div>
                  <div className="mk-foot">
                    <span className="chip mono" title={p.repository_url}>{p.owner}</span>
                    <button
                      className="btn small primary"
                      disabled={pInstalling === p.full_name}
                      onClick={() => void installPlugin(p)}
                    >
                      {pInstalling === p.full_name ? "安装中…" : "安装技能"}
                    </button>
                  </div>
                </div>
              ))}
            </div>
            {pTotal > PAGE_SIZE && (
              <div className="market-pager">
                <button className="btn small" disabled={pPage <= 1 || pLoading} onClick={() => void loadPlugins(pPage - 1, category)}>
                  ← 上一页
                </button>
                <span className="mono">
                  第 {pPage} / {Math.max(1, Math.ceil(pTotal / PAGE_SIZE))} 页 · 共 {fmtTokens(pTotal)} 个插件
                </span>
                <button className="btn small" disabled={pPage >= Math.max(1, Math.ceil(pTotal / PAGE_SIZE)) || pLoading} onClick={() => void loadPlugins(pPage + 1, category)}>
                  下一页 →
                </button>
              </div>
            )}
          </>
        ) : (
          <>
            {error && <div className="card" style={{ borderColor: "var(--bad)" }}>加载失败: {error}</div>}
            {loading && <div className="empty-state"><div className="big"><Icon name="download" size={36} /></div><h3>加载中…</h3></div>}
            {!loading && !error && skills.length === 0 && (
              <div className="empty-state"><div className="big"><Icon name="bag" size={40} /></div><h3>没有匹配的技能</h3><p>换个关键词试试</p></div>
            )}
            <div className="market-grid">
          {skills.map((s) => {
            const isInstalled = installed.has(s.slug);
            return (
              <div className="market-card" key={`${s.slug}-${s.namespace?.handle ?? ""}`}>
                <div className="mk-head">
                  {s.icon_url ? (
                    <img className="mk-icon" src={s.icon_url} alt="" loading="lazy" />
                  ) : (
                    <span className="mk-icon mk-fallback">S</span>
                  )}
                  <div style={{ minWidth: 0, flex: 1 }}>
                    <div className="mk-name" title={s.name}>{s.name || s.slug}</div>
                    <div className="mk-meta">
                      <span title="下载数"><Icon name="download" size={12} /> {fmtCount(s.downloads)}</span>
                      <span title="安装数"><Icon name="inbox" size={12} /> {fmtCount(s.installs)}</span>
                      <span title="星标"><Icon name="star" size={12} /> {fmtCount(s.stars)}</span>
                      {s.verified && <span className="chip good">已认证</span>}
                    </div>
                  </div>
                </div>
                <div className="mk-desc" title={s.description_zh || s.description}>
                  {s.description_zh || s.description || "（无描述）"}
                </div>
                <div className="mk-foot">
                  <span className="chip mono">{s.slug}</span>
                  {isInstalled ? (
                    <span className="chip good"><Icon name="check" size={12} /> 已安装</span>
                  ) : (
                    <button
                      className="btn small primary"
                      disabled={installing === s.slug}
                      onClick={() => void install(s)}
                    >
                      {installing === s.slug ? "安装中…" : "安装"}
                    </button>
                  )}
                </div>
              </div>
            );
          })}
        </div>
        {total > PAGE_SIZE && (
          <div className="market-pager">
            <button className="btn small" disabled={page <= 1 || loading} onClick={() => { const p = page - 1; setPage(p); void load(p); }}>
              ← 上一页
            </button>
            <span className="mono">
              第 {page} / {pages} 页 · 共 {fmtTokens(total)} 个技能
            </span>
            <button className="btn small" disabled={page >= pages || loading} onClick={() => { const p = page + 1; setPage(p); void load(p); }}>
              下一页 →
            </button>
          </div>
        )}
          </>
        )}
      </div>
    </>
  );
}
