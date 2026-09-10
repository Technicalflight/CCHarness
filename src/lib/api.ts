// Typed wrappers over the Tauri command surface. All app code talks to the
// backend exclusively through this module.
import { invoke, Channel } from "@tauri-apps/api/core";
import { open as tauriOpen } from "@tauri-apps/plugin-dialog";
import type {
  AppConfig,
  AuxStats,
  BenchCase,
  BenchRun,
  ChatImage,
  CompactionInfo,
  EnhanceOutcome,
  GoalInfo,
  GitBranch,
  GitOverview,
  GitRemote,
  GlobalStats,
  ImportCandidate,
  MarketSkill,
  McpServer,
  McpStatusEntry,
  MarketPlugin,
  MessageRecord,
  Provider,
  ProviderKind,
  SendResult,
  SessionBinding,
  SessionMeta,
  SessionTelemetry,
  SkillInfo,
  StreamEvent,
  TestResult,
  TodoItem,
  UpdateInfo,
  WriteLog,
  WriteLogEntry,
  WtInfo,
  WtState,
} from "../types";

export async function getConfig(): Promise<AppConfig> {
  return invoke<AppConfig>("get_config");
}

/** Open the OS-native directory picker; resolves null when the user cancels. */
export async function pickDirectory(title: string): Promise<string | null> {
  try {
    const sel = await tauriOpen({ directory: true, multiple: false, title });
    return typeof sel === "string" ? sel : null;
  } catch {
    return null;
  }
}

/** One entry of a workspace directory listing (preview-panel explorer). */
export interface WorkspaceEntry {
  name: string;
  is_dir: boolean;
  size: number;
}

export async function listWorkspaceDir(workspace: string, path: string): Promise<WorkspaceEntry[]> {
  return invoke<WorkspaceEntry[]>("list_workspace_dir", { workspace, path });
}

export async function openExternal(url: string): Promise<void> {
  await invoke("open_external", { url });
}

/** Check GitHub Releases for a newer version. `token` (settings
 *  `update_token`) is only needed for private-repository releases. */
export async function checkUpdate(token: string): Promise<UpdateInfo> {
  return invoke<UpdateInfo>("check_update", { token });
}

export async function getTodos(sessionId: string): Promise<TodoItem[]> {
  return invoke<TodoItem[]>("get_todos", { sessionId });
}

export async function saveConfig(config: AppConfig): Promise<void> {
  await invoke("save_config", { config });
}

/** Hide the window; the tray icon keeps the app running. */
export async function hideToTray(): Promise<void> {
  await invoke("hide_to_tray");
}

/** Real exit (bypasses the close interception). */
export async function quitApp(): Promise<void> {
  await invoke("app_quit");
}

/** One-click import providers from the local cc-switch database.
 *  Returns only the newly added entries (deduped); [] = nothing new. */
export async function importCcSwitch(): Promise<Provider[]> {
  return invoke<Provider[]>("ccswitch_import");
}

export async function testProvider(provider: Provider): Promise<TestResult> {
  return invoke<TestResult>("test_provider", { provider });
}

export async function fetchModels(provider: Provider): Promise<string[]> {
  return invoke<string[]>("fetch_models", { provider });
}

export async function listSessions(): Promise<SessionMeta[]> {
  return invoke<SessionMeta[]>("list_sessions");
}

export async function createSession(
  kind: "chat" | "arena",
  bindings: SessionBinding[],
  title: string
): Promise<SessionMeta> {
  return invoke<SessionMeta>("create_session", { kind, bindings, title });
}

export async function deleteSession(sessionId: string): Promise<void> {
  await invoke("delete_session", { sessionId });
}

export async function renameSession(sessionId: string, title: string): Promise<void> {
  await invoke("rename_session", { sessionId, title });
}

export async function updateBindings(
  sessionId: string,
  bindings: SessionBinding[]
): Promise<void> {
  await invoke("update_bindings", { sessionId, bindings });
}

export async function setWorkspace(
  sessionId: string,
  workspace: string | null
): Promise<void> {
  await invoke("set_workspace", { sessionId, workspace });
}

export async function getSessionMessages(sessionId: string): Promise<MessageRecord[]> {
  return invoke<MessageRecord[]>("get_session_messages", { sessionId });
}

