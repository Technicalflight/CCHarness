// Prefix-cache telemetry dashboard: hit-rate curve with epoch markers,
// per-request ledger, and cross-session totals.
import { useEffect, useState } from "react";
import { useApp } from "../store";
import { HitRateChart } from "../components/Chart";
import { Dropdown } from "../components/Dropdown";
import { fmtBytes, fmtHit, fmtTime, fmtTokens, fmtUsd } from "../lib/format";
import { Icon } from "../lib/icons";
import * as api from "../lib/api";
import type { AuxOrigin, AuxStats, Divergence, GlobalStats, SessionTelemetry } from "../types";

const AUX_KIND_LABEL: Record<string, string> = {
  title: "标题生成",
  enhance: "提示词增强",
};

const DIVERGENCE_META: Record<Divergence["kind"], { label: string; color: string }> = {
  chain_broken: { label: "链断裂", color: "var(--bad)" },
  prefix_shrunk: { label: "前缀回退", color: "var(--bad)" },
  tail_dominant: { label: "尾区过大", color: "var(--warn)" },
  upstream_loss: { label: "上游丢失", color: "var(--warn)" },
  partial_drop: { label: "部分骤降", color: "var(--warn)" },
};

const AUX_ORIGIN_META: Record<AuxOrigin, { label: string; color: string }> = {
  l1: { label: "L1 内存", color: "var(--good)" },
  l2: { label: "L2 磁盘", color: "var(--good)" },
  miss: { label: "未命中", color: "var(--bad)" },
};

