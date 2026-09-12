// Central app state. All backend IO flows through here so views stay dumb.
import { create } from "zustand";
import * as api from "./lib/api";
import { notifyIfHidden } from "./lib/notify";
import type {
  AppConfig,
  ChatImage,
  MessageRecord,
  Provider,
  RequestStat,
  SessionBinding,
  SessionMeta,
  StreamEvent,
  TodoItem,
  UpdateInfo,
} from "./types";

export type View = "chat" | "arena" | "models" | "mcp" | "market" | "telemetry" | "review" | "subagents" | "workflows" | "bench" | "settings";

/** Right-side preview panel tabs. */
export type PreviewTab = "files" | "git" | "browser" | "tasks";

export interface StreamingTool {
  name: string;
  args: string;
  /** Wire call id — pairs result & live sub-agent progress to the right card. */
  callId?: string;
  result?: string;
  /** Live output streamed from a delegated sub-agent (delegate_subagent). */
  progress?: string;
  /** Wall-clock timing for the subagent progress cards. */
  startedAt?: number;
  endedAt?: number;
}

export interface PendingApproval {
  approvalId: string;
  sessionId: string;
  lane: number;
  tool: string;
  path: string;
  preview: string;
}

export interface StreamingLane {
  lane: number;
  messageId: string;
  model: string;
  content: string;
  reasoning: string;
  tools: StreamingTool[];
}

export interface Toast {
  id: number;
  kind: "info" | "success" | "error";
  text: string;
}

/** A message queued while the session is busy; auto-sent when free. */
export interface QueuedMessage {
  content: string;
  skills: string[];
  images?: ChatImage[];
}

interface AppState {
  config: AppConfig | null;
  view: View;
  sessions: SessionMeta[];
  activeSessionId: string | null;
  messages: Record<string, MessageRecord[]>;
  streaming: Record<string, StreamingLane[]>;
  busy: Record<string, boolean>;
  /** per-session send queue (chat only), drained automatically */
  queue: Record<string, QueuedMessage[]>;
  lastRequest: Record<string, RequestStat>;
  toasts: Toast[];
  paletteOpen: boolean;
  approvals: Record<string, PendingApproval>;

  /** right-side preview panel */
  previewOpen: boolean;
  previewTab: PreviewTab;
  /** workspace-relative path of the file open in the files tab */
  previewFile: string | null;
  /** left sidebar visibility (collapsed via the titlebar toggle) */
  sidebarOpen: boolean;
  /** session task lists (todo_write tool), keyed by session id */
  todos: Record<string, TodoItem[]>;
  /** bumped whenever a write-tool succeeds — files preview listens to this */
  fsVersion: number;
  /** preview panel width in logical px (draggable splitter, persisted) */
  panelW: number;

  /** in-app update check (sidebar version button / silent startup check) */
  updateInfo: UpdateInfo | null;
  updateChecking: boolean;
  updateDialogOpen: boolean;
  runUpdateCheck: (openDialog: boolean) => Promise<UpdateInfo>;
  setUpdateDialogOpen: (open: boolean) => void;

  bootstrap: () => Promise<void>;
  setView: (v: View) => void;
  /** Sidebar「会话」button: switch to the chat view AND land on the welcome
   *  hero page (clears the active session) instead of restoring the last
   *  transcript — the session list below is the way back into a chat. */
  openChatHome: () => void;
  setPalette: (open: boolean) => void;
  toast: (kind: Toast["kind"], text: string) => void;
  dismissToast: (id: number) => void;
  setPreview: (patch: { open?: boolean; tab?: PreviewTab; file?: string | null }) => void;
  toggleSidebar: () => void;
  ensureTodos: (sessionId: string) => Promise<void>;
  refreshTodos: (sessionId: string) => Promise<void>;
  setPanelW: (w: number) => void;