export function sendMessage(
  sessionId: string,
  content: string,
  skillCalls: string[] | null,
  images: ChatImage[] | null,
  onEvent: (e: StreamEvent) => void
): Promise<SendResult> {
  const channel = new Channel<StreamEvent>();
  channel.onmessage = onEvent;
  return invoke<SendResult>("send_message", { sessionId, content, skillCalls, images, channel });
}

export function arenaSend(
  sessionId: string,
  content: string,
  skillCalls: string[] | null,
  images: ChatImage[] | null,
  lanes: SessionBinding[],
  onEvent: (e: StreamEvent) => void
): Promise<SendResult> {
  const channel = new Channel<StreamEvent>();
  channel.onmessage = onEvent;
  return invoke<SendResult>("arena_send", { sessionId, content, skillCalls, images, lanes, channel });
}

/** Sequential group chat / round-table: members speak in lane order, each
 *  later member sees the earlier replies. With `moderated`, an LLM host
 *  picks who speaks next each round until it ends the discussion.
 *  Events stream over the channel. */
export function groupSend(
  sessionId: string,
  content: string,
  skillCalls: string[] | null,
  images: ChatImage[] | null,
  lanes: SessionBinding[],
  moderated: boolean | null,
  onEvent: (e: StreamEvent) => void
): Promise<SendResult> {
  const channel = new Channel<StreamEvent>();
  channel.onmessage = onEvent;
  return invoke<SendResult>("group_send", { sessionId, content, skillCalls, images, lanes, moderated, channel });
}

/** Resolve one stored attachment to a data URI for <img> rendering. */
export async function attachmentData(sessionId: string, filename: string): Promise<string> {
  return invoke<string>("attachment_data", { sessionId, filename });
}

export async function stopGeneration(sessionId: string): Promise<void> {
  await invoke("stop_generation", { sessionId });
}

export async function resolveApproval(
  approvalId: string,
  sessionId: string,
  tool: string,
  approved: boolean,
  remember: boolean
): Promise<void> {
  await invoke("resolve_approval", { approvalId, sessionId, tool, approved, remember });
}

export async function setPermissionMode(sessionId: string, mode: string): Promise<void> {
  await invoke("set_permission_mode", { sessionId, mode });
}

/** Drop the user message at fromTs and everything after it. */
export async function rollbackSession(sessionId: string, fromTs: number): Promise<number> {
  return invoke<number>("rollback_session", { sessionId, fromTs });
}

export async function compactSession(sessionId: string): Promise<string> {
  return invoke<string>("compact_session", { sessionId });
}

export async function getSessionCompaction(sessionId: string): Promise<CompactionInfo | null> {
  return invoke<CompactionInfo | null>("get_session_compaction", { sessionId });
}

export async function mcpStatus(): Promise<McpStatusEntry[]> {
  return invoke<McpStatusEntry[]>("mcp_status");
}

export async function mcpTest(server: McpServer): Promise<TestResult> {
  return invoke<TestResult>("mcp_test", { server });
}

export async function getSkills(workspace: string | null): Promise<SkillInfo[]> {
  return invoke<SkillInfo[]>("get_skills", { workspace });
}

export async function deleteSkill(name: string): Promise<void> {
  await invoke("delete_skill", { name });
}

export async function clearSession(sessionId: string): Promise<number> {
  return invoke<number>("clear_session", { sessionId });
}

export async function skillhubList(
  page: number,
  pageSize: number,
  sortBy: string,
  keyword: string
): Promise<{ skills: MarketSkill[]; total: number }> {
  return invoke("skillhub_list", { page, pageSize, sortBy, keyword });
}

export async function skillhubInstall(
  slug: string,
  namespace: string,
  version: string | null,
  description: string
): Promise<string> {
  return invoke<string>("skillhub_install", { slug, namespace, version, description });
}

export async function skillhubPlugins(
  page: number,
  pageSize: number,
  category: string
): Promise<{ plugins: MarketPlugin[]; total: number }> {
  return invoke("skillhub_plugins", { page, pageSize, category });
}

export async function skillhubPluginInstall(
  owner: string,
  name: string,
  defaultBranch: string,
  description: string
): Promise<string> {
  return invoke<string>("skillhub_plugin_install", { owner, name, defaultBranch, description });
}

