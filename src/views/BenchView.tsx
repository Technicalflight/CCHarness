// Benchmark: run a built-in (or custom) Q&A case list against one model and
// grade replies by expected-keyword matching (optionally re-graded by an
// LLM-as-judge); runs persist to history.
import { useEffect, useMemo, useState } from "react";
import { useApp } from "../store";
import { Dropdown } from "../components/Dropdown";
import { Icon } from "../lib/icons";
import { fmtTime } from "../lib/format";
import * as api from "../lib/api";
import type { BenchCase, BenchRun } from "../types";

export function BenchView() {
  // per-field selectors — a bare useApp() re-renders on every stream delta
  const config = useApp((s) => s.config);
  const toast = useApp((s) => s.toast);
  const providers = useMemo(
    () => (config?.providers ?? []).filter((p) => p.enabled && p.api_key.trim().length > 0),
    [config]
  );
  const [providerId, setProviderId] = useState("");
  const [model, setModel] = useState("");
  const [cases, setCases] = useState<BenchCase[]>([]);
  const [running, setRunning] = useState(false);
  const [judge, setJudge] = useState(() => {
    try {
      return localStorage.getItem("ccharness-bench-judge") === "1";
    } catch {
      return false;
    }
  });
  const [lastRun, setLastRun] = useState<BenchRun | null>(null);
  const [history, setHistory] = useState<BenchRun[]>([]);

  const activeProvider = providers.find((p) => p.id === providerId);

  useEffect(() => {
    void api.benchCasesDefault().then(setCases).catch(() => setCases([]));
    void api.benchHistory().then(setHistory).catch(() => setHistory([]));
  }, []);

  useEffect(() => {
    if (providers.length && !providers.some((p) => p.id === providerId)) {
      setProviderId(providers[0].id);
      setModel(providers[0].models[0] ?? "");
    }
  }, [providers, providerId]);

  const pickProvider = (pid: string) => {
    setProviderId(pid);
    const p = providers.find((x) => x.id === pid);
    setModel(p?.models[0] ?? "");
  };

  const toggleJudge = (v: boolean) => {
    setJudge(v);
    try {
      localStorage.setItem("ccharness-bench-judge", v ? "1" : "0");
    } catch {
      /* keep in-memory only */
    }
  };

  const run = async () => {
    if (!providerId || !model.trim() || running) return;
    setRunning(true);
    setLastRun(null);
    try {
      const r = await api.benchRun(providerId, model.trim(), null, judge);
      setLastRun(r);
      setHistory(await api.benchHistory().catch(() => []));
      toast(
        r.passed === r.total ? "success" : "info",
        `评测完成：${r.passed}/${r.total} 通过${judge ? "（含 LLM 评审）" : ""}`
      );
    } catch (e) {
      toast("error", `评测失败: ${String(e)}`);
    } finally {
      setRunning(false);
    }
  };

  return (
    <>
      <div className="view-header">
        <div>
          <div className="view-title">Benchmark 评测</div>
          <div className="view-sub">固定用例逐题非流式问答 · 关键字判分 · 历史可跨模型对比</div>
        </div>
        <div className="spacer" />
        <button
          className={`btn small ${judge ? "primary" : ""}`}
          style={{ marginRight: 10 }}
          title={
            judge
              ? "LLM-as-judge 已开启 —— 关键字判分后再由模型评审（pass/score/reason），评测耗时约翻倍"
              : "开启 LLM-as-judge —— 关键字判分之外再由模型给每题评分"
          }
          onClick={() => toggleJudge(!judge)}
        >
          <Icon name="spark" size={13} /> LLM 评审{judge ? "已开" : ""}
        </button>
        <button className="btn primary" disabled={running || !providerId || !model.trim()} onClick={() => void run()}>
          {running ? "评测中…" : "▶ 运行评测"}
        </button>
      </div>

      <div className="view-body">
        <div className="card">
          <h3>目标模型</h3>
          <div className="row" style={{ gap: 10, marginTop: 10, flexWrap: "wrap" }}>
            <label style={{ fontSize: 12, color: "var(--text-dim)", minWidth: 220 }}>
              Provider
              <div style={{ marginTop: 4 }}>
                <Dropdown
                  value={providerId}
                  options={providers.map((p) => ({ value: p.id, label: p.name }))}
                  onChange={pickProvider}
                  minWidth={200}
                />
              </div>
            </label>
            <label style={{ fontSize: 12, color: "var(--text-dim)", minWidth: 220 }}>
              模型
              <div style={{ marginTop: 4 }}>
                <Dropdown
                  value={model}
                  options={(activeProvider?.models ?? []).map((m) => ({ value: m, label: m }))}
                  onChange={setModel}
                  minWidth={200}
                />
              </div>
            </label>
          </div>
          <div className="hint" style={{ marginTop: 10 }}>
            内置 {cases.length} 个用例：算术 · 逻辑 · 中文理解 · 指令跟随 · 代码 · 概括 · 建议质量 · 常识。
            回复命中任一期望关键字即判通过；每题独立计时（非流式，超时 120 秒）。
          </div>
        </div>

        {lastRun && (
          <div className="card">
            <h3>
              最近一次结果
              <span className="chip" style={{ marginLeft: 10 }}>
                {lastRun.model} · {lastRun.passed}/{lastRun.total} 通过
              </span>
            </h3>
            <table style={{ width: "100%", marginTop: 10, fontSize: 12.5, borderCollapse: "collapse" }}>
              <tbody>
                {lastRun.results.map((r) => {
                  const c = cases.find((x) => x.id === r.case_id);
                  return (
                    <tr key={r.case_id} style={{ borderTop: "1px solid var(--border)" }}>
                      <td style={{ padding: "8px 8px 8px 0", whiteSpace: "nowrap", verticalAlign: "top" }}>
                        {r.passed ? (
                          <span className="chip" style={{ color: "var(--ok)" }}>✓ 通过</span>
                        ) : (
                          <span className="chip warn">✗ 失败</span>
                        )}
                      </td>
                      <td style={{ padding: 8, verticalAlign: "top" }}>
                        <div style={{ fontWeight: 600 }}>{c?.question ?? r.case_id}</div>
                        <div style={{ color: "var(--text-dim)", marginTop: 4 }}>
                          {r.error
                            ? `错误：${r.error}`
                            : r.reply_preview || "（空回复）"}
                        </div>
                      </td>
                      <td style={{ padding: 8, textAlign: "right", whiteSpace: "nowrap", verticalAlign: "top" }}>
                        <div className="mono">{r.latency_ms} ms</div>
                        {r.hit && (
                          <div style={{ color: "var(--text-dim)", marginTop: 4 }}>命中 “{r.hit}”</div>
                        )}
                        {r.judge_passed != null && (
                          <div style={{ marginTop: 4 }} title={r.judge_reason ?? ""}>
                            <span
                              className="chip"
                              style={{ color: r.judge_passed ? "var(--ok)" : "var(--warn, var(--bad))" }}
                            >
                              评审 {r.judge_passed ? "✓" : "✗"}
                              {r.judge_score != null ? ` ${r.judge_score}/10` : ""}
                            </span>
                          </div>
                        )}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}

        <div className="card">
          <h3>历史记录</h3>
          {history.length === 0 ? (
            <div className="desc" style={{ marginTop: 8 }}>
              还没有评测记录 —— 选择模型后点击「运行评测」。
            </div>
          ) : (
            <div style={{ marginTop: 8, display: "flex", flexDirection: "column", gap: 6 }}>
              {history.map((h) => {
                const pct = h.total ? Math.round((h.passed / h.total) * 100) : 0;
                return (
                  <div
                    key={h.id}
                    className="row"
                    style={{ gap: 10, padding: "6px 0", borderTop: "1px solid var(--border)" }}
                  >
                    <span className="mono" style={{ minWidth: 150 }}>
                      {h.model}
                    </span>
                    <span
                      className="mono"
                      style={{ color: pct >= 80 ? "var(--ok)" : pct >= 50 ? "inherit" : "var(--bad)" }}
                    >
                      {h.passed}/{h.total}（{pct}%）
                    </span>
                    <span style={{ color: "var(--text-dim)", marginLeft: "auto" }}>{fmtTime(h.ts)}</span>
                  </div>
                );
              })}
            </div>
          )}
        </div>

        {providers.length === 0 && (
          <div className="card" style={{ textAlign: "center", padding: "30px 20px" }}>
            <Icon name="target" size={26} />
            <div style={{ marginTop: 10, fontSize: 13.5, fontWeight: 600 }}>没有可用的 Provider</div>
            <div className="desc" style={{ marginTop: 6 }}>
              先到「模型管理」配置并启用一个 Provider（需填写 API Key）。
            </div>
          </div>
        )}
      </div>
    </>
  );
}
