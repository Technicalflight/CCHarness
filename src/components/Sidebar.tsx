import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { useApp } from "../store";
import { providerColor } from "../lib/color";
import { fmtHit } from "../lib/format";
import * as api from "../lib/api";
import type { SessionMeta } from "../types";
import type { View } from "../store";
import { Icon, type IconName } from "../lib/icons";

const NAV: { view: View; label: string; icon: IconName; hint?: string }[] = [
  { view: "chat", label: "会话", icon: "chat" },
  { view: "models", label: "模型管理", icon: "layers" },
  { view: "subagents", label: "子智能体", icon: "robot" },
  { view: "workflows", label: "工作流", icon: "branch" },
  { view: "mcp", label: "MCP 服务", icon: "plug" },
  { view: "market", label: "技能市场", icon: "bag" },
  { view: "settings", label: "设置", icon: "gear" },
];

// Secondary views tucked behind the「更多」popover (hover reveals, click pins).
const MORE_NAV: { view: View; label: string; icon: IconName }[] = [
  { view: "arena", label: "竞技场", icon: "arena" },
  { view: "telemetry", label: "缓存遥测", icon: "chart" },
  { view: "bench", label: "Benchmark", icon: "target" },
  { view: "review", label: "审阅", icon: "search" },
];

function SessionRow({
  s,
  active,
  confirmDel,
  onConfirmDel,
  onCancelDel,
  onSelect,
  onTogglePin,
  onToggleArchive,
}: {
  s: SessionMeta;
  active: boolean;
  confirmDel: boolean;
  onConfirmDel: () => void;
  onCancelDel: () => void;
  onSelect: () => void;
  onTogglePin: () => void;
  onToggleArchive: () => void;
}) {
  return (
    <div className={`session-item ${active ? "active" : ""}`} onClick={onSelect}>
      {s.pinned && <span className="s-pin" title="已置顶"><Icon name="pinSolid" size={12} /></span>}
      <span className="s-title">{s.title || "未命名"}</span>
      {s.kind === "arena" && <span className="s-badge">{s.bindings.length} 模型</span>}
      <span className="s-acts">
        {confirmDel ? (
          <>
            <button
              className="s-act"
              title="确认删除"
              onClick={(e) => {
                e.stopPropagation();
                onConfirmDel();
              }}
            >
              <Icon name="check" size={13} />
            </button>
            <button
              className="s-act"
              title="取消"
              onClick={(e) => {
                e.stopPropagation();
                onCancelDel();
              }}
            >
              <Icon name="x" size={13} />
            </button>
          </>
        ) : (
          <>
            <button
              className="s-act"
              title={s.pinned ? "取消置顶" : "置顶会话"}
              onClick={(e) => {
                e.stopPropagation();
                onTogglePin();
              }}
            >
              {s.pinned ? <Icon name="pinSolid" size={13} /> : <Icon name="pin" size={13} />}
            </button>
            <button
              className="s-act"
              title={s.archived ? "取消归档" : "归档会话"}
              onClick={(e) => {
                e.stopPropagation();
                onToggleArchive();
              }}
            >
              {s.archived ? <Icon name="unarchive" size={13} /> : <Icon name="archive" size={13} />}
            </button>
            <button
              className="s-act"
              title="删除会话"
              onClick={(e) => {
                e.stopPropagation();
                onCancelDel();
                setTimeout(onConfirmDel, 2500);
              }}
            >
              <Icon name="x" size={13} />
            </button>
          </>
        )}
      </span>
    </div>
  );
}

