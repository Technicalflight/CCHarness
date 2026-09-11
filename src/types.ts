// Shared contract types. Field names mirror the Rust side (serde) —
// any change here must be mirrored in src-tauri/src/*.

export type ProviderKind = "openai_compatible" | "anthropic";

export interface Pricing {
  input_per_m: number;
  cached_per_m: number;
  output_per_m: number;
}

/** Per-model behavior overrides; null/"default" = follow global/provider default. */
export interface ModelBehavior {
  max_output: number | null;
  temperature: number | null;
  reasoning: string | null;
}

export interface Provider {
  id: string;
  name: string;
  kind: ProviderKind;
  base_url: string;
  api_key: string;
  models: string[];
  enabled: boolean;
  /** Explicit user consent required before loopback/private endpoints are allowed. */
  allow_local: boolean;
  /** Model context window in tokens for the usage meter (default 128k). */
  context_window: number | null;
  pricing: Record<string, Pricing>;
  behavior: Record<string, ModelBehavior>;
}

export interface AppSettings {
  theme: "dark" | "light";
  send_on_enter: boolean;
  system_prompt: string;
  agent_tools: boolean;
  /** Reasoning effort: "default" (omitted) | "low" | "medium" | "high". */
  thinking_level: string;
  /** System notifications for turn-finished / approval-needed while hidden. */
  notify_done: boolean;
  /** Window close behavior: "ask" | "tray" | "quit". */
  close_action: "ask" | "tray" | "quit";
  /** Vector long-term memory master switch. */
  vector_memory?: boolean;
  /** OpenAI-compatible embeddings endpoint (e.g. https://…/v1/embeddings). */
  embeddings_url?: string;
  embeddings_key?: string;
  embeddings_model?: string;
  /** Automatic post-turn reflection into vector memory. */
  auto_reflect?: boolean;
  /** Prompt-injection guardrails for untrusted external content. */
  guardrails?: boolean;
  /** User-defined extra injection patterns (one per line in the UI). */
  guardrails_extra?: string[];
  /** Goal-mode soft budget (USD, per session): reaching it turns the next
   *  auto-continue into a wrap-up nudge, then stops. Null = unlimited. */
  goal_budget_usd?: number | null;
  /** Post-write verification hook: run this command at the workspace root
   *  after every successful file-mutating tool and append its output to the
   *  tool result. Null/empty = disabled. */
  post_write_command?: string | null;
}

/** Result of an update check (backend `check_update`). */
export interface UpdateInfo {
  current: string;
  latest: string | null;
  update_available: boolean;
  release_name: string | null;
  notes: string | null;
  url: string | null;
  error: string | null;
}

/** Persisted goal state machine (aligns with Codex /goal five states). */
export interface GoalState {
  objective: string;
  status: "active" | "paused" | "achieved" | "unmet" | "budget_limited";
  created_at: number;
  updated_at: number;
}

/** One goal-mode round snapshot (iteration timeline, ZCode parity). */
export interface GoalRound {
  round: number;
  title: string;
  done: number;
  total: number;
  claimed: number;
  ts: number;
}

/** Goal snapshot: state + session cost + last checklist parse. */
export interface GoalInfo {
  goal: GoalState | null;
  cost_usd: number | null;
  checklist_done: number;
  checklist_total: number;
  checklist_all_met: boolean;
  /** ✅ rows with no inline evidence (claimed, not verified). */
  checklist_claimed: number;
  /** Per-turn iteration timeline, oldest first (capped at 60). */
  goal_rounds: GoalRound[];
}

/** Miss-divergence localization entry (see divergence.rs). */
export interface Divergence {
  seq: number;
  ts: number;
  kind: "chain_broken" | "prefix_shrunk" | "tail_dominant" | "upstream_loss" | "partial_drop";
  detail: string;
}

/** Named subagent profile for delegate_subagent (Subagents page). */
export interface SubagentProfile {
  id: string;
  name: string;
  description: string;
  provider_id: string;
  model: string;
  system_prompt: string;
  enabled: boolean;
}

/** One ordered conditional-branch rule: the FIRST matching `when` wins. */
export interface SmBranch {
  /**
   * Predicate: "" | "always" | "ok" | "error" | "contains:<text>" |
   * "not_contains:<text>" | "tool_used:<name>".
   */
  when: string;
  /** Target state name when the predicate matches. */
  goto: string;
}

/** One state of a declarative workflow (state machine). */
export interface SmState {
  name: string;
  /** Injected verbatim ahead of every user message sent in this state. */
  directive: string;
  /** Tool surface while in this state. */
  tools: "none" | "readonly" | "full";
  /** Auto-advance target after a successful turn (null = hold). */
  next: string | null;
  /** Terminal states stop the auto-advance. */
  terminal: boolean;
  /** Ordered conditional branches, evaluated before `next`. */
  branches: SmBranch[];
  /** Parallel fan-out: state names run concurrently as subagents. */
  parallel: string[];
}

