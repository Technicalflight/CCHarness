// 安全策略自定义弹窗：字符串名单编辑（增删 / 恢复默认 / 可选开关），
// 沙箱的文件 / 命令 / 网络三类策略共用；附带伪匿名化映射日志查看弹窗。
import { useEffect, useState } from "react";
import type { PrivacyLogEntry } from "../types";
import * as api from "../lib/api";

export interface PolicyListSpec {
  key: string;
  label: string;
  hint?: string;
  placeholder?: string;
  items: string[];
}

export interface PolicyToggleSpec {
  key: string;
  label: string;
  hint?: string;
  value: boolean;
}

export function PolicyDialog(props: {
  title: string;
  description?: string;
  lists: PolicyListSpec[];
  toggles?: PolicyToggleSpec[];
  defaults: { lists: Record<string, string[]>; toggles?: Record<string, boolean> };
  onSave: (lists: Record<string, string[]>, toggles: Record<string, boolean>) => void;
  onClose: () => void;
}) {
  const [lists, setLists] = useState<Record<string, string[]>>(() =>
    Object.fromEntries(props.lists.map((l) => [l.key, [...l.items]])),
  );
  const [toggles, setToggles] = useState<Record<string, boolean>>(() =>
    Object.fromEntries((props.toggles ?? []).map((t) => [t.key, t.value])),
  );
  const [draft, setDraft] = useState<Record<string, string>>({});

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") props.onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [props.onClose]);

  const addItem = (key: string) => {
    const v = (draft[key] ?? "").trim();
    if (!v) return;
    setLists((s) => ({ ...s, [key]: [...(s[key] ?? []), v] }));
    setDraft((d) => ({ ...d, [key]: "" }));
  };
  const delItem = (key: string, i: number) =>
    setLists((s) => ({ ...s, [key]: (s[key] ?? []).filter((_, idx) => idx !== i) }));
  const reset = () => {
    setLists(
      Object.fromEntries(
        props.lists.map((l) => [l.key, [...(props.defaults.lists[l.key] ?? [])]]),
      ),
    );
    if (props.toggles?.length) {
      setToggles(
        Object.fromEntries(
          props.toggles.map((t) => [t.key, props.defaults.toggles?.[t.key] ?? false]),
        ),
      );
    }
  };

  return (
    <div className="sec-mask" onMouseDown={(e) => e.target === e.currentTarget && props.onClose()}>
      <div className="sec-dialog" role="dialog" aria-label={props.title}>
        <h3>{props.title}</h3>
        {props.description && <div className="sec-desc">{props.description}</div>}
        {(props.toggles ?? []).map((t) => (
          <div key={t.key} className="row" style={{ marginBottom: 0 }}>
            <button
              className={`switch ${toggles[t.key] ? "on" : ""}`}
              role="switch"
              aria-checked={!!toggles[t.key]}
              onClick={() => setToggles((s) => ({ ...s, [t.key]: !s[t.key] }))}
            />
            <span style={{ fontSize: 13 }}>
              <strong>{t.label}</strong>
              {t.hint && <span style={{ color: "var(--text-dim)" }}> —— {t.hint}</span>}
            </span>
          </div>
        ))}
        {props.lists.map((l) => (
          <div key={l.key} className="sec-list">
            <div className="sec-list-head">{l.label}</div>
            {l.hint && <div className="sec-list-hint">{l.hint}</div>}
            {(lists[l.key]?.length ?? 0) > 0 && (
              <div className="sec-chips">
                {(lists[l.key] ?? []).map((item, i) => (
                  <span key={`${item}-${i}`} className="sec-chip">
                    <span>{item}</span>
                    <button
                      title="删除"
                      onClick={() => delItem(l.key, i)}
                      aria-label={`删除 ${item}`}
                    >
                      ×
                    </button>
                  </span>
                ))}
              </div>
            )}
            <div className="sec-add">
              <input
                className="input mono"
                placeholder={l.placeholder ?? "输入后回车或点添加"}
                value={draft[l.key] ?? ""}
                onChange={(e) => setDraft((d) => ({ ...d, [l.key]: e.target.value }))}
                onKeyDown={(e) => {
                  if (e.key === "Enter") {
                    e.preventDefault();
                    addItem(l.key);
                  }
                }}
              />
              <button className="btn small" onClick={() => addItem(l.key)}>
                添加
              </button>
            </div>
          </div>
        ))}
        <div className="sec-foot">
          <button className="btn small ghost" onClick={reset} style={{ marginRight: "auto" }}>
            恢复默认
          </button>
          <button className="btn small ghost" onClick={props.onClose}>
            取消
          </button>
          <button className="btn small" onClick={() => props.onSave(lists, toggles)}>
            保存
          </button>
        </div>
      </div>
    </div>
  );
}