export function Sidebar() {
  const { view, setView, openChatHome, sessions, activeSessionId, selectSession, deleteSession, newSession, refreshSessions, lastRequest, config } =
    useApp();
  const [confirmDel, setConfirmDel] = useState<string | null>(null);
  const [showArchived, setShowArchived] = useState(false);
  // 「更多」popover: hover or click OPENS it. Moving from the button across
  // the positioning gap onto the menu must NOT close it, yet a fly-by hover
  // must not leave it open forever → grace-period close: leaving the
  // button+menu area arms a short timer, re-entering cancels it. Clicking
  // anywhere outside closes it immediately (document-level listener — the
  // menu is a DOM child of the container, so clicks inside never hit it).
  const [moreOpen, setMoreOpen] = useState(false);
  // the popover is fixed-positioned next to the rail (inline styles) so the
  // sidebar's overflow:hidden can never clip it
  const moreRef = useRef<HTMLDivElement>(null);
  const moreCloseTimer = useRef<number | null>(null);
  const [morePos, setMorePos] = useState({ left: 200, top: 60 });
  const moreActive = MORE_NAV.some((n) => n.view === view);
  useLayoutEffect(() => {
    if (moreOpen && moreRef.current) {
      const r = moreRef.current.getBoundingClientRect();
      setMorePos({
        left: r.right + 8,
        top: Math.max(8, Math.min(r.top, window.innerHeight - 230)),
      });
    }
  }, [moreOpen]);
  const cancelMoreClose = () => {
    if (moreCloseTimer.current != null) {
      window.clearTimeout(moreCloseTimer.current);
      moreCloseTimer.current = null;
    }
  };
  const armMoreClose = () => {
    if (moreCloseTimer.current == null) {
      moreCloseTimer.current = window.setTimeout(() => {
        moreCloseTimer.current = null;
        setMoreOpen(false);
      }, 450);
    }
  };
  useEffect(() => {
    if (!moreOpen) return;
    const onDoc = (e: MouseEvent) => {
      if (moreRef.current && !moreRef.current.contains(e.target as Node)) {
        cancelMoreClose();
        setMoreOpen(false);
      }
    };
    document.addEventListener("mousedown", onDoc);
    return () => document.removeEventListener("mousedown", onDoc);
  }, [moreOpen]);
  useEffect(() => cancelMoreClose, []); // unmount cleanup

  const hit = lastRequest[activeSessionId ?? ""];
  const providerCount = config?.providers.filter((p) => p.enabled && p.api_key).length ?? 0;

  // sub-agent sessions live on disk (inspectable via the parent's tool
  // records) but never appear as sidebar entries
  const visible = sessions.filter((s) => s.kind !== "sub");
  const pinned = visible.filter((s) => s.pinned && !s.archived);
  const normal = visible.filter((s) => !s.pinned && !s.archived);
  const archived = visible.filter((s) => s.archived);

  const togglePin = async (s: SessionMeta) => {
    try {
      await api.setSessionPinned(s.id, !s.pinned);
      await refreshSessions();
    } catch {
      /* silent: flags are cosmetic */
    }
  };
  const toggleArchive = async (s: SessionMeta) => {
    try {
      await api.setSessionArchived(s.id, !s.archived);
      await refreshSessions();
    } catch {
      /* silent */
    }
  };

  const row = (s: SessionMeta) => (
    <SessionRow
      key={s.id}
      s={s}
      active={activeSessionId === s.id}
      confirmDel={confirmDel === s.id}
      onConfirmDel={() => {
        setConfirmDel(null);
        void deleteSession(s.id);
      }}
      onCancelDel={() => setConfirmDel((c) => (c === s.id ? null : c))}
      onSelect={() => void selectSession(s.id)}
      onTogglePin={() => void togglePin(s)}
      onToggleArchive={() => void toggleArchive(s)}
    />
  );

  return (
    <aside className="sidebar">
      {NAV.map((n) => (
        <button
          key={n.view}
          className={`nav-item ${view === n.view ? "active" : ""}`}
          title={n.view === "chat" ? "回到会话欢迎页 —— 点击下方会话列表可回到具体会话" : undefined}
          onClick={() => (n.view === "chat" ? openChatHome() : setView(n.view))}
        >
          <span className="nav-ico"><Icon name={n.icon} size={17} /></span>
          <span className="nav-label">{n.label}</span>
          {n.view === "chat" && <span className="kbd-hint">Ctrl K</span>}
        </button>
      ))}

      {/* secondary views behind a「更多」popover */}
      <div
        ref={moreRef}
        className={`nav-more ${moreOpen ? "open" : ""} ${moreActive ? "active" : ""}`}
        onMouseEnter={() => {
          cancelMoreClose();
          setMoreOpen(true);
        }}
        onMouseLeave={armMoreClose}
      >
        <button
          className={`nav-item ${moreActive ? "active" : ""}`}
          title="更多页面"
          onClick={() => setMoreOpen((o) => !o)}
        >
          <span className="nav-ico"><Icon name="dots" size={17} /></span>
          <span className="nav-label">更多</span>
          <span className="kbd-hint">{moreOpen ? "▾" : "▸"}</span>
        </button>
        {moreOpen && (
          <div className="nav-more-pop" style={{ position: "fixed", left: morePos.left, top: morePos.top, zIndex: 60 }}>
            {MORE_NAV.map((n) => (
              <button
                key={n.view}
                className={`nav-item ${view === n.view ? "active" : ""}`}
                onClick={() => {
                  setView(n.view);
                  setMoreOpen(false);
                }}
              >
                <span className="nav-ico"><Icon name={n.icon} size={17} /></span>
                <span className="nav-label">{n.label}</span>
              </button>
            ))}
          </div>
        )}
      </div>

      <div className="sidebar-sep">
        <span className="sep-label">会话</span>
        <button
          className="mini-btn"
          title="新建会话"
          onClick={async () => {
            const binding = sessions.find((s) => s.kind === "chat" && s.bindings[0])?.bindings[0];
            await newSession("chat", binding ? [binding] : [], "新会话");
          }}
        >
          ＋
        </button>
      </div>

      <div className="session-list">
        {sessions.length === 0 && (
          <div style={{ padding: "14px 10px", fontSize: 12, color: "var(--text-faint)" }}>
            还没有会话。点击 ＋ 新建，或 Ctrl K 打开命令面板。
          </div>
        )}
        {pinned.length > 0 && (
          <div className="s-group-label">
            <Icon name="pin" size={12} /> 置顶
          </div>
        )}
        {pinned.map(row)}
        {pinned.length > 0 && normal.length > 0 && <div className="s-group-label">最近</div>}
        {normal.map(row)}
        {archived.length > 0 && (
          <>
            <button className="s-group-label as-btn" onClick={() => setShowArchived((o) => !o)}>
              <Icon name="archive" size={12} /> 归档（{archived.length}）{showArchived ? "▾" : "▸"}
            </button>
            {showArchived && archived.map(row)}
          </>
        )}
      </div>

      <div className="sidebar-foot">
        <span>v0.1.0 · {providerCount} 个 Provider 就绪</span>
        {hit && hit.cached_tokens != null && hit.input_tokens ? (
          <span className="hit-pill">缓存 {fmtHit(hit.cached_tokens, hit.input_tokens)}</span>
        ) : null}
      </div>
    </aside>
  );
}