export async function getTelemetry(sessionId: string): Promise<SessionTelemetry> {
  return invoke<SessionTelemetry>("get_telemetry", { sessionId });
}

export async function getGlobalStats(): Promise<GlobalStats> {
  return invoke<GlobalStats>("get_global_stats");
}

export async function exportSession(sessionId: string): Promise<string> {
  return invoke<string>("export_session", { sessionId });
}

export async function appDataDir(): Promise<string> {
  return invoke<string>("get_app_data_dir");
}

// ---- AuxMemo ----

/** Enhance the composer draft with the session's model (exact-cached). */
export async function enhancePrompt(sessionId: string, draft: string): Promise<EnhanceOutcome> {
  return invoke<EnhanceOutcome>("enhance_prompt", { sessionId, draft });
}

export async function getAuxStats(): Promise<AuxStats> {
  return invoke<AuxStats>("get_aux_stats");
}

// ---- workflow gate (plan mode) ----

export async function setWorkflowMode(sessionId: string, mode: string): Promise<void> {
  await invoke("set_workflow_mode", { sessionId, mode });
}

export async function getWorkflowMode(sessionId: string): Promise<string> {
  return invoke<string>("get_workflow_mode", { sessionId });
}

// ---- goal state machine (persisted in SessionMeta.goal) ----

/** Create/replace the session goal; also activates the "goal" gate. */
export async function goalSet(sessionId: string, objective: string): Promise<void> {
  await invoke("goal_set", { sessionId, objective });
}

/** Goal snapshot: state + session cost + last ```goal checklist parse. */
export async function goalGet(sessionId: string): Promise<GoalInfo> {
  return invoke<GoalInfo>("goal_get", { sessionId });
}

/** Set the goal status. pause/resume/clear are user-only by design — the
 *  model's update_goal tool is hard-restricted to achieved/unmet. */
export async function goalStatus(sessionId: string, status: string): Promise<void> {
  await invoke("goal_status", { sessionId, status });
}

/** Drop the goal entirely; the "goal" gate stays until switched manually. */
export async function goalClear(sessionId: string): Promise<void> {
  await invoke("goal_clear", { sessionId });
}

// ---- declarative state machine (sm:<def>:<state>) ----

/** Full gate "sm:<def_id>:<state>" for the session, or "" when not in one. */
export async function smGet(sessionId: string): Promise<string> {
  return invoke<string>("sm_get", { sessionId });
}

/** Manual state jump (progress-bar chips). */
export async function smSet(sessionId: string, defId: string, stateName: string): Promise<void> {
  await invoke("sm_set", { sessionId, defId, stateName });
}

// ---- worktree isolation ----

export async function wtStart(sessionId: string): Promise<WtState> {
  return invoke<WtState>("wt_start", { sessionId });
}

export async function wtInfo(sessionId: string): Promise<WtInfo> {
  return invoke<WtInfo>("wt_info", { sessionId });
}

/** Full plain-text diff of the isolation branch vs its base commit. */
export async function wtDiff(sessionId: string): Promise<string> {
  return invoke<string>("wt_diff", { sessionId });
}

/** Merge the isolation branch back into the main workspace. */
export async function wtMerge(sessionId: string): Promise<string> {
  return invoke<string>("wt_merge", { sessionId });
}

/** Discard the isolation branch and worktree (frontend confirms first). */
export async function wtDiscard(sessionId: string): Promise<void> {
  await invoke("wt_discard", { sessionId });
}

// ---- git panel (main workspace management) ----

/** Status lists + branch + upstream counts + recent log for a workspace. */
export async function gitOverview(workspace: string): Promise<GitOverview> {
  return invoke<GitOverview>("git_overview", { workspace });
}

export async function gitStage(workspace: string, paths: string[]): Promise<void> {
  await invoke("git_stage", { workspace, paths });
}

export async function gitStageAll(workspace: string): Promise<void> {
  await invoke("git_stage_all", { workspace });
}

export async function gitUnstage(workspace: string, path: string): Promise<void> {
  await invoke("git_unstage", { workspace, path });
}