  refreshSessions: () => Promise<void>;
  newSession: (
    kind: "chat" | "arena",
    bindings: SessionBinding[],
    title: string
  ) => Promise<SessionMeta>;
  selectSession: (id: string) => Promise<void>;
  ensureMessages: (id: string) => Promise<void>;
  ensureFreshMessages: (id: string) => Promise<void>;
  deleteSession: (id: string) => Promise<void>;
  renameSession: (id: string, title: string) => Promise<void>;
  updateBindings: (id: string, bindings: SessionBinding[]) => Promise<void>;
  setWorkspace: (id: string, workspace: string | null) => Promise<void>;

  send: (sessionId: string, content: string, skillCalls?: string[], images?: ChatImage[]) => Promise<void>;
  sendArena: (
    sessionId: string,
    content: string,
    lanes: SessionBinding[],
    skillCalls?: string[],
    images?: ChatImage[]
  ) => Promise<void>;
  sendGroup: (
    sessionId: string,
    content: string,
    lanes: SessionBinding[],
    skillCalls?: string[],
    images?: ChatImage[],
    moderated?: boolean
  ) => Promise<void>;
  stop: (sessionId: string) => Promise<void>;

  persistConfig: (config: AppConfig) => Promise<boolean>;
  providerById: (id: string) => Provider | undefined;
}

let toastSeq = 1;

/** Tools whose success mutates workspace files — bumps fsVersion so the
 *  right-side files preview re-reads without a manual refresh. */
const WRITE_TOOLS = new Set([
  "write_file",
  "edit_file",
  "apply_patch",
  "delete_file",
  "move_path",
  "run_command",
]);

/** Approval cards live client-side until answered; the backend silently
 *  denies after 120s without broadcasting a removal — expire them here a
 *  bit later (6s grace for timer skew between backend and card render). */
const APPROVAL_TTL_MS = 126_000;