const KIND_LABELS: Record<string, string> = {
  apikey: "API Key",
  email: "邮箱",
  userpath: "用户路径",
  idcard: "身份证",
  bank: "银行卡",
  phone: "手机号",
  secret: "密钥",
  ipv4: "IP 地址",
  custom: "自定义",
};

export function PrivacyLogDialog(props: {
  onClose: () => void;
  toast: (kind: "success" | "error" | "info", msg: string) => void;
}) {
  const [rows, setRows] = useState<PrivacyLogEntry[] | null>(null);
  const [busy, setBusy] = useState(false);
  const [armed, setArmed] = useState(false);

  const load = () => {
    setBusy(true);
    api
      .privacyLogTail(300)
      .then(setRows)
      .catch((e) => props.toast("error", String(e)))
      .finally(() => setBusy(false));
  };
  useEffect(() => {
    load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const doClear = async () => {
    try {
      await api.privacyLogClear();
      setRows([]);
      setArmed(false);
      props.toast("success", "映射日志已清空（进行中会话的内存映射不受影响）");
    } catch (e) {
      props.toast("error", String(e));
    }
  };

  return (
    <div className="sec-mask" onMouseDown={(e) => e.target === e.currentTarget && props.onClose()}>
      <div className="sec-dialog" role="dialog" aria-label="伪匿名化映射日志" style={{ width: "min(860px, 94vw)" }}>
        <h3>伪匿名化 · 映射日志</h3>
        <div className="sec-desc">
          记录每一条新建立的「原文 → 替身」映射（最多保留最近 300 条，文件超过 5MB 自动裁剪）。
          日志只保存在本机应用数据目录，不会上传；关闭伪匿名化模式后仍可查看历史记录。
        </div>
        <div className="sec-log-scroll">
          {rows === null ? (
            <div className="sec-empty">{busy ? "加载中…" : ""}</div>
          ) : rows.length === 0 ? (
            <div className="sec-empty">
              暂无映射记录 —— 开启伪匿名化并在对话中出现敏感信息后，这里会显示匿名化明细
            </div>
          ) : (
            <table className="sec-log-table">
              <thead>
                <tr>
                  <th style={{ width: 150 }}>时间</th>
                  <th style={{ width: 80 }}>类型</th>
                  <th>原文 → 替身</th>
                  <th style={{ width: 90 }}>会话</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((r, i) => (
                  <tr key={i}>
                    <td className="mono">{r.ts}</td>
                    <td>{KIND_LABELS[r.kind] ?? r.kind}</td>
                    <td className="mono">
                      {r.original} <span style={{ color: "var(--accent)" }}>→</span> {r.surrogate}
                    </td>
                    <td className="mono">{r.session.slice(0, 8)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </div>
        <div className="sec-foot">
          <button
            className={`btn small ${armed ? "" : "ghost"}`}
            style={armed ? { background: "var(--warn-soft)", color: "var(--warn)", borderColor: "var(--warn)" } : undefined}
            onClick={() => {
              if (!armed) {
                setArmed(true);
                setTimeout(() => setArmed(false), 3000);
              } else {
                void doClear();
              }
            }}
          >
            {armed ? "再点一次确认清空" : "清空日志"}
          </button>
          <button className="btn small ghost" onClick={load} disabled={busy}>
            刷新
          </button>
          <button className="btn small" onClick={props.onClose}>
            关闭
          </button>
        </div>
      </div>
    </div>
  );
}