/** Discard one file's working-tree changes (frontend confirms first). */
export async function gitDiscard(workspace: string, path: string): Promise<void> {
  await invoke("git_discard", { workspace, path });
}

/** Commit what's staged; resolves the new short hash. */
export async function gitCommit(workspace: string, message: string): Promise<string> {
  return invoke<string>("git_commit", { workspace, message });
}

/** Combined diff of one file (untracked files get a synthesized patch). */
export async function gitFileDiff(workspace: string, path: string): Promise<string> {
  return invoke<string>("git_file_diff", { workspace, path });
}

export async function gitBranches(workspace: string): Promise<GitBranch[]> {
  return invoke<GitBranch[]>("git_branches", { workspace });
}

export async function gitSwitch(workspace: string, name: string): Promise<void> {
  await invoke("git_switch", { workspace, name });
}

// ---- git panel: remote repositories ----

export async function gitRemotes(workspace: string): Promise<GitRemote[]> {
  return invoke<GitRemote[]>("git_remotes", { workspace });
}

export async function gitRemoteAdd(workspace: string, name: string, url: string): Promise<void> {
  await invoke("git_remote_add", { workspace, name, url });
}

export async function gitRemoteRemove(workspace: string, name: string): Promise<void> {
  await invoke("git_remote_remove", { workspace, name });
}

/** Push the current branch; setUpstream (-u) links it on the first push. */
export async function gitPush(
  workspace: string,
  remote: string,
  branch: string,
  setUpstream: boolean
): Promise<string> {
  return invoke<string>("git_push", { workspace, remote, branch, setUpstream });
}

export async function gitPull(workspace: string): Promise<string> {
  return invoke<string>("git_pull", { workspace });
}

/** Fetch all remotes (with prune) so ahead/behind counters refresh. */
export async function gitFetch(workspace: string): Promise<string> {
  return invoke<string>("git_fetch", { workspace });
}

// ---- benchmark ----

export async function benchCasesDefault(): Promise<BenchCase[]> {
  return invoke<BenchCase[]>("bench_cases_default");
}

export async function benchHistory(): Promise<BenchRun[]> {
  return invoke<BenchRun[]>("bench_history");
}

export async function benchRun(
  providerId: string,
  model: string,
  cases: BenchCase[] | null,
  judge?: boolean
): Promise<BenchRun> {
  return invoke<BenchRun>("bench_run", { providerId, model, cases, judge: judge ?? false });
}

// ---- review panel ----

export async function listSessionWrites(sessionId: string): Promise<WriteLogEntry[]> {
  return invoke<WriteLogEntry[]>("list_session_writes", { sessionId });
}

export async function getWriteDiff(sessionId: string, ts: number): Promise<WriteLog | null> {
  return invoke<WriteLog | null>("get_write_diff", { sessionId, ts });
}

// ---- session flags ----

export async function setSessionPinned(sessionId: string, pinned: boolean): Promise<void> {
  await invoke("set_session_pinned", { sessionId, pinned });
}

export async function setSessionArchived(sessionId: string, archived: boolean): Promise<void> {
  await invoke("set_session_archived", { sessionId, archived });
}

/** Fork a chat session at the clicked user message (inclusive). */
export async function branchSession(sessionId: string, fromTs: number): Promise<SessionMeta> {
  return invoke<SessionMeta>("branch_session", { sessionId, fromTs });
}

// ---- @-file references ----

/** Workspace-relative paths for the @ completion menu. */
export async function searchWorkspaceFiles(workspace: string, query: string): Promise<string[]> {
  return invoke<string[]>("search_workspace_files", { workspace, query });
}

/** Read one workspace text file for @-reference expansion. */
export async function readWorkspaceFile(workspace: string, path: string): Promise<string> {
  return invoke<string>("read_workspace_file", { workspace, path });
}

// ---- session import ----

export async function importScan(source: string): Promise<ImportCandidate[]> {
  return invoke<ImportCandidate[]>("import_scan", { source });
}

export async function importSession(
  source: string,
  path: string,
  title?: string
): Promise<SessionMeta> {
  return invoke<SessionMeta>("import_session", { source, path, title: title ?? null });
}

export async function openDataDir(): Promise<void> {
  await invoke("open_data_dir");
}

export type { ProviderKind };