// Shared streaming-event reducer for both chat and arena sends.
function makeEventHandler(
  get: () => AppState,
  set: (fn: (s: AppState) => Partial<AppState>) => void,
  sessionId: string
) {
  return (ev: StreamEvent) => {
    if (ev.type === "started") {
      set((s) => {
        const cur = s.streaming[sessionId] ?? [];
        if (cur.some((l) => l.lane === ev.lane)) return {};
        return {
          streaming: {
            ...s.streaming,
            [sessionId]: [
              ...cur,
              { lane: ev.lane, messageId: ev.message_id, model: ev.model, content: "", reasoning: "", tools: [] },
            ],
          },
        };
      });
    } else if (ev.type === "delta" || ev.type === "reasoning") {
      set((s) => {
        const arr = s.streaming[sessionId] ?? [];
        const idx = arr.findIndex((l) => l.lane === ev.lane);
        if (idx < 0) return {};
        const lane = arr[idx];
        const next =
          ev.type === "delta"
            ? { ...lane, content: lane.content + ev.text }
            : { ...lane, reasoning: lane.reasoning + ev.text };
        const copy = [...arr];
        copy[idx] = next;
        return { streaming: { ...s.streaming, [sessionId]: copy } };
      });
    } else if (ev.type === "tool_call") {
      set((s) => {
        const arr = s.streaming[sessionId] ?? [];
        const idx = arr.findIndex((l) => l.lane === ev.lane);
        if (idx < 0) return {};
        const copy = [...arr];
        copy[idx] = {
          ...copy[idx],
          tools: [
            ...copy[idx].tools,
            { name: ev.name, args: ev.args, callId: ev.call_id, startedAt: Date.now() },
          ],
        };
        return { streaming: { ...s.streaming, [sessionId]: copy } };
      });
    } else if (ev.type === "tool_result") {
      if (ev.name === "todo_write") {
        // the task panel refreshes as soon as the model updates the list
        void get().refreshTodos(sessionId);
      }
      if (WRITE_TOOLS.has(ev.name)) {
        // file tree / preview re-read the workspace when a write lands
        set((s) => ({ fsVersion: s.fsVersion + 1 }));
      }
      set((s) => {
        const arr = s.streaming[sessionId] ?? [];
        const idx = arr.findIndex((l) => l.lane === ev.lane);
        if (idx < 0) return {};
        const lane = arr[idx];
        const tools = [...lane.tools];
        // pair by call id when present (correct for parallel delegations);
        // fall back to name+unresolved for legacy events
        for (let i = tools.length - 1; i >= 0; i--) {
          if (
            (ev.call_id && tools[i].callId === ev.call_id) ||
            (!ev.call_id && tools[i].name === ev.name && tools[i].result === undefined)
          ) {
            tools[i] = { ...tools[i], result: ev.result, endedAt: Date.now() };
            break;
          }
        }
        const copy = [...arr];
        copy[idx] = { ...lane, tools };
        return { streaming: { ...s.streaming, [sessionId]: copy } };
      });
    } else if (ev.type === "sub_progress") {
      // live output of a delegated sub-agent → stream into its tool card
      set((s) => {
        const arr = s.streaming[sessionId] ?? [];
        const idx = arr.findIndex((l) => l.lane === ev.lane);
        if (idx < 0) return {};
        const lane = arr[idx];
        const tools = [...lane.tools];
        for (let i = tools.length - 1; i >= 0; i--) {
          if (tools[i].callId === ev.call_id) {
            tools[i] = { ...tools[i], progress: (tools[i].progress ?? "") + ev.text };
            break;
          }
        }
        const copy = [...arr];
        copy[idx] = { ...lane, tools };
        return { streaming: { ...s.streaming, [sessionId]: copy } };
      });
    } else if (ev.type === "usage") {
      set((s) => ({ lastRequest: { ...s.lastRequest, [sessionId]: ev.request } }));
    } else if (ev.type === "approval_request") {
      notifyIfHidden("操作待审批", `${ev.tool}${ev.path ? ` · ${ev.path}` : ""} —— 回到窗口处理`);
      set((s) => ({
        approvals: {
          ...s.approvals,
          [ev.approval_id]: {
            approvalId: ev.approval_id,
            sessionId,
            lane: ev.lane,
            tool: ev.tool,
            path: ev.path,
            preview: ev.preview,
          },
        },
      }));
      // 幽灵卡片治理：后端超时拒绝是静默的，卡片若只靠用户点击清理
      // 会永久滞留 —— 本地到点兜底摘除（已应答的 id 不存在，直接跳过）
      const aid = ev.approval_id;
      setTimeout(() => {
        set((s) => {
          if (!(aid in s.approvals)) return {};
          const approvals = { ...s.approvals };
          delete approvals[aid];
          return { approvals };
        });
      }, APPROVAL_TTL_MS);
    } else if (ev.type === "error") {
      get().toast("error", ev.message);
    }
  };
}

// Optimistic local user record so the sent message is visible immediately;
// replaced by server truth when the stream finishes.
function appendOptimisticUser(
  set: (fn: (s: AppState) => Partial<AppState>) => void,
  sessionId: string,
  content: string,
  skillCalls: string[],
  images?: ChatImage[]
) {
  const optimistic: MessageRecord = {
    id: `local-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`,
    lane: 0,
    role: "user",
    content,
    reasoning: null,
    ts: Date.now(),
    model: null,
    status: "ok",
    usage: null,
    cost_usd: null,
    confidence: null,
    tool_calls: null,
    tool_call_id: null,
    skill_calls: skillCalls.length > 0 ? skillCalls : null,
    workflow: null,
    // data URIs render instantly; the post-turn refresh swaps in the real
    // records whose images are attachment file names
    images: images && images.length > 0 ? images.map((i) => `data:${i.mime};base64,${i.b64}`) : undefined,
  };
  set((s) => ({
    messages: { ...s.messages, [sessionId]: [...(s.messages[sessionId] ?? []), optimistic] },
  }));
}