export function TelemetryView() {
  // per-field selectors: a bare useApp() re-renders the whole view on every
  // stream delta (sessions/toast are the only fields this view touches)
  const sessions = useApp((s) => s.sessions);
  const toast = useApp((s) => s.toast);
  const [sel, setSel] = useState<string>("");
  const [tel, setTel] = useState<SessionTelemetry | null>(null);
  const [global, setGlobal] = useState<GlobalStats | null>(null);
  const [aux, setAux] = useState<AuxStats | null>(null);

  useEffect(() => {
    if (!sel && sessions.length > 0) setSel(sessions[0].id);
  }, [sessions, sel]);

  useEffect(() => {
    void (async () => {
      try {
        setGlobal(await api.getGlobalStats());
        setAux(await api.getAuxStats());
      } catch {
        /* ignore */
      }
    })();
  }, []);

  useEffect(() => {
    if (!sel) return;
    // alive guard: a slow response for the previous selection must not
    // land after the user switched sessions
    let alive = true;
    void (async () => {
      try {
        const t = await api.getTelemetry(sel);
        if (alive) setTel(t);
      } catch (e) {
        if (alive) toast("error", `遥测加载失败: ${String(e)}`);
      }
    })();
    return () => {
      alive = false;
    };
  }, [sel, toast]);

  const summary = tel?.summary;
  const reqs = tel?.requests ?? [];
  const chartPoints =
    reqs
      .filter((r) => r.cached_tokens != null && r.input_tokens)
      .map((r, i, arr) => ({
        x: arr.length === 1 ? 0 : i / (arr.length - 1),
        y: Math.min(100, (r.cached_tokens! / r.input_tokens!) * 100),
      })) ?? [];

  const epochXs =
    tel?.epochs.map((ts) => {
      const idx = reqs.findIndex((r) => r.ts >= ts);
      if (idx < 0) return 1;
      return reqs.length <= 1 ? 0 : idx / (reqs.length - 1);
    }) ?? [];

  return (
    <>
      <div className="view-header">
        <div>
          <div className="view-title">缓存遥测</div>
          <div className="view-sub">前缀命中率 · 纪元事件 · 请求账本 —— 长会话稳态目标 ≥ 95%</div>
        </div>
        <div className="spacer" />
        <Dropdown
          value={sel}
          minWidth={240}
          placeholder={sessions.length === 0 ? "暂无会话" : "选择会话…"}
          options={sessions
            .filter((s) => s.kind !== "sub")
            .map((s) => ({
              value: s.id,
              label: `${s.title || "未命名"} · ${s.kind === "arena" ? "竞技场" : "会话"}`,
            }))}
          onChange={setSel}
        />
      </div>

      <div className="view-body">
        {/* global cards */}
        {global && (
          <div className="stat-cards">
            <div className="stat-card">
              <div className="label">全部会话请求</div>
              <div className="value">{global.requests}</div>
              <div className="sub">{global.sessions} 个会话</div>
            </div>
            <div className="stat-card">
              <div className="label">累计缓存命中 token</div>
              <div className="value">{fmtTokens(global.total_cached)}</div>
              <div className="sub">输入共 {fmtTokens(global.total_input)}</div>
            </div>
            <div className="stat-card">
              <div className="label">累计输出</div>
              <div className="value">{fmtTokens(global.total_output)}</div>
            </div>
            <div className="stat-card">
              <div className="label">估算总成本</div>
              <div className="value">{fmtUsd(global.total_cost)}</div>
              <div className="sub">按已配置定价计算</div>
            </div>
          </div>
        )}

        {summary && (
          <div className="stat-cards">
            <div className="stat-card">
              <div className="label">本会话请求</div>
              <div className="value">{summary.requests}</div>
            </div>
            <div className="stat-card">
              <div className="label">稳态命中率</div>
              <div className="value" style={{ color: (summary.steady_hit_rate ?? 0) >= 95 ? "var(--good)" : "var(--warn)" }}>
                {summary.steady_hit_rate != null ? `${summary.steady_hit_rate.toFixed(1)}%` : "—"}
              </div>
              <div className="sub">均值 {summary.avg_hit_rate != null ? `${summary.avg_hit_rate.toFixed(1)}%` : "—"}</div>
            </div>
            <div className="stat-card">
              <div className="label">当前纪元</div>
              <div className="value">#{summary.current_epoch + 1}</div>
              <div className="sub">{tel?.epochs.length ?? 0} 次重建</div>
            </div>
            <div className="stat-card">
              <div className="label">稳定前缀长度</div>
              <div className="value" style={{ fontSize: 16 }}>{fmtBytes(summary.prefix_bytes)}</div>
              <div className="sub">append-only 字节</div>
            </div>
            <div className="stat-card">
              <div className="label">miss 重计费</div>
              <div
                className="value"
                style={{ fontSize: 16, color: (summary.significant_misses ?? 0) > 0 ? "var(--warn)" : "var(--good)" }}
              >
                {(summary.rebilled_cost ?? 0) > 0 ? fmtUsd(summary.rebilled_cost!) : summary.rebilled_cost === null && (summary.significant_misses ?? 0) > 0 ? `${fmtTokens(summary.rebilled_tokens ?? 0)} tok` : "—"}
              </div>
              <div className="sub">
                {(summary.significant_misses ?? 0) > 0
                  ? `${summary.significant_misses} 次显著 miss 的冤枉钱`
                  : "无显著 miss（预期重建不计入）"}
              </div>
            </div>
          </div>
        )}

        <div className="card">
          <h3>前缀命中率曲线</h3>
          <div className="desc">虚线竖标 = 纪元边界（模型切换 / 新前缀重建），重建后的第一个请求命中率回落属预期行为。</div>
          <HitRateChart points={chartPoints} epochs={epochXs} />
        </div>

        <div className="card">
          <h3>miss 分歧定位</h3>
          <div className="desc">
            当 provider 报 0 命中 / 部分骤降，而本地指纹链显示稳定区完整（chain ✓ 且非纪元首轮）时，按请求形态自动归因：
            链断裂（客户端改写，缺陷）→ 前缀回退（同纪元字节变短）→ 尾区过大（本轮新增超过稳定前缀）→ 上游丢失（路由切换 / TTL 逐出 / 网关不路由缓存）。
          </div>
          {(() => {
            const divs = tel?.divergences ?? [];
            if (!tel || divs.length === 0) {
              return (
                <div className="desc" style={{ color: "var(--good)", marginBottom: 0 }}>
                  ✓ 未检测到分歧 —— 本地链与上游命中行为一致（纪元首轮与未上报缓存字段的请求不计入）。
                </div>
              );
            }
            return (
              <table className="data wrap">
                <thead>
                  <tr>
                    <th>#</th>
                    <th>时间</th>
                    <th>归因</th>
                    <th>说明</th>
                  </tr>
                </thead>
                <tbody>
                  {divs
                    .slice()
                    .reverse()
                    .map((d) => {
                      const meta = DIVERGENCE_META[d.kind] ?? { label: d.kind, color: undefined };
                      return (
                        <tr key={d.seq}>
                          <td>{d.seq}</td>
                          <td className="plain">{fmtTime(d.ts)}</td>
                          <td style={{ color: meta.color, fontWeight: 600 }}>{meta.label}</td>
                          <td className="plain">{d.detail}</td>
                        </tr>
                      );
                    })}
                </tbody>
              </table>
            );
          })()}
        </div>

        <div className="card">
          <h3>请求账本</h3>
          <div className="desc">
            每行 = 一次 API 请求的单独口径。会话/轮次上的命中率是按输入 token
            加权的聚合值（Σ缓存命中 ÷ Σ输入），不等于各行命中率的简单平均。
          </div>
          {reqs.length === 0 ? (
            <div className="desc">暂无数据。</div>
          ) : (
            <div className="tbl-scroll">
              <table className="data">
                <thead>
                  <tr>
                    <th>#</th>
                    <th>时间</th>
                    <th>模型</th>
                    <th>泳道</th>
                    <th>纪元</th>
                    <th title="本地字节链连续性：✓ 表示本请求稳定区完整覆盖上一次请求（append-only），客户端无改写">链</th>
                    <th>前缀字节</th>
                    <th>新增字节</th>
                    <th>输入</th>
                    <th>缓存命中</th>
                    <th>命中率</th>
                    <th>输出</th>
                    <th>成本</th>
                    <th>重计费</th>
                  </tr>
                </thead>
                <tbody>
                  {reqs
                    .slice()
                    .reverse()
                    .map((r) => (
                      <tr key={r.seq}>
                        <td>{r.seq}</td>
                        <td className="plain">{fmtTime(r.ts)}</td>
                        <td>{r.model}</td>
                        <td>#{r.lane + 1}</td>
                        <td>{r.epoch + 1}</td>
                        <td
                          title={r.chain_ok ? "本地字节链连续" : "本地链断裂——出现客户端改写（应视为缺陷上报）"}
                          style={{ color: r.chain_ok ? "var(--good)" : "var(--bad)" }}
                        >
                          {r.chain_ok ? "✓" : "✗"}
                        </td>
                        <td>{fmtBytes(r.prefix_bytes)}</td>
                        <td>{fmtBytes(r.added_bytes)}</td>
                        <td>{fmtTokens(r.input_tokens)}</td>
                        <td>{fmtTokens(r.cached_tokens)}</td>
                        <td
                          style={{
                            color:
                              r.cached_tokens != null && r.input_tokens
                                ? r.cached_tokens / r.input_tokens >= 0.9
                                  ? "var(--good)"
                                  : "var(--warn)"
                                : undefined,
                          }}
                        >
                          {fmtHit(r.cached_tokens, r.input_tokens)}
                        </td>
                        <td>{fmtTokens(r.output_tokens)}</td>
                        <td>{fmtUsd(r.cost_usd)}</td>
                        <td
                          title={
                            r.significant_miss
                              ? `显著 miss（${r.miss_cause === "client" ? "客户端改写" : "上游"}）：重计费 ≈ ${fmtTokens(r.rebilled_tokens ?? 0)} tokens`
                              : "无显著 miss——纪元首轮（预期重建）与未上报缓存字段的请求不计入"
                          }
                          style={{ color: r.significant_miss ? "var(--warn)" : undefined }}
                        >
                          {r.significant_miss
                            ? r.rebilled_cost != null
                              ? fmtUsd(r.rebilled_cost)
                              : `${fmtTokens(r.rebilled_tokens ?? 0)} tok`
                            : "—"}
                        </td>
                      </tr>
                    ))}
                </tbody>
              </table>
            </div>
          )}
        </div>

        {/* Compaction ledger — the third book (L6 §6), disjoint from the
            request ledger above and the AuxMemo book below */}
        <div className="card">
          <h3>压缩账本</h3>
          <div className="desc">
            每行 = 一次边界压缩的单独口径。三桶对账：KEEP 最近轮次逐字节保留，FOLD 中段历史折叠为结构化摘要，DROP
            过时大块工具输出降级为存根。回本 = 摘要重算成本 ÷ 每轮缓存节省，只有预期剩余轮数足够摊薄时自动压缩才会执行。10
            轮内出现第 2 次自动压缩会将会话触发线临时上调至 80%（30 轮后回落）并在会话中插入提示。
          </div>
          {(() => {
            const comps = tel?.compactions ?? [];
            const boosting =
              tel != null && tel.boost_until_turn > 0 && tel.boost_until_turn > tel.completed_turns;
            const tot = comps.reduce(
              (a, c) => ({
                folded: a.folded + c.folded_tokens,
                dropped: a.dropped + c.dropped_tokens,
                stubs: a.stubs + c.stubs,
              }),
              { folded: 0, dropped: 0, stubs: 0 }
            );
            return (
              <>
                {boosting && (
                  <div className="desc" style={{ color: "var(--warn)", marginBottom: 10 }}>
                    ⚠ 本会话处于压缩抖动保护中：触发线临时上调至 80%（第 {tel!.boost_until_turn} 轮回落，当前第{" "}
                    {tel!.completed_turns} 轮）。
                  </div>
                )}
                {comps.length > 0 && (
                  <div className="stat-cards" style={{ marginBottom: 14 }}>
                    <div className="stat-card">
                      <div className="label">压缩次数</div>
                      <div className="value">{comps.length}</div>
                      <div className="sub">
                        自动 {comps.filter((c) => c.trigger === "auto").length} · 手动{" "}
                        {comps.filter((c) => c.trigger !== "auto").length}
                      </div>
                    </div>
                    <div className="stat-card">
                      <div className="label">累计折叠</div>
                      <div className="value">{fmtTokens(tot.folded)}</div>
                      <div className="sub">FOLD 桶进入摘要</div>
                    </div>
                    <div className="stat-card">
                      <div className="label">累计降级</div>
                      <div className="value">{fmtTokens(tot.dropped)}</div>
                      <div className="sub">{tot.stubs} 个存根（免费档）</div>
                    </div>
                    <div className="stat-card">
                      <div className="label">最近回本</div>
                      <div className="value">
                        {comps[comps.length - 1].payback_turns != null
                          ? `${comps[comps.length - 1].payback_turns} 轮`
                          : "—"}
                      </div>
                      <div className="sub">无定价数据时显示 —</div>
                    </div>
                  </div>
                )}
                {comps.length === 0 ? (
                  <div className="desc" style={{ marginBottom: 0 }}>
                    暂无压缩记录 —— 会话输入达到上下文窗口 70% 时在轮边界自动触发，也可手动压缩。压缩前会先跑两档免费清理（回溯剪枝 + 过时输出降级），能救回就不调摘要。
                  </div>
                ) : (
                  <div className="tbl-scroll">
                    <table className="data">
                      <thead>
                        <tr>
                          <th>时间</th>
                          <th>触发</th>
                          <th title="FOLD 桶：中段历史折叠为结构化摘要的 token 数">折叠</th>
                          <th title="DROP 桶：过时大块工具输出降级为存根省下的 token 数（含存根数）">降级</th>
                          <th>摘要</th>
                          <th title="压缩时 RollingMemo 渲染字符数">备忘</th>
                          <th title="回本估算：摘要重算成本 ÷ 每轮缓存节省">回本</th>
                          <th title="触发时的已完成用户轮数">轮次</th>
                        </tr>
                      </thead>
                      <tbody>
                        {comps
                          .slice()
                          .reverse()
                          .map((c, i) => (
                            <tr key={`${c.ts}-${i}`}>
                              <td className="plain">{fmtTime(c.ts)}</td>
                              <td style={{ color: c.trigger === "auto" ? undefined : "var(--info)" }}>
                                {c.trigger === "auto" ? "自动" : "手动"}
                              </td>
                              <td>{fmtTokens(c.folded_tokens)}</td>
                              <td>
                                {c.dropped_tokens > 0
                                  ? `${fmtTokens(c.dropped_tokens)}（${c.stubs} 存根）`
                                  : "—"}
                              </td>
                              <td>{fmtTokens(c.summary_tokens)}</td>
                              <td>{c.memo_chars > 0 ? `${c.memo_chars} 字` : "—"}</td>
                              <td>{c.payback_turns != null ? `${c.payback_turns} 轮` : "—"}</td>
                              <td>{c.completed_turns}</td>
                            </tr>
                          ))}
                      </tbody>
                    </table>
                  </div>
                )}
              </>
            );
          })()}
        </div>

        {/* AuxMemo — a separate book from the main-loop ledger above */}
        <div className="card">
          <h3>辅助调用精确缓存（AuxMemo）</h3>
          <div className="desc">
            只统计白名单幂等调用（标题生成、提示词增强）。agent 主循环的每轮请求记录在上面的请求账本中，两类账本永不分摊——命中行 billed=0 是精确缓存的直接证据。
          </div>
          {aux && aux.kinds.length > 0 ? (
            <>
              <div className="stat-cards" style={{ marginBottom: 14 }}>
                {(() => {
                  const t = aux.kinds.reduce(
                    (acc, k) => ({
                      calls: acc.calls + k.calls,
                      hits: acc.hits + k.l1_hits + k.l2_hits,
                      saved: acc.saved + k.saved_usd,
                      billed: acc.billed + k.billed_usd,
                    }),
                    { calls: 0, hits: 0, saved: 0, billed: 0 }
                  );
                  const rate = t.calls > 0 ? (t.hits / t.calls) * 100 : null;
                  return (
                    <>
                      <div className="stat-card">
                        <div className="label">辅助调用次数</div>
                        <div className="value">{t.calls}</div>
                        <div className="sub">
                          {aux.kinds.map((k) => `${AUX_KIND_LABEL[k.kind] ?? k.kind} ${k.calls}`).join(" · ")}
                        </div>
                      </div>
                      <div className="stat-card">
                        <div className="label">精确命中率</div>
                        <div className="value" style={{ color: (rate ?? 0) >= 50 ? "var(--good)" : undefined }}>
                          {rate != null ? `${rate.toFixed(1)}%` : "—"}
                        </div>
                        <div className="sub">L1 内存 + L2 磁盘</div>
                      </div>
                      <div className="stat-card">
                        <div className="label">节省成本（估算）</div>
                        <div className="value">{fmtUsd(t.saved)}</div>
                        <div className="sub">命中时按入库成本计</div>
                      </div>
                      <div className="stat-card">
                        <div className="label">实际计费</div>
                        <div className="value">{fmtUsd(t.billed)}</div>
                        <div className="sub">仅未命中轮产生</div>
                      </div>
                    </>
                  );
                })()}
              </div>
              <div className="tbl-scroll">
                <table className="data">
                  <thead>
                    <tr>
                      <th>时间</th>
                      <th>类型</th>
                      <th>模型</th>
                      <th>来源</th>
                      <th>输入</th>
                      <th>输出</th>
                      <th>计费</th>
                      <th>节省</th>
                    </tr>
                  </thead>
                  <tbody>
                    {aux.recent.map((r, i) => {
                      const om = AUX_ORIGIN_META[r.origin] ?? { label: r.origin, color: undefined };
                      return (
                        <tr key={`${r.ts}-${i}`}>
                          <td className="plain">{fmtTime(r.ts)}</td>
                          <td>{AUX_KIND_LABEL[r.kind] ?? r.kind}</td>
                          <td>{r.model}</td>
                          <td style={{ color: om.color }}>{om.label}</td>
                          <td>{fmtTokens(r.input_tokens)}</td>
                          <td>{fmtTokens(r.output_tokens)}</td>
                          <td style={{ color: r.origin === "miss" ? undefined : "var(--good)" }}>
                            {fmtUsd(r.origin === "miss" ? r.billed_usd : 0)}
                          </td>
                          <td style={{ color: r.origin !== "miss" ? "var(--good)" : undefined }}>
                            {r.origin !== "miss" ? fmtUsd(r.saved_usd) : "—"}
                          </td>
                        </tr>
                      );
                    })}
                  </tbody>
                </table>
              </div>
            </>
          ) : (
            <div className="desc" style={{ marginBottom: 0 }}>
              暂无辅助调用记录 —— 在输入框点「<Icon name="spark" size={12} /> 优化提示词」即可产生第一条。
            </div>
          )}
        </div>

        <div className="card">
          <h3>读数说明</h3>
          <div className="desc" style={{ marginBottom: 0 }}>
            <p style={{ marginBottom: 6 }}>
              · <b>前缀字节</b>：本次请求中 append-only 历史的长度 —— 它只增不减，除非纪元重建。
            </p>
            <p style={{ marginBottom: 6 }}>
              · <b>命中率</b>来自 provider 回传的缓存 token 字段；显示 「—」 说明该 provider 未上报缓存用量。
            </p>
            <p style={{ marginBottom: 6 }}>
              · <b>持续 0%</b> 分两种情况：<b>输入 &lt; 1024 token</b> 时，OpenAI 系前缀缓存根本不会建条目（Codex
              之所以同网关能命中，是因为它每个请求自带上万 token 的系统提示 + 工具定义，远超门槛）；输入已超门槛仍 0%，则多半是<b>中转网关不路由上游缓存</b>
              （多后端轮询天然无法命中）。CCHarness 已在请求头带稳定的 <code>prompt_cache_key</code> 做路由亲和，但仍建议直连官方端点对比。
            </p>
            <p>
              · 命中率异常偏低而本地前缀链未断时，通常是纪元切换后的第一轮（预期），或 provider 侧缓存被逐出（受上游策略控制）。
            </p>
          </div>
        </div>
      </div>
    </>
  );
}