/** Declarative workflow definition — states[0] is the entry state. */
export interface WorkflowDef {
  id: string;
  name: string;
  description: string;
  enabled: boolean;
  states: SmState[];
}

export interface AppConfig {
  version: number;
  providers: Provider[];
  mcp_servers: McpServer[];
  subagents: SubagentProfile[];
  workflows?: WorkflowDef[];
  settings: AppSettings;
}

/** One benchmark case: question + ANY-match pass keywords. */
export interface BenchCase {
  id: string;
  question: string;
  expect_any: string[];
}

export interface BenchCaseResult {
  case_id: string;
  passed: boolean;
  hit: string | null;
  error: string | null;
  latency_ms: number;
  reply_preview: string;
  /** LLM-as-judge verdict (absent when judge mode off / call failed). */
  judge_passed?: boolean | null;
  judge_score?: number | null;
  judge_reason?: string | null;
}

export interface BenchRun {
  id: string;
  ts: number;
  provider_id: string;
  model: string;
  passed: number;
  total: number;
  judge?: boolean;
  results: BenchCaseResult[];
}

/** Active git-worktree isolation for a session (worktree isolation). */
export interface WtState {
  /** Absolute worktree path (under the app data dir, never in the workspace). */
  path: string;
  /** Branch checked out in the worktree ("cch/<sid8>-<ts>"). */
  branch: string;
  /** Main-workspace HEAD at creation — the diff/merge base. */
  base_head: string;
  created_at: number;
}

/** One changed file inside the isolation worktree (capsule badge list). */
export interface WtFileInfo {
  /** Porcelain status letter: M/A/D/R/U/?. */
  status: string;
  path: string;
}

export interface WtInfo {
  wt: WtState | null;
  files: WtFileInfo[];
}

// ---- git management panel (main workspace) ----

export interface GitCommit {
  hash: string;
  subject: string;
  author: string;
  /** Unix seconds. */
  ts: number;
}

export interface GitFile {
  /** Porcelain letter: M/A/D/R/U/C/T or "?" for untracked. */
  status: string;
  path: string;
}

export interface GitOverview {
  repo: boolean;
  branch: string | null;
  ahead: number | null;
  behind: number | null;
  staged: GitFile[];
  unstaged: GitFile[];
  untracked: GitFile[];
  log: GitCommit[];
}

export interface GitBranch {
  name: string;
  current: boolean;
}

export interface GitRemote {
  name: string;
  /** Fetch URL (push URL is almost always identical). */
  url: string;
}

export interface SessionMeta {
  id: string;
  title: string;
  /** "chat" | "arena" | "sub" (sub = hidden background sub-agent session) */
  kind: "chat" | "arena" | "sub";
  created_at: number;
  updated_at: number;
  /** chat: single binding. arena: one per lane. */
  bindings: SessionBinding[];
  /** Workspace root for agent tools + AGENTS.md; null = not bound. */
  workspace: string | null;
  /** Pinned sessions sort first in the sidebar. */
  pinned: boolean;
  /** Archived sessions collapse into a dedicated sidebar section. */
  archived: boolean;
  /** Active git-worktree isolation, if any. */
  wt?: WtState | null;
}

export interface ToolCallInfo {
  id: string;
  name: string;
  arguments: string;
}

export interface SessionBinding {
  provider_id: string;
  model: string;
}

export type MessageStatus = "ok" | "stopped" | "error";

export interface MessageRecord {
  id: string;
  lane: number;
  role: "system" | "user" | "assistant" | "tool";
  content: string;
  reasoning: string | null;
  ts: number;
  model: string | null;
  status: MessageStatus;
  usage: UsageStat | null;
  cost_usd: number | null;
  /** Parsed from provider-injected `\confidence{NN}` markers, if any. */
  confidence: number | null;
  /** Assistant batches that requested tool execution. */
  tool_calls: ToolCallInfo[] | null;
  /** Links a role="tool" record back to its call. */
  tool_call_id: string | null;
  /** Skills invoked by this user message (bodies resolved at send time). */
  skill_calls: string[] | null;
  /** Workflow gate active when this user message was sent ("plan"). */
  workflow: string | null;
  /** Attachments saved for this user message (file names under the
   *  session's attachments dir; resolved to data URIs on demand). */
  images?: string[];
}

/** One inline image attached to a user message (base64, pre-upload). */
export interface ChatImage {
  mime: string;
  b64: string;
}

export interface UsageStat {
  input: number | null;
  output: number | null;
  cached: number | null;
}

export interface RequestStat {
  seq: number;
  ts: number;
  lane: number;
  model: string;
  epoch: number;
  prefix_bytes: number;
  added_bytes: number;
  /** Local digest-chain continuity — false would mean a client-side rewrite. */
  chain_ok: boolean;
  input_tokens: number | null;
  cached_tokens: number | null;
  output_tokens: number | null;
  cost_usd: number | null;
}

export interface TelemetrySummary {
  requests: number;
  avg_hit_rate: number | null;
  steady_hit_rate: number | null;
  total_input: number;
  total_cached: number;
  total_output: number;
  total_cost: number;
  current_epoch: number;
  prefix_bytes: number;
}