export const useApp = create<AppState>((set, get) => ({
  config: null,
  view: "chat",
  sessions: [],
  activeSessionId: null,
  messages: {},
  streaming: {},
  busy: {},
  queue: {},
  lastRequest: {},
  toasts: [],
  paletteOpen: false,
  approvals: {},
  previewOpen: false,
  previewTab: "files",
  previewFile: null,
  sidebarOpen: localStorage.getItem("cc.sidebarOpen") !== "0",
  todos: {},
  fsVersion: 0,
  panelW: Number(localStorage.getItem("cc.panelW")) || 400,

  updateInfo: null,
  updateChecking: false,
  updateDialogOpen: false,

  runUpdateCheck: async (openDialog) => {
    set({ updateChecking: true });
    if (openDialog) set({ updateDialogOpen: true });
    try {
      const info = await api.checkUpdate();
      set({ updateInfo: info });
      if (info.update_available && !openDialog) {
        get().toast(
          "info",
          `发现新版本 v${info.latest} — 点击侧边栏底部版本号查看`
        );
      }
      return info;
    } catch (e) {
      const info: UpdateInfo = {
        current: "",
        latest: null,
        update_available: false,
        release_name: null,
        notes: null,
        url: null,
        error: String(e),
      };
      set({ updateInfo: info });
      return info;
    } finally {
      set({ updateChecking: false });
    }
  },

  setUpdateDialogOpen: (open) => set({ updateDialogOpen: open }),

  bootstrap: async () => {
    try {
      const config = await api.getConfig();
      document.documentElement.dataset.theme = config.settings.theme;
      set({ config });
    } catch (e) {
      get().toast("error", `加载配置失败: ${String(e)}`);
    }
    await get().refreshSessions();
  },

  setView: (v) => set({ view: v }),
  openChatHome: () => set({ view: "chat", activeSessionId: null }),
  setPalette: (open) => set({ paletteOpen: open }),
  setPreview: (patch) =>
    set((s) => ({
      previewOpen: patch.open ?? s.previewOpen,
      previewTab: patch.tab ?? s.previewTab,
      previewFile: patch.file !== undefined ? patch.file : s.previewFile,
    })),

  toast: (kind, text) => {
    const id = toastSeq++;
    set((s) => ({ toasts: [...s.toasts, { id, kind, text }] }));
    setTimeout(() => get().dismissToast(id), kind === "error" ? 6000 : 3200);
  },
  dismissToast: (id) => set((s) => ({ toasts: s.toasts.filter((t) => t.id !== id) })),

  refreshSessions: async () => {
    try {
      const sessions = await api.listSessions();
      set({ sessions });
    } catch (e) {
      get().toast("error", `会话列表加载失败: ${String(e)}`);
    }
  },

  newSession: async (kind, bindings, title) => {
    const meta = await api.createSession(kind, bindings, title);
    await get().refreshSessions();
    set({ activeSessionId: meta.id, view: kind === "arena" ? "arena" : "chat" });
    get().ensureMessages(meta.id);
    return meta;
  },

  selectSession: async (id) => {
    const meta = get().sessions.find((s) => s.id === id);
    set({ activeSessionId: id, view: meta?.kind === "arena" ? "arena" : "chat" });
    await get().ensureMessages(id);
    void get().ensureTodos(id);
  },

  ensureTodos: async (id) => {
    if (get().todos[id]) return;
    await get().refreshTodos(id);
  },

  refreshTodos: async (id) => {
    try {
      const todos = await api.getTodos(id);
      set((s) => ({ todos: { ...s.todos, [id]: todos } }));
    } catch {
      /* panel-only data — silent */
    }
  },

  setPanelW: (w) => {
    const clamped = Math.round(Math.min(720, Math.max(240, w)));
    try {
      localStorage.setItem("cc.panelW", String(clamped));
    } catch {
      /* ignore */
    }
    set({ panelW: clamped });
  },

  toggleSidebar: () =>
    set((s) => {
      const sidebarOpen = !s.sidebarOpen;
      try {
        localStorage.setItem("cc.sidebarOpen", sidebarOpen ? "1" : "0");
      } catch {
        /* private mode etc. — state still toggles for this run */
      }
      return { sidebarOpen };
    }),

  ensureMessages: async (id) => {
    if (get().messages[id]) return;
    await get().ensureFreshMessages(id);
  },

  ensureFreshMessages: async (id) => {
    try {
      const msgs = await api.getSessionMessages(id);
      // A send may have appended optimistic local records while this fetch
      // was in flight (hero page: newSession → ensureMessages races the
      // first send). Server truth predating the send would clobber them —
      // skip the overwrite; the post-stream refresh (after busy clears)
      // brings back the full transcript including the persisted user msg.
      // The skip only applies when local records actually exist: entering
      // a busy session for the FIRST time has nothing to clobber, and
      // skipping would leave the transcript blank for the whole stream.
      if (get().busy[id] && (get().messages[id]?.length ?? 0) > 0) return;
      set((s) => ({ messages: { ...s.messages, [id]: msgs } }));
    } catch (e) {
      get().toast("error", `消息加载失败: ${String(e)}`);
    }
  },

  deleteSession: async (id) => {
    // streaming guard: deleting mid-turn must stop the run first — the
    // orphaned stream keeps burning tokens and its events land on a dead
    // session with confusing toasts
    if (get().busy[id] || get().streaming[id]?.length) {
      try {
        await get().stop(id);
      } catch {
        /* already stopped */
      }
    }
    await api.deleteSession(id);
    set((s) => {
      const messages = { ...s.messages };
      delete messages[id];
      const todos = { ...s.todos };
      delete todos[id];
      const streaming = { ...s.streaming };
      delete streaming[id];
      const busy = { ...s.busy };
      delete busy[id];
      const queue = { ...s.queue };
      delete queue[id];
      // 会话没了，挂着写入审批卡没有任何意义 —— 一并清掉
      const approvals: Record<string, PendingApproval> = {};
      for (const [k, v] of Object.entries(s.approvals)) {
        if (v.sessionId !== id) approvals[k] = v;
      }
      const activeSessionId = s.activeSessionId === id ? null : s.activeSessionId;
      return { messages, todos, approvals, activeSessionId, streaming, busy, queue };
    });
    await get().refreshSessions();
  },

  renameSession: async (id, title) => {
    await api.renameSession(id, title);
    await get().refreshSessions();
  },

  updateBindings: async (id, bindings) => {
    await api.updateBindings(id, bindings);
    await get().refreshSessions();
    // keep the local copy of the active session in sync for the header picker
    set((s) => ({
      sessions: s.sessions.map((m) => (m.id === id ? { ...m, bindings } : m)),
    }));
  },

  setWorkspace: async (id, workspace) => {
    await api.setWorkspace(id, workspace);
    await get().refreshSessions();
    set((s) => ({
      sessions: s.sessions.map((m) => (m.id === id ? { ...m, workspace } : m)),
    }));
  },

  // ---- streaming ----

  send: async (sessionId, content, skillCalls, images) => {
    // queue while busy — drained automatically when the running turn ends
    if (get().busy[sessionId]) {
      set((s) => ({
        queue: {
          ...s.queue,
          [sessionId]: [...(s.queue[sessionId] ?? []), { content, skills: skillCalls ?? [], images }],
        },
      }));
      get().toast("info", "消息已排队 —— 当前回复结束后自动发送");
      return;
    }
    appendOptimisticUser(set, sessionId, content, skillCalls ?? [], images);
    set((s) => ({ busy: { ...s.busy, [sessionId]: true } }));
    const startedAt = Date.now();
    try {
      const onEvent = makeEventHandler(get, set, sessionId);
      await api.sendMessage(sessionId, content, skillCalls ?? null, images && images.length > 0 ? images : null, onEvent);
    } catch (e) {
      get().toast("error", `发送失败: ${String(e)}`);
    } finally {
      set((s) => {
        const streaming = { ...s.streaming };
        delete streaming[sessionId];
        return { busy: { ...s.busy, [sessionId]: false }, streaming };
      });
      await get().ensureFreshMessages(sessionId);
      await get().refreshSessions();
      // long turns that finished out of sight get a system notification
      if (Date.now() - startedAt > 5000 && get().config?.settings.notify_done !== false) {
        const title = get().sessions.find((m) => m.id === sessionId)?.title || "会话";
        notifyIfHidden("回复完成", `${title} —— 回复已就绪`);
      }
      // drain the queue: one message per finished turn
      const q = get().queue[sessionId];
      if (q && q.length > 0) {
        const [next, ...rest] = q;
        set((s) => ({ queue: { ...s.queue, [sessionId]: rest } }));
        void get().send(sessionId, next.content, next.skills, next.images);
      }
    }
  },

  sendArena: async (sessionId, content, lanes, skillCalls, images) => {
    if (get().busy[sessionId]) return;
    appendOptimisticUser(set, sessionId, content, skillCalls ?? [], images);
    set((s) => ({ busy: { ...s.busy, [sessionId]: true } }));
    const startedAt = Date.now();
    try {
      const onEvent = makeEventHandler(get, set, sessionId);
      await api.arenaSend(sessionId, content, skillCalls ?? null, images && images.length > 0 ? images : null, lanes, onEvent);
    } catch (e) {
      get().toast("error", `发送失败: ${String(e)}`);
    } finally {
      set((s) => {
        const streaming = { ...s.streaming };
        delete streaming[sessionId];
        return { busy: { ...s.busy, [sessionId]: false }, streaming };
      });
      await get().ensureFreshMessages(sessionId);
      await get().refreshSessions();
      if (Date.now() - startedAt > 8000 && get().config?.settings.notify_done !== false) {
        const title = get().sessions.find((m) => m.id === sessionId)?.title || "竞技场";
        notifyIfHidden("竞技完成", `${title} —— 各泳道回复已就绪`);
      }
    }
  },

  sendGroup: async (sessionId, content, lanes, skillCalls, images, moderated) => {
    if (get().busy[sessionId]) return;
    appendOptimisticUser(set, sessionId, content, skillCalls ?? [], images);
    set((s) => ({ busy: { ...s.busy, [sessionId]: true } }));
    const startedAt = Date.now();
    try {
      const onEvent = makeEventHandler(get, set, sessionId);
      await api.groupSend(sessionId, content, skillCalls ?? null, images && images.length > 0 ? images : null, lanes, moderated ?? null, onEvent);
    } catch (e) {
      get().toast("error", `发送失败: ${String(e)}`);
    } finally {
      set((s) => {
        const streaming = { ...s.streaming };
        delete streaming[sessionId];
        return { busy: { ...s.busy, [sessionId]: false }, streaming };
      });
      await get().ensureFreshMessages(sessionId);
      await get().refreshSessions();
      if (Date.now() - startedAt > 8000 && get().config?.settings.notify_done !== false) {
        const title = get().sessions.find((m) => m.id === sessionId)?.title || "圆桌";
        notifyIfHidden("圆桌结束", `${title} —— 全部成员已发言`);
      }
    }
  },

  stop: async (sessionId) => {
    try {
      await api.stopGeneration(sessionId);
    } catch (e) {
      get().toast("error", `停止失败: ${String(e)}`);
      return;
    }
    // 中止后该会话的审批等待者已随 lane 任务一起取消 —— 卡片立即摘除，
    // 不必等 TTL 到点；停止失败则不动卡片（turn 还在跑，审批仍有效）
    set((s) => {
      const approvals: Record<string, PendingApproval> = {};
      let hit = false;
      for (const [k, v] of Object.entries(s.approvals)) {
        if (v.sessionId === sessionId) hit = true;
        else approvals[k] = v;
      }
      return hit ? { approvals } : {};
    });
  },

  persistConfig: async (config) => {
    try {
      await api.saveConfig(config);
      document.documentElement.dataset.theme = config.settings.theme;
      set({ config });
      return true;
    } catch (e) {
      // never silent: config saves can be refused (e.g. SSRF guard)
      get().toast("error", `保存配置失败: ${String(e)}`);
      return false;
    }
  },

  providerById: (id) => get().config?.providers.find((p) => p.id === id),
}));
