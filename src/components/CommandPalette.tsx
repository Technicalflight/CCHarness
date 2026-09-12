// Ctrl+K command palette: fuzzy actions + session jumping.
import { useEffect, useMemo, useRef, useState } from "react";
import { useApp } from "../store";
import type { View } from "../store";

interface Action {
  id: string;
  label: string;
  tag: string;
  run: () => void | Promise<void>;
}

export function CommandPalette() {
  const open = useApp((s) => s.paletteOpen);
  const setPalette = useApp((s) => s.setPalette);
  // per-field selectors: this palette is ALWAYS mounted at the app root —
  // a whole-store subscription re-rendered it on every stream delta
  const setView = useApp((s) => s.setView);
  const sessions = useApp((s) => s.sessions);
  const newSession = useApp((s) => s.newSession);
  const toast = useApp((s) => s.toast);
  const config = useApp((s) => s.config);
  const persistConfig = useApp((s) => s.persistConfig);
  const selectSession = useApp((s) => s.selectSession);
  const [q, setQ] = useState("");
  const [hl, setHl] = useState(0);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        setPalette(!open);
      }
      if (e.key === "Escape") setPalette(false);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, setPalette]);

  useEffect(() => {
    if (open) {
      setQ("");
      setHl(0);
      setTimeout(() => inputRef.current?.focus(), 20);
    }
  }, [open]);

  const actions = useMemo<Action[]>(() => {
    const list: Action[] = [];
    const views: [View, string][] = [
      ["chat", "会话"],
      ["arena", "竞技场"],
      ["models", "模型管理"],
      ["mcp", "MCP 服务"],
      ["market", "技能市场"],
      ["telemetry", "缓存遥测"],
      ["bench", "Benchmark 评测"],
      ["review", "审阅"],
      ["settings", "设置"],
    ];
    for (const [v, label] of views) {
      list.push({ id: `view-${v}`, label: `打开${label}`, tag: "视图", run: () => setView(v) });
    }
    list.push({
      id: "new-chat",
      label: "新建会话",
      tag: "操作",
      run: async () => {
        try {
          const binding = sessions.find((s) => s.kind === "chat" && s.bindings[0])?.bindings[0];
          await newSession("chat", binding ? [binding] : [], "新会话");
        } catch (e) {
          toast("error", `新建会话失败: ${String(e)}`);
        }
      },
    });
    list.push({
      id: "new-arena",
      label: "新建竞技场（多模型对比）",
      tag: "操作",
      run: async () => {
        try {
          const lanes = sessions.find((s) => s.kind === "arena")?.bindings ?? [];
          await newSession("arena", lanes, "竞技场");
        } catch (e) {
          toast("error", `新建竞技场失败: ${String(e)}`);
        }
      },
    });
    list.push({
      id: "toggle-theme",
      label: "切换 深色 / 浅色主题",
      tag: "操作",
      run: async () => {
        if (!config) return;
        await persistConfig({
          ...config,
          settings: { ...config.settings, theme: config.settings.theme === "dark" ? "light" : "dark" },
        });
      },
    });
    for (const s of sessions) {
      if (s.kind === "sub") continue; // hidden sub-agent sessions
      list.push({
        id: `session-${s.id}`,
        label: s.title || "未命名会话",
        tag: s.kind === "arena" ? `${s.bindings.length} 模型` : "会话",
        run: () => selectSession(s.id),
      });
    }
    return list;
  }, [setView, sessions, newSession, toast, config, persistConfig, selectSession]);

  const filtered = useMemo(() => {
    const needle = q.trim().toLowerCase();
    if (!needle) return actions.slice(0, 12);
    return actions
      .filter((a) => a.label.toLowerCase().includes(needle) || a.tag.toLowerCase().includes(needle))
      .slice(0, 12);
  }, [actions, q]);

  if (!open) return null;

  const runAt = (i: number) => {
    const a = filtered[i];
    if (!a) return;
    setPalette(false);
    void a.run();
  };

  return (
    <div className="palette-overlay" onMouseDown={(e) => e.target === e.currentTarget && setPalette(false)}>
      <div className="palette">
        <input
          ref={inputRef}
          value={q}
          placeholder="搜索命令与会话…"
          onChange={(e) => {
            setQ(e.target.value);
            setHl(0);
          }}
          onKeyDown={(e) => {
            if (e.key === "ArrowDown") {
              e.preventDefault();
              setHl((h) => Math.min(h + 1, filtered.length - 1));
            }
            if (e.key === "ArrowUp") {
              e.preventDefault();
              setHl((h) => Math.max(h - 1, 0));
            }
            if (e.key === "Enter") {
              e.preventDefault();
              runAt(hl);
            }
          }}
        />
        <div className="palette-list">
          {filtered.length === 0 && <div className="palette-empty">没有匹配项</div>}
          {filtered.map((a, i) => (
            <button key={a.id} className={`palette-item ${i === hl ? "hl" : ""}`} onMouseEnter={() => setHl(i)} onClick={() => runAt(i)}>
              <span>{a.label}</span>
              <span className="p-tag">{a.tag}</span>
            </button>
          ))}
        </div>
      </div>
    </div>
  );
}