export interface SessionTelemetry {
  session_id: string;
  requests: RequestStat[];
  epochs: number[];
  summary: TelemetrySummary;
  divergences: Divergence[];
}

export interface GlobalStats {
  sessions: number;
  requests: number;
  total_input: number;
  total_cached: number;
  total_output: number;
  total_cost: number;
}

export interface CompactionInfo {
  summary: string;
  upto_ts: number;
  created_at: number;
}

export interface SkillInfo {
  name: string;
  description: string;
  auto_inject: boolean;
  source: string;
  body: string;
}

/** One entry of a session task list (todo_write tool + 任务 panel). */
export interface TodoItem {
  text: string;
  status: "pending" | "in_progress" | "done";
}

/** A plugin entry from the SkillHub plugins registry (skillhub.cn/plugins). */
export interface MarketPlugin {  full_name: string;
  name: string;
  owner: string;
  description: string;
  avatar_url: string | null;
  category_key: string;
  stars: number | null;
  forks: number | null;
  license: string | null;
  installability: string;
  repository_url: string;
  default_branch: string;
  topics: string[];
}

export interface McpServer {
  id: string;
  name: string;
  /** Optional human note shown in the list and passed as context. */
  description?: string;
  transport: "stdio" | "http";
  command: string;
  args: string[];
  url: string;
  enabled: boolean;
  /** Trusted servers skip the per-call approval card. */
  trusted: boolean;
  allow_local: boolean;
  env: Record<string, string>;
  /** stdio only: working directory for the child process (empty = inherit). */
  cwd?: string;
  /** Per-server tool-call timeout in seconds (0/undefined = default 60). */
  timeout_secs?: number;
  /** http only: extra request headers (e.g. Authorization) per JSON-RPC POST. */
  headers?: Record<string, string>;
}

export interface McpStatusEntry {
  id: string;
  name: string;
  enabled: boolean;
  transport: string;
  trusted: boolean;
  state: string;
  tools: number;
}

export interface MarketSkill {
  slug: string;
  name: string;
  description: string;
  description_zh: string | null;
  icon_url: string | null;
  downloads: number | null;
  installs: number | null;
  stars: number | null;
  score: number | null;
  version: string | null;
  namespace: { handle?: string; publicSlug?: string } | null;
  source: string | null;
  verified: boolean | null;
}

// ---- streaming events (Channel<StreamEvent>) ----

export type StreamEvent =
  | { type: "started"; lane: number; model: string; message_id: string }
  | { type: "delta"; lane: number; message_id: string; text: string }
  | { type: "reasoning"; lane: number; message_id: string; text: string }
  | {
      type: "usage";
      lane: number;
      message_id: string;
      usage: UsageStat;
      request: RequestStat;
    }
  | { type: "done"; lane: number; message_id: string; status: MessageStatus; confidence?: number | null }
  | { type: "error"; lane: number; message: string }
  | { type: "tool_call"; lane: number; call_id: string; name: string; args: string }
  | { type: "tool_result"; lane: number; call_id: string; name: string; result: string }
  | {
      type: "sub_progress";
      lane: number;
      call_id: string;
      title: string;
      text: string;
      done: boolean;
    }
  | {
      type: "approval_request";
      lane: number;
      approval_id: string;
      tool: string;
      path: string;
      preview: string;
    };

export interface SendResult {
  ok: boolean;
}

export interface TestResult {
  ok: boolean;
  message: string;
  models: string[];
}

// ---- AuxMemo (whitelisted idempotent-call exact cache) ----

export type AuxOrigin = "l1" | "l2" | "miss";

export interface EnhanceOutcome {
  text: string;
  origin: AuxOrigin;
  model: string;
}

export interface AuxLedgerRow {
  ts: number;
  kind: string;
  model: string;
  origin: AuxOrigin;
  input_tokens: number | null;
  output_tokens: number | null;
  billed_usd: number | null;
  saved_usd: number | null;
}

export interface AuxKindStat {
  kind: string;
  calls: number;
  l1_hits: number;
  l2_hits: number;
  saved_usd: number;
  billed_usd: number;
}

export interface AuxStats {
  kinds: AuxKindStat[];
  /** newest first, capped at 50 */
  recent: AuxLedgerRow[];
}

// ---- Review panel (writes + tool calls) ----

export interface WriteLogEntry {
  ts: number;
  tool: string;
  path: string;
}

export interface WriteLog {
  ts: number;
  tool: string;
  path: string;
  before: string | null;
  after: string | null;
}

export type DiffLineType = "same" | "add" | "del";

export interface DiffLine {
  type: DiffLineType;
  text: string;
}

export interface DiffResult {
  lines: DiffLine[];
  addCount: number;
  delCount: number;
  truncated: boolean;
}

// ---- session import ----

export interface ImportCandidate {
  source: string;
  path: string;
  /** first user-message head — suggested title */
  title: string;
  messages: number;
  size_bytes: number;
}
