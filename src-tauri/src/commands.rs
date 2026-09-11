// Tauri command layer: the glue between the frontend and the engine.
// All session-file mutations go through one async lock so parallel arena
// lanes cannot interleave read-modify-write cycles.
use crate::chat::{self, SendCtx};
use crate::config::{self, AppConfig, Provider};
use crate::prefix::{message_json, ChatMessage, LanePrefix};
use crate::sessions::{now_ms, SessionStore};
use crate::types_rs::{
    GoalInfo, GoalState, MessageRecord, RequestStat, SessionBinding, SessionMeta, SessionTelemetry,
    StreamEvent, TelemetrySummary,
};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{Manager, State};
use uuid::Uuid;

/// Prefix state lives for the whole process — one window, one engine.
/// Key: (session_id, lane).
static PREFIXES: Mutex<Option<HashMap<(String, u32), LanePrefix>>> = Mutex::new(None);
static SEQ: AtomicU64 = AtomicU64::new(1);
static LAST_TS: AtomicU64 = AtomicU64::new(0);
/// Pending write-tool approvals: approval_id → resolver. Dropped senders
/// simply fail the await (deny, fail-closed).
static APPROVALS: Mutex<Option<HashMap<String, tokio::sync::oneshot::Sender<bool>>>> = Mutex::new(None);
/// Session-level grants: "session_id:tool" remembered via the approval card.
static GRANTS: Mutex<Option<std::collections::HashSet<String>>> = Mutex::new(None);
/// Last total sent bytes per lane for chain-continuity checks, with the
/// epoch it belonged to (an epoch change is a legitimate rebuild).
static LAST_SPAN: Mutex<Option<HashMap<(String, u32), (u32, usize)>>> = Mutex::new(None);
/// Auto-compaction threshold: compact at a user boundary once the last
/// request's input tokens reached this fraction of the context window.
const COMPACT_AT_FRACTION: f64 = 0.7;
/// Hysteresis target: free rungs (prune / elision) count as "rescued" only
/// when they push the projected input back BELOW this fraction — landing
/// between the two lines prevents immediate re-triggering (0.70 trigger →
/// 0.55 target leaves a 15% headroom band).
const COMPACT_TARGET_FRACTION: f64 = 0.55;
/// Don't bother compacting tiny conversations.
const COMPACT_MIN_MESSAGES: usize = 8;
/// Character budget fed to the summarizer.
const SUMMARIZE_INPUT_CAP: usize = 24_000;

/// Session tool-permission mode: "readonly" (write tools not offered),
/// "approve" (default — write tools gated by the approval card), "auto"
/// (write tools run without per-execution approval, still workspace-bound).
static PERMISSIONS: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

const VALID_MODES: &[&str] = &["readonly", "approve", "auto"];

/// Session workflow gate: "agent" (default — direct execution) | "plan"
/// (read-only research, then a frozen ```plan proposal that the user must
/// approve before any write can happen) | "goal" (only the goal + acceptance
/// criteria are locked; the agent picks its own path until all criteria pass,
/// with dynamic re-planning) | "deep" (Tree-of-Thoughts rehearsal: parallel
/// candidate approaches + judge, then the normal loop) | "review" (three
/// read-only expert pre-reviews + the model as Lead publishing a deduped,
/// graded findings table) | "sm:<def_id>:<state>" (declarative state machine,
/// see SM_STATE).
/// In-memory like PERMISSIONS, checkpointed onto SessionMeta.wf_gate so a
/// restart resumes the same mode instead of falling back to agent.
static WORKFLOW: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

/// Declarative state-machine position per session: session_id → gate string
/// "sm:<def_id>:<state_name>". Mirrored into WORKFLOW (the gate is the single
/// source of truth for mode checks; SM_STATE marks that the session is
/// actively running a state machine and remembers the resolved position for
/// auto-advance). In-memory like WORKFLOW — a restart drops back to agent.
static SM_STATE: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

/// Store an SM position and mirror it into the workflow gate.
fn sm_put(session_id: &str, gate: &str) {
    WORKFLOW
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(session_id.to_string(), gate.to_string());
    SM_STATE
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(session_id.to_string(), gate.to_string());
}

/// Checkpoint the workflow gate onto the session meta (best-effort — a
/// failed save leaves the in-memory gate in charge; it only matters across
/// restarts).
fn persist_gate(data_dir: &std::path::Path, session_id: &str, gate: Option<&str>) {
    let store = SessionStore::new(data_dir);
    if let Ok(mut sf) = store.load(session_id) {
        sf.meta.wf_gate = gate.map(|g| g.to_string());
        let _ = store.save(&sf);
    }
}

/// Resolve the active workflow gate. In-memory first; on a miss (fresh
/// process) backfill from the persisted checkpoint on SessionMeta so a
/// restart resumes the previous mode (断点续传). Falls back to "agent".
fn workflow_of_in(session_id: &str, data_dir: &std::path::Path) -> String {
    let cached = WORKFLOW
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|m| m.get(session_id))
        .cloned();
    let gate = match cached {
        Some(g) => Some(g),
        None => match SessionStore::new(data_dir)
            .load(session_id)
            .ok()
            .and_then(|sf| sf.meta.wf_gate.clone())
        {
            Some(g) => {
                WORKFLOW
                    .lock()
                    .unwrap()
                    .get_or_insert_with(HashMap::new)
                    .insert(session_id.to_string(), g.clone());
                Some(g)
            }
            None => None,
        },
    };
    match gate.as_deref() {
        Some("plan") => "plan".to_string(),
        Some("goal") => "goal".to_string(),
        Some("deep") => "deep".to_string(),
        Some("review") => "review".to_string(),
        Some("image") => "image".to_string(),
        Some(w) if w.starts_with("sm:") => w.to_string(),
        _ => "agent".to_string(),
    }
}

#[tauri::command]
pub fn set_workflow_mode(
    state: State<'_, AppState>,
    session_id: String,
    mode: String,
) -> Result<(), String> {
    if matches!(mode.as_str(), "agent" | "plan" | "goal" | "deep" | "review" | "image") {
        // leaving (or never entering) a state machine — clear the SM position
        if let Some(m) = SM_STATE.lock().unwrap().as_mut() {
            m.remove(&session_id);
        }
        WORKFLOW
            .lock()
            .unwrap()
            .get_or_insert_with(HashMap::new)
            .insert(session_id.clone(), mode.clone());
        // checkpoint: "agent" clears the persisted gate, everything else
        // persists so a restart resumes the same mode
        if mode == "agent" {
            persist_gate(&state.data_dir, &session_id, None);
        } else {
            persist_gate(&state.data_dir, &session_id, Some(&mode));
        }
        return Ok(());
    }
    if let Some(def_id) = mode.strip_prefix("sm:") {
        // declarative workflow: validate the def is real and enabled, then
        // initialize the session at its entry state (states[0])
        let cfg = config::load(&state.data_dir);
        let def = cfg
            .workflows
            .iter()
            .find(|d| d.id == def_id && d.enabled)
            .ok_or_else(|| format!("工作流不存在或未启用: sm:{def_id}"))?;
        let first = def
            .states
            .first()
            .ok_or_else(|| format!("工作流「{}」没有任何状态", def.name))?;
        let gate = format!("sm:{}:{}", def.id, first.name);
        sm_put(&session_id, &gate);
        persist_gate(&state.data_dir, &session_id, Some(&gate));
        return Ok(());
    }
    Err(format!("未知工作流模式: {mode}"))
}

#[tauri::command]
pub fn get_workflow_mode(state: State<'_, AppState>, session_id: String) -> String {
    workflow_of_in(&session_id, &state.data_dir)
}

// ---- goal lifecycle (Codex /goal parity) ----

/// Goal statuses the command surface may set. The model can only reach
/// achieved/unmet — and only through the update_goal tool (see
/// handle_goal_tool); pause/resume/clear are user-only.
const GOAL_STATUSES: &[&str] = &["active", "paused", "achieved", "unmet", "budget_limited"];

/// Parse the LAST ```goal checklist block in an assistant reply — extended
/// (better-harness "claimed vs exercised" grading): counts ✅ criteria whose
/// line carries NO inline evidence — no backtick
/// span (`path` / `cmd` / test name) and no （…） bracket note. A ✅ without
/// evidence is a *claim*, not a verified completion; surfaces use this to
/// warn instead of trusting the checkmark.
/// Returns (✅ count, total criteria (✅+⬜ lines), GOAL_DONE seen, claimed).
/// Lines without a ✅/⬜ marker (progress notes, replan remarks) are not
/// counted as criteria.
pub fn parse_goal_summary_ext(content: &str) -> (usize, usize, bool, usize) {
    let mut block: Option<&str> = None;
    let mut rest = content;
    while let Some(pos) = rest.find("```goal") {
        let after = &rest[pos + 7..];
        match after.find("```") {
            Some(end) => {
                block = Some(&after[..end]);
                rest = &after[end + 3..];
            }
            None => {
                block = Some(after);
                break;
            }
        }
    }
    let Some(body) = block else {
        return (0, 0, false, 0);
    };
    let done = body.contains("GOAL_DONE");
    let mut ok = 0usize;
    let mut total = 0usize;
    let mut claimed = 0usize;
    for line in body.lines() {
        let t = line.trim_start();
        if t.starts_with("✅") {
            ok += 1;
            total += 1;
            let has_backtick = t.contains('`');
            let has_bracket = match t.find('（') {
                Some(i) => t[i + '（'.len_utf8()..].contains('）'),
                None => false,
            };
            if !has_backtick && !has_bracket {
                claimed += 1;
            }
        } else if t.starts_with("⬜") {
            total += 1;
        }
    }
    (ok, total, done, claimed)
}

/// Create/replace the session goal and activate the goal gate. Shared by
/// the /goal command surface and the model's create_goal tool.
fn set_goal_inner(
    data_dir: &std::path::Path,
    session_id: &str,
    objective: String,
) -> Result<GoalState, String> {
    let store = SessionStore::new(data_dir);
    let mut sf = store.load(session_id)?;
    let now = now_ms();
    let g = GoalState {
        objective,
        status: "active".into(),
        created_at: now,
        updated_at: now,
    };
    sf.meta.goal = Some(g.clone());
    sf.meta.goal_rounds.clear();
    sf.meta.updated_at = now;
    store.save(&sf)?;
    // flip the workflow gate to goal (same map + persist as
    // set_workflow_mode) so the next turn runs under GOAL_DIRECTIVE
    if let Some(m) = SM_STATE.lock().unwrap().as_mut() {
        m.remove(session_id);
    }
    WORKFLOW
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(session_id.to_string(), "goal".to_string());
    persist_gate(data_dir, session_id, Some("goal"));
    Ok(g)
}

#[tauri::command]
pub fn goal_set(
    state: State<'_, AppState>,
    session_id: String,
    objective: String,
) -> Result<GoalState, String> {
    let objective = objective.trim().to_string();
    if objective.is_empty() {
        return Err("目标内容不能为空".into());
    }
    if objective.chars().count() > 4_000 {
        return Err("目标内容过长（上限 4000 字符）".into());
    }
    set_goal_inner(&state.data_dir, &session_id, objective)
}

/// Goal summary for the command surface and the summary bar: the persisted
/// goal record plus live transcript stats.
#[tauri::command]
pub fn goal_get(state: State<'_, AppState>, session_id: String) -> Result<GoalInfo, String> {
    let store = SessionStore::new(&state.data_dir);
    let sf = store.load(&session_id)?;
    let goal = sf.meta.goal.clone();
    let mut cost = 0f64;
    let mut has_cost = false;
    let mut last_assistant = String::new();
    for m in &sf.messages {
        if m.role == "assistant" {
            if let Some(c) = m.cost_usd {
                cost += c;
                has_cost = true;
            }
            last_assistant = m.content.clone();
        }
    }
    let (checklist_done, checklist_total, checklist_all_met, checklist_claimed) =
        parse_goal_summary_ext(&last_assistant);
    Ok(GoalInfo {
        goal,
        cost_usd: if has_cost { Some(cost) } else { None },
        checklist_done,
        checklist_total,
        checklist_all_met,
        checklist_claimed,
        goal_rounds: sf.meta.goal_rounds.clone(),
    })
}

/// User/runtime-side status transition (pause/resume/soft-stop/achieved
/// bookkeeping). Any whitelist status is allowed here — the restriction to
/// achieved/unmet applies only to the model's update_goal tool.
#[tauri::command]
pub fn goal_status(
    state: State<'_, AppState>,
    session_id: String,
    status: String,
) -> Result<GoalState, String> {
    if !GOAL_STATUSES.contains(&status.as_str()) {
        return Err(format!("未知目标状态: {status}"));
    }
    let store = SessionStore::new(&state.data_dir);
    let mut sf = store.load(&session_id)?;
    let mut g = sf.meta.goal.clone().ok_or("该会话还没有目标")?;
    g.status = status;
    g.updated_at = now_ms();
    sf.meta.goal = Some(g.clone());
    sf.meta.updated_at = now_ms();
    store.save(&sf)?;
    Ok(g)
}

/// Remove the goal, returning the previous record (for the toast).
#[tauri::command]
pub fn goal_clear(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<Option<GoalState>, String> {
    let store = SessionStore::new(&state.data_dir);
    let mut sf = store.load(&session_id)?;
    let prev = sf.meta.goal.take();
    sf.meta.goal_rounds.clear();
    sf.meta.updated_at = now_ms();
    store.save(&sf)?;
    Ok(prev)
}

/// Model-facing goal tools (Codex /goal parity): get_goal (read),
/// create_goal (create + flip the gate), update_goal (declare achieved /
/// unmet only). Pausing, resuming and clearing are NOT reachable from the
/// model — the same safety boundary Codex draws.
fn handle_goal_tool(data_dir: &std::path::Path, session_id: &str, name: &str, args: &Value) -> String {
    let store = SessionStore::new(data_dir);
    match name {
        "get_goal" => {
            let Ok(sf) = store.load(session_id) else {
                return "ERROR: 会话不存在".into();
            };
            let Some(g) = sf.meta.goal else {
                return "NONE: 当前会话尚未创建目标（可用 create_goal 创建）".into();
            };
            let last = sf
                .messages
                .iter()
                .rev()
                .find(|m| m.role == "assistant")
                .map(|m| m.content.clone())
                .unwrap_or_default();
            let (ok, total, all_met, claimed) = parse_goal_summary_ext(&last);
            let info = serde_json::json!({
                "objective": g.objective,
                "status": g.status,
                "checklist_done": ok,
                "checklist_total": total,
                "checklist_all_met": all_met,
                "checklist_claimed_no_evidence": claimed,
                "hint": if claimed > 0 { format!("{claimed} 条 ✅ 行内缺少反引号或（…）证据标注——按完成审计规则补上可核验证据，否则降回 ⬜") } else { String::new() },
            });
            format!("OK:{info}")
        }
        "create_goal" => {
            let obj = args
                .get("objective")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if obj.is_empty() {
                return "ERROR: objective 参数不能为空".into();
            }
            if obj.chars().count() > 4_000 {
                return "ERROR: objective 超过 4000 字符上限".into();
            }
            match set_goal_inner(data_dir, session_id, obj) {
                Ok(_) => "OK: 目标已创建并进入目标模式。请按目标模式规则推进：每轮回复末尾输出 ```goal 验收清单（✅/⬜ + 证据），全部 ✅ 时输出 GOAL_DONE。".into(),
                Err(e) => format!("ERROR: {e}"),
            }
        }
        "update_goal" => {
            let st = args.get("status").and_then(|v| v.as_str()).unwrap_or("");
            if st != "achieved" && st != "unmet" {
                return "ERROR: update_goal 仅允许 status=\"achieved\" 或 \"unmet\"（暂停/恢复/清除只能由用户操作）".into();
            }
            let Ok(mut sf) = store.load(session_id) else {
                return "ERROR: 会话不存在".into();
            };
            let Some(g) = sf.meta.goal.as_mut() else {
                return "NONE: 当前会话尚未创建目标".into();
            };
            if g.status == "achieved" || g.status == "unmet" {
                return format!("ERROR: 目标已是终态（{}），不能再次变更", g.status);
            }
            g.status = st.to_string();
            g.updated_at = now_ms();
            sf.meta.updated_at = now_ms();
            if store.save(&sf).is_err() {
                return "ERROR: 目标状态保存失败".into();
            }
            format!("OK: 目标状态已更新为 {st}")
        }
        _ => "ERROR: 未知目标工具".into(),
    }
}

/// Current state-machine gate for a session: "sm:<def_id>:<state>" or "".
#[tauri::command]
pub fn sm_get(session_id: String) -> String {
    SM_STATE
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|m| m.get(&session_id))
        .cloned()
        .unwrap_or_default()
}

/// Manual state jump (progress-bar chips): validates the target state exists
/// in the definition, then moves the session there. The next turn runs under
/// the new state's directive and tool surface.
#[tauri::command]
pub fn sm_set(
    state: State<'_, AppState>,
    session_id: String,
    def_id: String,
    state_name: String,
) -> Result<(), String> {
    let cfg = config::load(&state.data_dir);
    let def = cfg
        .workflows
        .iter()
        .find(|d| d.id == def_id)
        .ok_or_else(|| format!("工作流不存在: {def_id}"))?;
    if !def.states.iter().any(|s| s.name == state_name) {
        return Err(format!("工作流「{}」没有状态「{state_name}」", def.name));
    }
    let gate = format!("sm:{def_id}:{state_name}");
    sm_put(&session_id, &gate);
    persist_gate(&state.data_dir, &session_id, Some(&gate));
    Ok(())
}

fn permission_of(session_id: &str) -> &'static str {
    let m = PERMISSIONS.lock().unwrap().as_ref().and_then(|m| m.get(session_id)).cloned();
    match m.as_deref() {
        Some("readonly") => "readonly",
        Some("auto") => "auto",
        _ => "approve",
    }
}

#[tauri::command]
pub fn set_permission_mode(session_id: String, mode: String) -> Result<(), String> {
    if !VALID_MODES.contains(&mode.as_str()) {
        return Err(format!("未知权限模式: {mode}"));
    }
    PERMISSIONS
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(session_id, mode);
    Ok(())
}

/// Guard against runaway tool loops; each round is one provider request.
const MAX_TOOL_ROUNDS: usize = 8;
/// Goal mode locks the result, not the path — allow a longer loop per turn.
const GOAL_MAX_TOOL_ROUNDS: usize = 24;
/// Sub-agent delegations allowed per parent turn (cost bound).
const MAX_DELEGATIONS_PER_TURN: usize = 3;
/// Per-side file snapshot cap for the review panel's write log (chars).
const WRITE_LOG_CAP: usize = 64_000;
/// Approval timeout — fail-closed like every other permission surface here.
const APPROVAL_TIMEOUT_SECS: u64 = 120;

fn grant_key(session_id: &str, tool: &str) -> String {
    format!("{session_id}:{tool}")
}

/// Register a pending approval and return its receiver.
fn open_approval(id: &str) -> tokio::sync::oneshot::Receiver<bool> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    APPROVALS
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(id.to_string(), tx);
    rx
}

fn take_approval(id: &str) -> Option<tokio::sync::oneshot::Sender<bool>> {
    APPROVALS.lock().unwrap().as_mut()?.remove(id)
}

/// Monotonic per-record timestamp: equal-ms records keep their order.
fn next_record_ts() -> u64 {
    loop {
        let now = now_ms();
        let prev = LAST_TS.load(Ordering::Relaxed);
        let next = now.max(prev + 1);
        if LAST_TS
            .compare_exchange_weak(prev, next, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return next;
        }
    }
}

fn prefixes_lock() -> std::sync::MutexGuard<'static, Option<HashMap<(String, u32), LanePrefix>>> {
    PREFIXES.lock().unwrap()
}

/// Zone H empty (only the frozen system prompt, if any) — used to detect a
/// fresh process that must rebuild its prefix from the persisted transcript.
fn lp_is_empty(lp: &LanePrefix) -> bool {
    lp.prefix_bytes_public() <= 0 || lp.history_len_public() == 0
}

pub struct AppState {
    pub data_dir: PathBuf,
    pub store: SessionStore,
    pub client: reqwest::Client,
    /// Cancellation flags per session.
    pub stops: Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// Serializes session-file read-modify-write across lanes.
    pub save_lock: Arc<tokio::sync::Mutex<()>>,
}

// ---------- task-list panel (todo_write tool) ----------

/// One entry of a session's task list. Status is one of
/// "pending" | "in_progress" | "done" (normalized on write).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TodoItem {
    pub text: String,
    pub status: String,
}

impl AppState {
    pub fn get_todos(&self, session_id: &str) -> Vec<TodoItem> {
        load_todos(&self.data_dir, session_id)
    }
}

fn todos_path(data_dir: &std::path::Path, session_id: &str) -> PathBuf {
    data_dir.join("todos").join(format!("{session_id}.json"))
}

fn load_todos(data_dir: &std::path::Path, session_id: &str) -> Vec<TodoItem> {
    std::fs::read_to_string(todos_path(data_dir, session_id))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_todos(data_dir: &std::path::Path, session_id: &str, todos: &[TodoItem]) {
    let path = todos_path(data_dir, session_id);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(todos) {
        let _ = std::fs::write(&path, json);
    }
}

/// Task list of one session, shown in the preview panel's 任务 tab.
#[tauri::command]
pub fn get_todos(state: State<'_, AppState>, session_id: String) -> Vec<TodoItem> {
    state.get_todos(&session_id)
}

impl AppState {
    pub fn new(data_dir: PathBuf) -> Self {
        // privacy mapping log lives in the data dir (no-op until privacy mode
        // actually scrubs something)
        crate::privacy::init_log(&data_dir);
        // spill root for the read_file allowlist (oversized tool outputs)
        crate::spill::init_root(&data_dir);
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .connect_timeout(std::time::Duration::from_secs(20))
            .build()
            .expect("http client");
        let store = SessionStore::new(&data_dir);
        // mirror the sandbox policy into the tool guard before any turn runs
        let boot_settings = config::load(&data_dir).settings;
        sync_sandbox_policy(&boot_settings);
        crate::privacy::set_custom_patterns(boot_settings.privacy_custom_patterns.clone());
        // The request sequence must survive restarts: seed the global counter
        // from the highest seq already recorded in any session file, or old
        // and new records collide on the same numbers.
        let mut max_seq = 0u64;
        for meta in store.list() {
            if let Ok(sf) = store.load(&meta.id) {
                for r in &sf.telemetry {
                    if r.seq > max_seq {
                        max_seq = r.seq;
                    }
                }
            }
        }
        SEQ.store(max_seq + 1, Ordering::Relaxed);
        Self {
            store,
            data_dir,
            client,
            stops: Mutex::new(HashMap::new()),
            save_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
}

#[derive(Serialize)]
pub struct TestResult {
    pub ok: bool,
    pub message: String,
    pub models: Vec<String>,
}

#[derive(Serialize)]
pub struct SendResult {
    pub ok: bool,
}

#[derive(Serialize)]
pub struct GlobalStats {
    pub sessions: u64,
    pub requests: u64,
    pub total_input: u64,
    pub total_cached: u64,
    pub total_output: u64,
    pub total_cost: f64,
}

// ---------- config ----------

#[tauri::command]
pub fn get_config(state: State<'_, AppState>) -> AppConfig {
    config::load(&state.data_dir)
}

#[tauri::command]
pub fn save_config(state: State<'_, AppState>, config: AppConfig) -> Result<(), String> {
    for p in &config.providers {
        // empty base_url = "preset added, endpoint not yet filled in" —
        // legitimate state; skip the SSRF check until a URL exists
        if p.base_url.trim().is_empty() {
            continue;
        }
        if let crate::urlguard::UrlCheck::Refused(msg) =
            crate::urlguard::check_base_url(&p.base_url, p.allow_local)
        {
            return Err(format!("{}: {msg}", p.name));
        }
    }
    config::save(&state.data_dir, &config);
    sync_sandbox_policy(&config.settings);
    crate::privacy::set_custom_patterns(config.settings.privacy_custom_patterns.clone());
    Ok(())
}

/// Mirror the sandbox settings into the agent-tools guard (startup +
/// every save).
fn sync_sandbox_policy(s: &config::AppSettings) {
    crate::agent_tools::set_sandbox_policy(crate::agent_tools::SandboxPolicy {
        on: s.sandbox_mode,
        files: s.sandbox_files,
        commands: s.sandbox_commands,
        network: s.sandbox_network,
        file_deny: s.sandbox_file_deny.clone(),
        file_allow: s.sandbox_file_allow.clone(),
        cmd_deny: s.sandbox_cmd_deny.clone(),
        cmd_allow: s.sandbox_cmd_allow.clone(),
        cmd_ask: s.sandbox_cmd_ask.clone(),
        net_deny: s.sandbox_net_deny.clone(),
        net_allow: s.sandbox_net_allow.clone(),
        net_block_all: s.sandbox_net_block_all,
        net_malicious: s.sandbox_net_malicious,
    });
}

/// Recursively restore surrogates inside a tool-call argument JSON so the
/// tool operates on REAL values while the model only ever saw surrogates.
fn restore_args(session_id: &str, seed: &[u8; 32], v: &mut Value) {
    match v {
        Value::String(s) => {
            let restored = crate::privacy::restore(session_id, s);
            if &restored != s {
                *s = restored;
            }
        }
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                restore_args(session_id, seed, item);
            }
        }
        Value::Object(map) => {
            for (_k, val) in map.iter_mut() {
                restore_args(session_id, seed, val);
            }
        }
        _ => {}
    }
}

/// Sandbox auto-backup: copy the file about to be modified into
/// <data_dir>/backups/<session_id>/<ts>-<name>, then trim the whole backups
/// dir down to the cap (oldest first). Best-effort — a backup failure never
/// blocks the write itself.
fn backup_snapshot(
    data_dir: &std::path::Path,
    session_id: &str,
    file: &std::path::Path,
    cap_mb: u64,
) -> Result<(), String> {
    let dir = data_dir.join("backups").join(session_id);
    fs::create_dir_all(&dir).map_err(|e| format!("backup dir: {e}"))?;
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S%.3f");
    let name = file
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file.bin");
    let dest = dir.join(format!("{ts}-{name}"));
    fs::copy(file, &dest).map_err(|e| format!("backup copy: {e}"))?;
    // trim: while the backups tree exceeds the cap, delete its oldest file
    let cap = cap_mb.saturating_mul(1024 * 1024);
    let mut files: Vec<(std::time::SystemTime, PathBuf, u64)> = vec![];
    fn walk(dir: &std::path::Path, out: &mut Vec<(std::time::SystemTime, PathBuf, u64)>) {
        if let Ok(rd) = fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if let Ok(md) = e.metadata() {
                    out.push((md.modified().unwrap_or(std::time::UNIX_EPOCH), p, md.len()));
                }
            }
        }
    }
    walk(&data_dir.join("backups"), &mut files);
    let total: u64 = files.iter().map(|(_, _, l)| l).sum();
    if total > cap {
        files.sort_by_key(|(t, _, _)| *t);
        let mut acc = total;
        for (_, p, l) in files {
            if acc <= cap {
                break;
            }
            if fs::remove_file(&p).is_ok() {
                acc = acc.saturating_sub(l);
            }
        }
    }
    Ok(())
}

/// Open the sandbox backup directory in the OS file manager.
#[tauri::command]
pub fn open_backup_dir(state: State<'_, AppState>) -> Result<(), String> {
    let dir = state.data_dir.join("backups");
    fs::create_dir_all(&dir).map_err(|e| format!("创建备份目录失败: {e}"))?;
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer")
            .arg(&dir)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(&dir)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(&dir)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ---------- privacy mapping log（伪匿名化映射日志）----------

/// Tail of the privacy mapping log: what got scrubbed, when, into what.
/// Returns at most `n` (default 200) NEWEST entries, newest first.
#[tauri::command]
pub fn privacy_log_tail(
    state: State<'_, AppState>,
    n: Option<u32>,
) -> Vec<crate::privacy::LogEntry> {
    let take = n.unwrap_or(200).clamp(1, 2000) as usize;
    let path = state.data_dir.join("privacy_log.jsonl");
    let Ok(raw) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    raw.lines()
        .rev()
        .take(take)
        .filter_map(|line| serde_json::from_str::<crate::privacy::LogEntry>(line).ok())
        .collect()
}

/// Clear the privacy mapping log (the in-memory vaults of live sessions are
/// untouched — the log is a view, not the mapping itself).
#[tauri::command]
pub fn privacy_log_clear(state: State<'_, AppState>) -> Result<(), String> {
    let path = state.data_dir.join("privacy_log.jsonl");
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// One-click import from cc-switch (https://github.com/farion1231/cc-switch).
/// Reads the provider table from ~/.cc-switch/cc-switch.db and maps entries
/// into our Provider model:
///   - app_type "claude": settings_config.env.ANTHROPIC_BASE_URL / _AUTH_TOKEN
///     → anthropic provider
///   - app_type "codex":  settings_config.auth.OPENAI_API_KEY + TOML config
///     text (model = "...", base_url = "...") → openai_compatible provider
/// Skips entries without a key and anything whose (base_url, api_key) pair
/// already exists in the config. Appends the rest and returns what was added.
#[tauri::command]
pub fn ccswitch_import(state: State<'_, AppState>) -> Result<Vec<Provider>, String> {
    let db_path = home_dir()
        .join(".cc-switch")
        .join("cc-switch.db");
    if !db_path.exists() {
        return Err("未找到 cc-switch 数据库（~/.cc-switch/cc-switch.db）—— 请确认已安装 cc-switch".into());
    }
    let db = rusqlite::Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(|e| format!("打开 cc-switch 数据库失败: {e}"))?;

    let mut stmt = db
        .prepare(
            "SELECT name, settings_config FROM providers
             WHERE app_type IN ('claude', 'codex') ORDER BY app_type, sort_index",
        )
        .map_err(|e| format!("读取 providers 表失败: {e}"))?;

    let mut config = config::load(&state.data_dir);
    let mut imported: Vec<Provider> = Vec::new();
    let mut rows: Vec<(String, String)> = Vec::new();
    let iterate = stmt
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .map_err(|e| format!("查询 providers 失败: {e}"))?;
    for row in iterate {
        let (name, cfg_json) = row.map_err(|e| format!("读取行失败: {e}"))?;
        rows.push((name, cfg_json));
    }
    drop(stmt);
    drop(db);

    for (name, cfg_json) in rows {
        let cfg: serde_json::Value = match serde_json::from_str(&cfg_json) {
            Ok(v) => v,
            Err(_) => continue, // malformed entry — skip
        };
        let env = cfg.get("env");
        let (kind, base_url, api_key, models) = if let Some(env) = env {
            // claude-style: {"env": {"ANTHROPIC_BASE_URL": ..., "ANTHROPIC_AUTH_TOKEN": ...}}
            let url = env
                .get("ANTHROPIC_BASE_URL")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .trim_end_matches('/')
                .to_string();
            let key = env
                .get("ANTHROPIC_AUTH_TOKEN")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            (crate::config::ProviderKind::Anthropic, url, key, Vec::new())
        } else {
            // codex-style: {"auth": {"OPENAI_API_KEY": ...}, "config": "<toml>"}
            let key = cfg
                .pointer("/auth/OPENAI_API_KEY")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let toml_text = cfg.get("config").and_then(|v| v.as_str()).unwrap_or("");
            let mut model = String::new();
            let mut url = String::new();
            for line in toml_text.lines() {
                let line = line.trim();
                if model.is_empty() && line.starts_with("model =") {
                    model = unquote_toml_value(line.trim_start_matches("model ="));
                } else if url.is_empty() && line.starts_with("base_url =") {
                    url = unquote_toml_value(line.trim_start_matches("base_url ="));
                }
            }
            let url = url.trim_end_matches('/').to_string();
            let models = if model.is_empty() { Vec::new() } else { vec![model] };
            (crate::config::ProviderKind::OpenaiCompatible, url, key, models)
        };

        if api_key.is_empty() {
            continue; // official placeholder entries carry no key — skip
        }
        let base_url = if base_url.is_empty() {
            // key without endpoint still imports; user fills the URL later
            String::new()
        } else {
            base_url
        };
        // dedupe against existing providers and this batch
        let dup = config
            .providers
            .iter()
            .chain(imported.iter())
            .any(|p| p.api_key == api_key && p.base_url == base_url);
        if dup {
            continue;
        }

        imported.push(Provider {
            id: format!("p_{}", uuid::Uuid::new_v4().simple()),
            name,
            kind,
            base_url,
            api_key,
            models,
            enabled: true,
            allow_local: false,
            context_window: None,
            pricing: std::collections::BTreeMap::new(),
            behavior: std::collections::BTreeMap::new(),
            cache_tier: None,
            cache_retention_24h: None,
            images_via_files: false,
        });
    }

    if imported.is_empty() {
        return Ok(imported);
    }
    config.providers.extend(imported.iter().cloned());
    // reuse the save_config SSRF guard semantics via config::save directly
    for p in &config.providers {
        if p.base_url.trim().is_empty() {
            continue;
        }
        if let crate::urlguard::UrlCheck::Refused(msg) =
            crate::urlguard::check_base_url(&p.base_url, p.allow_local)
        {
            return Err(format!("{}: {msg}", p.name));
        }
    }
    config::save(&state.data_dir, &config);
    Ok(imported)
}

/// `"...value..."` → `value` (tolerates trailing commas / whitespace).
fn unquote_toml_value(raw: &str) -> String {
    raw.trim()
        .trim_end_matches(',')
        .trim()
        .trim_matches('"')
        .to_string()
}

fn home_dir() -> std::path::PathBuf {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
}

#[tauri::command]
pub async fn test_provider(provider: Provider) -> TestResult {
    // same SSRF guard as save_config: these commands hit an arbitrary URL,
    // so loopback/private endpoints need the explicit allow_local consent
    if let crate::urlguard::UrlCheck::Refused(msg) =
        crate::urlguard::check_base_url(&provider.base_url, provider.allow_local)
    {
        return TestResult { ok: false, message: msg, models: vec![] };
    }
    match chat::fetch_models_async(&reqwest::Client::new(), &provider).await {
        Ok(models) => TestResult {
            ok: true,
            message: format!("连接成功，{} 个模型可用", models.len()),
            models: models.clone(),
        },
        Err(e) => TestResult { ok: false, message: e, models: vec![] },
    }
}

#[tauri::command]
pub async fn fetch_models(provider: Provider) -> Result<Vec<String>, String> {
    if let crate::urlguard::UrlCheck::Refused(msg) =
        crate::urlguard::check_base_url(&provider.base_url, provider.allow_local)
    {
        return Err(msg);
    }
    chat::fetch_models_async(&reqwest::Client::new(), &provider).await
}

// ---------- sessions ----------

#[tauri::command]
pub fn list_sessions(state: State<'_, AppState>) -> Vec<SessionMeta> {
    state.store.list()
}

#[tauri::command]
pub fn create_session(
    state: State<'_, AppState>,
    kind: String,
    bindings: Vec<SessionBinding>,
    title: String,
) -> Result<SessionMeta, String> {
    let sf = state.store.create(&kind, bindings, &title)?;
    Ok(sf.meta)
}

#[tauri::command]
pub fn delete_session(state: State<'_, AppState>, session_id: String) -> Result<(), String> {
    if let Some(map) = prefixes_lock().as_mut() {
        map.retain(|(sid, _), _| sid != &session_id);
    }
    state.store.delete(&session_id)
}

#[tauri::command]
pub fn rename_session(state: State<'_, AppState>, session_id: String, title: String) -> Result<SessionMeta, String> {
    state.store.rename(&session_id, &title)
}

#[tauri::command]
pub fn update_bindings(
    state: State<'_, AppState>,
    session_id: String,
    bindings: Vec<SessionBinding>,
) -> Result<SessionMeta, String> {
    state.store.set_bindings(&session_id, bindings)
}

#[tauri::command]
pub fn set_workspace(
    state: State<'_, AppState>,
    session_id: String,
    workspace: Option<String>,
) -> Result<SessionMeta, String> {
    // validate the folder exists before binding
    if let Some(ws) = &workspace {
        if !ws.trim().is_empty() && !std::path::Path::new(ws.trim()).is_dir() {
            return Err(format!("目录不存在: {ws}"));
        }
    }
    state.store.set_workspace(&session_id, workspace)
}

#[tauri::command]
pub fn set_session_pinned(state: State<'_, AppState>, session_id: String, pinned: bool) -> Result<SessionMeta, String> {
    state.store.set_pinned(&session_id, pinned)
}

#[tauri::command]
pub fn set_session_archived(state: State<'_, AppState>, session_id: String, archived: bool) -> Result<SessionMeta, String> {
    state.store.set_archived(&session_id, archived)
}

/// Fork a chat session at the clicked user message (inclusive).
#[tauri::command]
pub fn branch_session(
    state: State<'_, AppState>,
    session_id: String,
    from_ts: u64,
) -> Result<SessionMeta, String> {
    state.store.branch(&session_id, from_ts).map(|sf| sf.meta)
}

// ---------- @-file references ----------

/// Workspace-relative file paths for the composer's @ completion menu.
#[tauri::command]
pub fn search_workspace_files(workspace: String, query: String) -> Result<Vec<String>, String> {
    if !std::path::Path::new(&workspace).is_dir() {
        return Err("工作区不存在".into());
    }
    Ok(crate::agent_tools::search_files(&workspace, &query))
}

/// Read one workspace text file for @-reference expansion (same path guard
/// and 256KB cap as the read_file tool).
#[tauri::command]
pub fn read_workspace_file(workspace: String, path: String) -> Result<String, String> {
    if !std::path::Path::new(&workspace).is_dir() {
        return Err("工作区不存在".into());
    }
    crate::agent_tools::read_file_public(&workspace, &path)
}

// ---------- preview panel ----------

/// One entry of a workspace directory listing (structured, for the file
/// explorer in the preview panel).
#[derive(serde::Serialize)]
pub struct WorkspaceEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

/// List one directory of the workspace. Hidden/skip-listed entries
/// (.git, node_modules, target, …) are filtered; dirs first, both groups
/// case-insensitively sorted. Path is workspace-relative and guarded by
/// the same resolve_in_workspace check as the agent tools.
#[tauri::command]
pub fn list_workspace_dir(workspace: String, path: String) -> Result<Vec<WorkspaceEntry>, String> {
    if !std::path::Path::new(&workspace).is_dir() {
        return Err("工作区不存在".into());
    }
    let dir = crate::agent_tools::resolve_in_workspace(&workspace, &path)?;
    let entries = std::fs::read_dir(&dir).map_err(|e| format!("无法读取目录: {e}"))?;
    let mut out: Vec<WorkspaceEntry> = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || crate::agent_tools::is_skip_dir(&name) {
            continue;
        }
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let size = if is_dir { 0 } else { e.metadata().map(|m| m.len()).unwrap_or(0) };
        out.push(WorkspaceEntry { name, is_dir, size });
    }
    out.sort_by(|a, b| {
        b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(out)
}

/// Open an http/https URL in the system browser (preview panel "open
/// externally"). Scheme is whitelisted — no file/other handlers.
#[tauri::command]
pub fn open_external(url: String) -> Result<(), String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("仅支持 http/https 链接".into());
    }
    tauri_plugin_opener::open_url(url, None::<&str>).map_err(|e| e.to_string())
}

// ---------- session import ----------

/// Scan a source's default transcript location (source: "claude-code" |
/// "codex" | "opencode").
#[tauri::command]
pub fn import_scan(source: String) -> Vec<crate::importer::ImportCandidate> {
    crate::importer::scan(&source)
}

/// Import one transcript file as a regular chat session. `source` selects
/// the adapter ("claude-code" or "generic" for custom paths). Messages get
/// timestamps derived from the file's mtime; model binding is left empty —
/// pick one in the composer after importing.
#[tauri::command]
pub fn import_session(
    state: State<'_, AppState>,
    source: String,
    path: String,
    title: Option<String>,
) -> Result<SessionMeta, String> {
    if !std::path::Path::new(&path).is_file() {
        return Err(format!("文件不存在: {path}"));
    }
    let msgs = crate::importer::parse_file(&source, &path)?;
    if msgs.is_empty() {
        return Err("未解析出可导入的 user/assistant 消息".into());
    }
    let suggested: String = msgs
        .iter()
        .find(|m| m.role == "user")
        .map(|m| m.content.chars().take(24).collect())
        .unwrap_or_default();
    let title = title
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or(suggested);
    let sf = state.store.create("chat", Vec::new(), &title)?;
    let base: u64 = std::fs::metadata(&path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or_else(now_ms);
    let records: Vec<MessageRecord> = msgs
        .iter()
        .enumerate()
        .map(|(i, m)| MessageRecord {
            id: Uuid::new_v4().to_string(),
            lane: 0,
            role: m.role.clone(),
            content: m.content.clone(),
            reasoning: None,
            ts: base + i as u64,
            model: if m.role == "assistant" { Some("imported".into()) } else { None },
            status: "ok".into(),
            usage: None,
            cost_usd: None,
            confidence: None,
            tool_calls: None,
            tool_call_id: None,
            skill_calls: None,
            workflow: None,
            images: Vec::new(),
        })
        .collect();
    let mut out = state.store.load(&sf.meta.id)?;
    out.messages = records;
    out.meta.updated_at = now_ms();
    state.store.save(&out)?;
    Ok(out.meta)
}

#[tauri::command]
pub fn get_session_messages(state: State<'_, AppState>, session_id: String) -> Result<Vec<MessageRecord>, String> {
    Ok(state.store.load(&session_id)?.messages)
}

/// Return one stored attachment as a data URI so the frontend can render
/// image bubbles without enabling Tauri's asset protocol.
#[tauri::command]
pub fn attachment_data(
    state: State<'_, AppState>,
    session_id: String,
    filename: String,
) -> Result<String, String> {
    crate::sessions::attachment_data_uri(&state.data_dir, &session_id, &filename)
}

// ---------- telemetry ----------

#[tauri::command]
pub fn get_telemetry(state: State<'_, AppState>, session_id: String) -> Result<SessionTelemetry, String> {
    let sf = state.store.load(&session_id)?;
    let mut requests = sf.telemetry.clone();
    // chronological order; seq is the secondary key (legacy files may hold
    // duplicate seqs from before the restart-safe counter)
    requests.sort_by_key(|r| (r.ts, r.seq));

    let mut epochs: Vec<u64> = Vec::new();
    let mut total_input = 0u64;
    let mut total_cached = 0u64;
    let mut total_output = 0u64;
    let mut total_cost = 0f64;
    let mut hit_rates: Vec<f64> = Vec::new();
    let mut steady_rates: Vec<f64> = Vec::new();
    let mut rebilled_tokens = 0u64;
    let mut significant_misses = 0u64;
    let mut rebilled_cost = 0f64;
    let mut has_rebilled_cost = false;
    let mut prev_epoch: Option<u32> = None;

    for r in &requests {
        if prev_epoch != Some(r.epoch) {
            epochs.push(r.ts);
        }
        let is_first_of_epoch = prev_epoch != Some(r.epoch);
        prev_epoch = Some(r.epoch);

        if let (Some(c), Some(i)) = (r.cached_tokens, r.input_tokens) {
            total_input += i;
            total_cached += c;
            if i > 0 {
                let rate = c as f64 / i as f64 * 100.0;
                hit_rates.push(rate);
                // the first request of every epoch is an expected rebuild
                if !is_first_of_epoch {
                    steady_rates.push(rate);
                }
            }
        }
        total_output += r.output_tokens.unwrap_or(0);
        total_cost += r.cost_usd.unwrap_or(0.0);
        if r.significant_miss {
            significant_misses += 1;
            rebilled_tokens += r.rebilled_tokens;
            if let Some(c) = r.rebilled_cost {
                rebilled_cost += c;
                has_rebilled_cost = true;
            }
        }
    }

    let avg = |v: &[f64]| if v.is_empty() { None } else { Some(v.iter().sum::<f64>() / v.len() as f64) };
    let current_epoch = requests.last().map(|r| r.epoch).unwrap_or(0);
    let prefix_bytes = requests.last().map(|r| r.prefix_bytes + r.added_bytes).unwrap_or(0);

    Ok(SessionTelemetry {
        session_id,
        epochs,
        divergences: crate::divergence::classify(&requests),
        requests,
        summary: TelemetrySummary {
            requests: sf.telemetry.len() as u64,
            avg_hit_rate: avg(&hit_rates),
            steady_hit_rate: avg(&steady_rates),
            total_input,
            total_cached,
            total_output,
            total_cost: (total_cost * 10000.0).round() / 10000.0,
            current_epoch,
            prefix_bytes,
            rebilled_tokens,
            rebilled_cost: if has_rebilled_cost {
                Some((rebilled_cost * 10000.0).round() / 10000.0)
            } else {
                None
            },
            significant_misses,
        },
    })
}

#[tauri::command]
pub fn get_global_stats(state: State<'_, AppState>) -> GlobalStats {
    let mut g = GlobalStats { sessions: 0, requests: 0, total_input: 0, total_cached: 0, total_output: 0, total_cost: 0.0 };
    for meta in state.store.list() {
        if let Ok(sf) = state.store.load(&meta.id) {
            g.sessions += 1;
            g.requests += sf.telemetry.len() as u64;
            for r in &sf.telemetry {
                g.total_input += r.input_tokens.unwrap_or(0);
                g.total_cached += r.cached_tokens.unwrap_or(0);
                g.total_output += r.output_tokens.unwrap_or(0);
                g.total_cost += r.cost_usd.unwrap_or(0.0);
            }
        }
    }
    g.total_cost = (g.total_cost * 10000.0).round() / 10000.0;
    g
}

#[tauri::command]
pub fn export_session(state: State<'_, AppState>, session_id: String) -> Result<String, String> {
    let sf = state.store.load(&session_id)?;
    let dir = state.data_dir.join("exports");
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let safe: String = sf
        .meta
        .title
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let path = dir.join(format!("{}-{}.md", safe, chrono::Local::now().format("%Y%m%d-%H%M%S")));
    fs::write(&path, crate::sessions::export_markdown(&sf)).map_err(|e| e.to_string())?;
    Ok(path.display().to_string())
}

#[tauri::command]
pub fn get_app_data_dir(state: State<'_, AppState>) -> String {
    state.data_dir.display().to_string()
}

#[tauri::command]
pub fn open_data_dir(state: State<'_, AppState>) -> Result<(), String> {
    tauri_plugin_opener::open_path(state.data_dir.display().to_string(), None::<&str>)
        .map_err(|e| e.to_string())
}

// ---------- custom window frame ----------
// Window actions live behind app commands (not the JS window API) so the
// buttons work regardless of capability grants; state *reading* (is_maximized
// + resize events) uses core:default via the drag-region/`onResized` path.

fn main_window(app: &tauri::AppHandle) -> Option<tauri::WebviewWindow> {
    app.get_webview_window("main")
}

#[tauri::command]
pub fn window_minimize(app: tauri::AppHandle) {
    if let Some(w) = main_window(&app) {
        let _ = w.minimize();
    }
}

#[tauri::command]
pub fn window_toggle_maximize(app: tauri::AppHandle) -> bool {
    if let Some(w) = main_window(&app) {
        if w.is_maximized().unwrap_or(false) {
            let _ = w.unmaximize();
        } else {
            let _ = w.maximize();
        }
        return w.is_maximized().unwrap_or(false);
    }
    false
}

#[tauri::command]
pub fn window_close(app: tauri::AppHandle) {
    if let Some(w) = main_window(&app) {
        let _ = w.close();
    }
}

// ---------- close-to-tray ----------

/// Set once the user confirmed a real exit (dialog / tray menu). The
/// CloseRequested handler lets the window close only when this is set.
static FORCE_QUIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn is_force_quit() -> bool {
    FORCE_QUIT.load(std::sync::atomic::Ordering::Relaxed)
}

/// Mark exit-as-confirmed, then close the window for real.
pub fn request_quit(app: &tauri::AppHandle) {
    FORCE_QUIT.store(true, std::sync::atomic::Ordering::Relaxed);
    if let Some(w) = main_window(app) {
        let _ = w.close();
    }
}

/// Hide the window instead of closing — the tray icon keeps the app alive.
#[tauri::command]
pub fn hide_to_tray(app: tauri::AppHandle) {
    if let Some(w) = main_window(&app) {
        let _ = w.hide();
    }
}

/// Dialog "退出" / tray-menu "退出" — bypasses the close interception.
#[tauri::command]
pub fn app_quit(app: tauri::AppHandle) {
    request_quit(&app);
}

// ---------- sending ----------

/// Build the text fed to the summarizer: previous summary (if any) plus a
/// bounded excerpt of the transcript.
fn summarize_input(
    sf: &crate::sessions::SessionFile,
    keep_from_ts: u64,
    memo: Option<&str>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(m) = memo {
        parts.push(format!("[当前会话备忘]\n{m}"));
    }
    if let Some(prev) = &sf.compaction {
        parts.push(format!("[上次摘要]\n{}", prev.summary));
    }
    let upto = sf.compaction.as_ref().map(|c| c.upto_ts).unwrap_or(0);
    for m in &sf.messages {
        // fold range: after the previous boundary, strictly before KEEP
        if m.ts < upto || m.ts >= keep_from_ts || m.status == "error" {
            continue;
        }
        let role = match m.role.as_str() {
            "user" => "用户",
            "assistant" => "助手",
            "tool" => "工具",
            _ => continue,
        };
        let head: String = m.content.chars().take(1200).collect();
        parts.push(format!("{role}: {head}"));
    }
    let mut text = parts.join("\n\n");
    if text.chars().count() > SUMMARIZE_INPUT_CAP {
        text = text.chars().take(SUMMARIZE_INPUT_CAP).collect::<String>() + "\n…[已截断]";
    }
    text
}

/// Structured-summary prompt (L6 §5.2): the output shape mirrors
/// RollingMemo so the summary can be re-consumed by the same machinery,
/// and `CompactionRecord` needs no format migration.
const COMPACT_PROMPT: &str = "你是对话摘要器。把输入的历史压缩为信息密集的 YAML 会话备忘（全部内容合计不超过 1200 字符），字段固定：goal（当前目标，一行）、decisions（已确定的决定，每条一行）、files（涉及文件路径及其最后动作）、errors_fixed（已修复的错误）、open_items（未尽事项）。必须保留具体文件路径与数值。只输出 YAML 本身，不要代码块围栏。";

/// KEEP-bucket boundary (L6 §5.1): walk user turns newest-first and keep
/// including whole turns while the accumulated bytes fit the budget;
/// always keep at least the newest turn. Returns the ts such that every
/// record with ts ≥ it belongs to KEEP (byte accounting matches the
/// request estimator: content bytes + 96 overhead).
fn compute_keep_from_ts(messages: &[MessageRecord], keep_budget_bytes: usize) -> u64 {
    let starts: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == "user")
        .map(|(i, _)| i)
        .collect();
    if starts.is_empty() {
        return messages.first().map(|m| m.ts).unwrap_or(0);
    }
    let mut acc = 0usize;
    let mut kept = 0usize;
    let mut boundary = messages.last().map(|m| m.ts).unwrap_or(0);
    for n in (0..starts.len()).rev() {
        let start = starts[n];
        let end = starts.get(n + 1).copied().unwrap_or(messages.len());
        let span: usize = messages[start..end].iter().map(|m| m.content.len() + 96).sum();
        if kept > 0 && acc + span > keep_budget_bytes {
            break;
        }
        acc += span;
        kept += 1;
        boundary = messages[start].ts;
    }
    boundary
}

/// Stable hash of the tool-schema loadout sent in the request head. Fed to
/// LanePrefix::bind_tools_hash so MCP join/leave mid-session is honest about
/// the epoch rebuild it causes (tools serialize before messages).
fn tools_hash(tools: Option<&Value>) -> Option<u64> {
    tools.map(|t| {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        t.to_string().hash(&mut h);
        h.finish()
    })
}

/// Run boundary compaction for a chat session: summarize everything up to
/// now, persist the record, invalidate the lane prefix (next request
/// rebuilds from the compacted transcript — an expected epoch bump).
async fn compact_now(
    client: &reqwest::Client,
    data_dir: &std::path::Path,
    session_id: &str,
    provider: &Provider,
    model: &str,
    trigger: &str,
) -> Result<(), String> {
    let store = SessionStore::new(data_dir);
    let sf0 = store.load(session_id)?;
    if sf0.messages.len() < COMPACT_MIN_MESSAGES {
        return Err(format!("对话过短（{} 条），无需压缩", sf0.messages.len()));
    }
    let window = provider.context_window.unwrap_or(131_072) as f64;
    // KEEP bucket (L6 §5.1): recent user turns within window × 20%, always
    // at least the newest turn — records from keep_from_ts on stay verbatim.
    let keep_budget = (window * 0.20) as usize * 4; // tokens → bytes (÷4 convention)
    let keep_from_ts = compute_keep_from_ts(&sf0.messages, keep_budget);
    // DROP rung before folding (idempotent): stale oversized outputs →
    // stubs, so the summarizer reads a smaller input
    let cfg = config::load(data_dir);
    let (dropped, stubs) =
        elide_stale_tool_records(data_dir, session_id, cfg.settings.spill_max_chars, keep_from_ts);
    let sf = store.load(session_id)?;
    let prev_upto = sf.compaction.as_ref().map(|c| c.upto_ts).unwrap_or(0);
    let foldable: Vec<&MessageRecord> = sf
        .messages
        .iter()
        .filter(|m| m.ts >= prev_upto && m.ts < keep_from_ts)
        .filter(|m| matches!(m.role.as_str(), "user" | "assistant" | "tool"))
        .collect();
    // nothing to fold → the elision rung was the whole job
    if foldable.is_empty() {
        return Ok(());
    }
    let memo_render = sf.meta.rolling_memo.as_ref().and_then(render_memo);
    let memo_chars = memo_render.as_ref().map(|t| t.chars().count()).unwrap_or(0);
    let input = summarize_input(&sf, keep_from_ts, memo_render.as_deref());
    let outcome = chat::complete_once(client, provider, model, COMPACT_PROMPT, &input).await?;
    let summary: String = outcome.text.trim().chars().take(1600).collect();
    let record = crate::types_rs::CompactionRecord {
        summary,
        upto_ts: keep_from_ts,
        created_at: now_ms(),
    };
    // third ledger (L6 §6.1): what fired, what it folded, what it saved
    let folded_tokens =
        (foldable.iter().map(|m| m.content.len() + 96).sum::<usize>() as u64) / 4;
    let completed_turns = sf.messages.iter().filter(|m| m.role == "user").count() as u64;
    let epoch_before = prefixes_lock()
        .as_ref()
        .and_then(|m| m.get(&(session_id.to_string(), 0)))
        .map(|lp| lp.epoch)
        .unwrap_or(0);
    let stat = crate::types_rs::CompactionStat {
        ts: now_ms(),
        trigger: trigger.to_string(),
        folded_tokens,
        dropped_tokens: (dropped as u64) / 4,
        stubs,
        summary_tokens: 800,
        memo_chars,
        payback_turns: provider
            .pricing
            .get(model)
            .and_then(|pr| payback_turns(folded_tokens, 800, pr)),
        completed_turns,
        epoch_before,
    };
    let mut sf = store.load(session_id)?;
    sf.compaction = Some(record);
    sf.meta.compactions.push(stat);
    if sf.meta.compactions.len() > 60 {
        let overflow = sf.meta.compactions.len() - 60;
        sf.meta.compactions.drain(..overflow);
    }
    sf.meta.updated_at = now_ms();
    store.save(&sf)?;
    if let Some(map) = prefixes_lock().as_mut() {
        map.remove(&(session_id.to_string(), 0));
    }
    if let Some(map) = LAST_SPAN.lock().unwrap().as_mut() {
        map.remove(&(session_id.to_string(), 0));
    }
    Ok(())
}

#[tauri::command]
pub async fn compact_session(state: State<'_, AppState>, session_id: String) -> Result<String, String> {
    let cfg = config::load(&state.data_dir);
    let sf = state.store.load(&session_id)?;
    if sf.meta.kind != "chat" {
        return Err("竞技场会话暂不支持压缩".into());
    }
    let binding = sf.meta.bindings.first().cloned().ok_or("会话未绑定模型")?;
    let provider = resolve_provider(&cfg, &binding).cloned().ok_or("Provider 未配置")?;
    compact_now(&state.client, &state.data_dir, &session_id, &provider, &binding.model, "manual").await?;
    Ok(sf.compaction.as_ref().map(|_| "已在旧摘要基础上再次压缩".to_string()).unwrap_or_else(|| "已压缩".into()))
}

#[tauri::command]
pub fn get_session_compaction(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<Option<crate::types_rs::CompactionRecord>, String> {
    Ok(state.store.load(&session_id)?.compaction)
}

/// Pre-compaction break-even estimate (pi pruning economics): the summary
/// re-bills at the full input price once (the cache-write premium), while
/// every later turn saves re-reading the folded tokens from cache. Sync —
/// no model call, numbers come from byte counts + configured pricing.
#[tauri::command]
pub fn compact_estimate(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<crate::types_rs::CompactEstimate, String> {
    let cfg = config::load(&state.data_dir);
    let sf = state.store.load(&session_id)?;
    if sf.meta.kind != "chat" {
        return Err("竞技场会话暂不支持压缩".into());
    }
    let binding = sf.meta.bindings.first().cloned().ok_or("会话未绑定模型")?;
    let provider = resolve_provider(&cfg, &binding).cloned().ok_or("Provider 未配置")?;
    let model = binding.model.clone();

    // folded = what compaction replaces: every record that would go over the
    // wire now (content bytes + per-message overhead, same units as the tail
    // estimator in chat.rs; ÷4 = mixed-script token approximation)
    let folded_bytes: usize = sf
        .messages
        .iter()
        .filter(|m| m.role == "user" || m.role == "assistant" || m.role == "tool")
        .map(|m| m.content.len() + 96)
        .sum();
    let folded_tokens = (folded_bytes as u64) / 4;
    let summary_tokens: u64 = 800; // 600-char summary cap ≈ 600–900 CJK tokens

    let pricing = provider.pricing.get(&model);
    let (rewrite_cost_usd, save_per_turn_usd, payback) = match pricing {
        Some(pr) => {
            let spread = (pr.input_per_m - pr.cached_per_m).max(0.0);
            let rewrite = summary_tokens as f64 / 1e6 * spread;
            let save = folded_tokens as f64 / 1e6 * pr.cached_per_m;
            (
                Some((rewrite * 10000.0).round() / 10000.0),
                Some((save * 10000.0).round() / 10000.0),
                payback_turns(folded_tokens, summary_tokens, pr),
            )
        }
        None => (None, None, None),
    };
    Ok(crate::types_rs::CompactEstimate {
        folded_tokens,
        summary_tokens,
        rewrite_cost_usd,
        save_per_turn_usd,
        payback_turns: payback,
    })
}

// ---------- Skills ----------

#[tauri::command]
pub fn get_skills(workspace: Option<String>) -> Vec<crate::skills::SkillInfo> {
    crate::skills::scan(workspace.as_deref())
}

/// Delete an installed skill file. Only files inside the global skills dir
/// are deletable from the app; project skills are managed in the workspace.
#[tauri::command]
pub fn delete_skill(name: String) -> Result<(), String> {
    let safe: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    if safe.is_empty() || safe != name {
        return Err(format!("非法技能名: {name}"));
    }
    let dir = crate::skillhub::install_dir()?;
    let path = dir.join(format!("{safe}.md"));
    if !path.exists() {
        return Err(format!("技能 {name} 不存在于全局目录"));
    }
    std::fs::remove_file(&path).map_err(|e| format!("删除失败: {e}"))
}

/// Wipe a session's messages, telemetry and compaction (visible history
/// included — this is destructive and gated by a frontend confirm dialog).
#[tauri::command]
pub fn clear_session(state: State<'_, AppState>, session_id: String) -> Result<usize, String> {
    let mut sf = state.store.load(&session_id)?;
    let removed = sf.messages.len();
    sf.messages.clear();
    sf.telemetry.clear();
    sf.compaction = None;
    sf.meta.updated_at = now_ms();
    state.store.save(&sf)?;
    if let Some(map) = prefixes_lock().as_mut() {
        map.remove(&(session_id.clone(), 0));
    }
    if let Some(map) = LAST_SPAN.lock().unwrap().as_mut() {
        map.remove(&(session_id, 0));
    }
    Ok(removed)
}

// ---------- SkillHub market ----------

#[derive(Serialize)]
pub struct MarketPage {
    pub skills: Vec<crate::config::MarketSkill>,
    pub total: u64,
}

#[tauri::command]
pub async fn skillhub_list(
    state: State<'_, AppState>,
    page: u32,
    page_size: u32,
    sort_by: String,
    keyword: String,
) -> Result<MarketPage, String> {
    let page_size = page_size.clamp(1, 50);
    let (skills, total) = crate::skillhub::fetch_market(&state.client, page.max(1), page_size, &sort_by, &keyword).await?;
    Ok(MarketPage { skills, total })
}

#[tauri::command]
pub async fn skillhub_install(
    state: State<'_, AppState>,
    slug: String,
    namespace: String,
    version: Option<String>,
    description: String,
) -> Result<String, String> {
    let (path, files) = crate::skillhub::install_skill(
        &state.client,
        &slug,
        &namespace,
        version.as_deref(),
        &description,
    )
    .await?;
    Ok(format!("已安装到 {path}（包内 {files} 个文件，仅启用 SKILL.md 提示层）"))
}

#[tauri::command]
pub async fn skillhub_plugins(
    state: State<'_, AppState>,
    page: u32,
    page_size: u32,
    category: String,
) -> Result<crate::skillhub::PluginPage, String> {
    let page_size = page_size.clamp(1, 50);
    crate::skillhub::fetch_plugins(&state.client, page.max(1), page_size, &category).await
}

/// Install a SkillHub plugin: pull its GitHub archive and extract every
/// SKILL.md as a global skill (prompt layer only).
#[tauri::command]
pub async fn skillhub_plugin_install(
    state: State<'_, AppState>,
    owner: String,
    name: String,
    default_branch: String,
    description: String,
) -> Result<String, String> {
    let (installed, files) = crate::skillhub::install_plugin(
        &state.client,
        &owner,
        &name,
        &default_branch,
        &description,
    )
    .await?;
    let list = installed.iter().map(|s| format!("/{s}")).collect::<Vec<_>>().join(" ");
    Ok(format!(
        "已安装 {} 个技能 {list}（包内 {files} 个文件，仅启用提示层）",
        installed.len()
    ))
}

// ---------- MCP ----------

#[tauri::command]
pub fn mcp_status(state: State<'_, AppState>) -> Vec<Value> {
    let cfg = config::load(&state.data_dir);
    crate::mcp::global().status(&cfg.mcp_servers)
}

/// Connect (or reconnect) one server: spawn + handshake + tools/list.
#[tauri::command]
pub async fn mcp_test(
    _state: State<'_, AppState>,
    server: crate::config::McpServerConfig,
) -> Result<TestResult, String> {
    crate::mcp::global().drop_server(&server.id);
    match crate::mcp::global().ensure(&server).await {
        Ok(n) => Ok(TestResult {
            ok: true,
            message: format!("连接成功，发现 {n} 个工具"),
            models: Vec::new(),
        }),
        Err(e) => Ok(TestResult { ok: false, message: e, models: Vec::new() }),
    }
}

/// Cached tool schemas of all enabled, connected servers (spawned-task safe).
fn state_mcp_tools(data_dir: &std::path::Path) -> Vec<Value> {
    let cfg = config::load(data_dir);
    let enabled: Vec<crate::config::McpServerConfig> =
        cfg.mcp_servers.iter().filter(|s| s.enabled).cloned().collect();
    crate::mcp::global().cached_tools(&enabled)
}

/// Connect every enabled server once (best-effort) so the first send does
/// not pay the handshake latency. Called from run_send before lanes start.
async fn ensure_mcp_servers(data_dir: &std::path::Path, stop: &AtomicBool) {
    let cfg = config::load(data_dir);
    for s in cfg.mcp_servers.iter().filter(|s| s.enabled) {
        // each dead server can burn up to ~2×15s of handshake timeout before
        // the lanes spawn — a stop click must cut the walk short
        if stop.load(Ordering::Relaxed) {
            return;
        }
        if let Err(e) = crate::mcp::global().ensure(s).await {
            eprintln!("[mcp] {} 连接失败: {e}", s.name);
        }
    }
}

/// Compaction ladder L1 (free pruning): rewrite every tool record exceeding
/// `max_chars` through the spill trim — full output to the spill dir, bounded
/// head/tail + locator into the record. Persisted once; deterministic and
/// idempotent (marker guard), so Zone H replay and restart rebuilds stay
/// byte-identical. Returns the recovered character count (0 = nothing pruned
/// or the save failed).
/// Best-effort target extraction from a tool call's arguments JSON: the
/// first present string among common path/url/command keys (capped). Empty
/// when the tool has no meaningful target — the record then never elides.
fn extract_tool_target(arguments: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return String::new();
    };
    for key in ["path", "file_path", "url", "target", "command"] {
        if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
            return s.chars().take(120).collect();
        }
    }
    String::new()
}

/// Render budget for the memo block (L6 §4.1).
const MEMO_CAP_CHARS: usize = 1200;

/// User-correction heuristics for the decisions bucket (§4.2). Deliberately
/// narrow: only file/number facts are recorded, never guessed semantics.
const MEMO_CORRECTION_KEYS: [&str; 6] = ["不对", "改成", "还是用", "换成", "不是这个", "回退"];

/// Rule-based memo update from one finished turn (L6 §4.2): file targets
/// (last action wins), user corrections, and error→fix pairs within the
/// turn. Pure function over the wire messages; returns whether anything
/// changed. Idempotent on replay — the same turn applied twice is a no-op.
fn rolling_memo_apply(memo: &mut crate::types_rs::RollingMemo, turn: &[ChatMessage]) -> bool {
    let mut changed = false;
    if let Some(user) = turn.iter().find(|m| m.role == "user") {
        let text = user.content.trim();
        if text.chars().count() > 4 && MEMO_CORRECTION_KEYS.iter().any(|k| text.contains(k)) {
            let excerpt: String = text.chars().take(80).collect();
            if !memo.decisions.iter().any(|d| d == &excerpt) {
                memo.decisions.push(excerpt);
                if memo.decisions.len() > 12 {
                    memo.decisions.remove(0);
                }
                changed = true;
            }
        }
    }
    // collect (target, tool, errored) in call order, joining results by id
    let mut seq: Vec<(String, String, bool)> = Vec::new();
    for m in turn.iter() {
        let Some(calls) = m.tool_calls.as_ref().and_then(|v| v.as_array()) else {
            continue;
        };
        for call in calls {
            let id = call.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let fname =
                call.get("function").and_then(|f| f.get("name")).and_then(|v| v.as_str()).unwrap_or("");
            let args = call
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if id.is_empty() || fname.is_empty() {
                continue;
            }
            let Some(result) = turn
                .iter()
                .find(|r| r.role == "tool" && r.tool_call_id.as_deref() == Some(id))
            else {
                continue;
            };
            let errored = result.content.starts_with("ERROR");
            seq.push((extract_tool_target(args), fname.to_string(), errored));
        }
    }
    // last action per target wins
    for (target, fname, errored) in &seq {
        if target.is_empty() {
            continue;
        }
        let entry = format!("{fname}{}", if *errored { "（失败）" } else { "" });
        if memo.files.get(target).map(|v| v.as_str()) != Some(entry.as_str()) {
            memo.files.insert(target.clone(), entry);
            changed = true;
        }
    }
    // error→fix: last result for a target is ok, an earlier one errored
    let mut counted: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for i in (0..seq.len()).rev() {
        let (target, fname, errored) = &seq[i];
        if *errored || target.is_empty() || !counted.insert(target.clone()) {
            continue;
        }
        if seq[..i].iter().any(|(t2, _, e2)| t2 == target && *e2) {
            let item = format!("{target}（{fname}）");
            if !memo.errors_fixed.iter().any(|x| x == &item) {
                memo.errors_fixed.push(item);
                if memo.errors_fixed.len() > 8 {
                    memo.errors_fixed.remove(0);
                }
                changed = true;
            }
        }
    }
    changed
}

/// Deterministic memo render (L6 §4.1): fixed section order, BTreeMap file
/// ordering, hard char cap with a truncation marker. None when nothing has
/// been extracted yet.
fn render_memo(memo: &crate::types_rs::RollingMemo) -> Option<String> {
    let has_goal = memo.goal.as_deref().is_some_and(|g| !g.trim().is_empty());
    if !has_goal
        && memo.decisions.is_empty()
        && memo.files.is_empty()
        && memo.errors_fixed.is_empty()
        && memo.open_items.is_empty()
    {
        return None;
    }
    let mut out = String::from("[会话备忘·自动维护]\n");
    if let Some(goal) = memo.goal.as_deref().filter(|g| !g.trim().is_empty()) {
        out.push_str(&format!("goal: {goal}\n"));
    }
    let sections: Vec<(&str, Vec<String>)> = vec![
        ("decisions", memo.decisions.clone()),
        (
            "files",
            memo.files.iter().map(|(k, v)| format!("{k} ← {v}")).collect(),
        ),
        ("errors_fixed", memo.errors_fixed.clone()),
        ("open_items", memo.open_items.clone()),
    ];
    for (title, items) in sections {
        if items.is_empty() {
            continue;
        }
        let mut block = format!("{title}:\n");
        for it in items {
            let line = format!("- {it}\n");
            if out.chars().count() + block.chars().count() + line.chars().count() > MEMO_CAP_CHARS
            {
                block.push_str("- …[截断]\n");
                break;
            }
            block.push_str(&line);
        }
        out.push_str(&block);
    }
    Some(out)
}

fn prune_oversized_tool_records(
    data_dir: &std::path::Path,
    session_id: &str,
    max_chars: usize,
) -> usize {
    if max_chars == 0 {
        return 0;
    }
    let store = SessionStore::new(data_dir);
    let Ok(mut sf) = store.load(session_id) else {
        return 0;
    };
    let mut saved = 0usize;
    let mut changed = false;
    for m in sf.messages.iter_mut().filter(|m| m.role == "tool") {
        if m.content.chars().count() <= max_chars {
            continue;
        }
        let trimmed = crate::spill::maybe_spill_in(
            &data_dir.join("spills"),
            session_id,
            "tool",
            &m.content,
            max_chars,
        );
        let before = m.content.chars().count();
        let after = trimmed.chars().count();
        if after < before {
            saved += before - after;
            m.content = trimmed;
            changed = true;
        }
    }
    if !changed {
        return 0;
    }
    sf.meta.updated_at = now_ms();
    if store.save(&sf).is_err() {
        return 0;
    }
    // expected rebuild: the next request re-creates the prefix from the
    // smaller transcript instead of judging the byte change an upstream miss
    if let Some(map) = prefixes_lock().as_mut() {
        map.remove(&(session_id.to_string(), 0));
    }
    if let Some(map) = LAST_SPAN.lock().unwrap().as_mut() {
        map.remove(&(session_id.to_string(), 0));
    }
    saved
}

/// Free rung 2 (L6 §3): downgrade stale oversized tool outputs to one-line
/// stubs. A tool record is elidable when it is big, older than the recent
/// window (`keep_from_ts`), and SUPERSEDED — a later tool record for the
/// same (tool, target) exists, so the stub loses nothing current. Same
/// materialized-rewrite pattern as `prune_oversized_tool_records`: the
/// persisted record is rewritten once, the live prefix is dropped, and
/// restart rebuilds read the rewritten bytes. The stub keeps role=tool +
/// tool_call_id, so call/result pairing survives (design invariant I1).
/// Returns (saved chars, stubs written); (0, 0) = nothing changed.
fn elide_stale_tool_records(
    data_dir: &std::path::Path,
    session_id: &str,
    max_chars: usize,
    keep_from_ts: u64,
) -> (usize, u32) {
    if max_chars == 0 {
        return (0, 0);
    }
    let store = SessionStore::new(data_dir);
    let Ok(mut sf) = store.load(session_id) else {
        return (0, 0);
    };
    let min_chars = (max_chars / 2).max(1);
    // tool_call_id → (tool name, target) from the assistant batches
    let mut call_meta: HashMap<String, (String, String)> = HashMap::new();
    for m in &sf.messages {
        if let Some(calls) = &m.tool_calls {
            for c in calls {
                let target = extract_tool_target(&c.arguments);
                if !target.is_empty() {
                    call_meta.insert(c.id.clone(), (c.name.clone(), target));
                }
            }
        }
    }
    // newest ts per (tool, target): the latest read/write stays verbatim
    let mut latest: HashMap<(String, String), u64> = HashMap::new();
    for m in sf.messages.iter().filter(|m| m.role == "tool") {
        if let Some(id) = &m.tool_call_id {
            if let Some((name, target)) = call_meta.get(id) {
                let entry = latest.entry((name.clone(), target.clone())).or_insert(0);
                if m.ts > *entry {
                    *entry = m.ts;
                }
            }
        }
    }
    let mut saved = 0usize;
    let mut stubs = 0u32;
    let mut changed = false;
    for m in sf.messages.iter_mut() {
        if m.role != "tool" || m.ts >= keep_from_ts {
            continue;
        }
        if m.content.chars().count() <= min_chars {
            continue;
        }
        let Some(id) = &m.tool_call_id else { continue };
        let Some((name, target)) = call_meta.get(id) else { continue };
        match latest.get(&(name.clone(), target.clone())) {
            Some(latest_ts) if *latest_ts > m.ts => {}
            _ => continue,
        }
        let stub = format!(
            "[已降级] {name} {target} @ts={}。原输出 {} 字符已过时；如需当前内容请重新读取。",
            m.ts,
            m.content.chars().count()
        );
        let before = m.content.chars().count();
        let after = stub.chars().count();
        if after < before {
            saved += before - after;
            stubs += 1;
            m.content = stub;
            changed = true;
        }
    }
    if !changed {
        return (0, 0);
    }
    sf.meta.updated_at = now_ms();
    if store.save(&sf).is_err() {
        return (0, 0);
    }
    if let Some(map) = prefixes_lock().as_mut() {
        map.remove(&(session_id.to_string(), 0));
    }
    if let Some(map) = LAST_SPAN.lock().unwrap().as_mut() {
        map.remove(&(session_id.to_string(), 0));
    }
    (saved, stubs)
}

/// Payback math shared by the manual estimate UI and the automatic gate
/// (single source so the two never drift): how many turns of cached-read
/// savings it takes to amortize one summary rewrite. None = economics
/// undefined (no savings because cached reads are free, or no spread).
fn payback_turns(
    folded_tokens: u64,
    summary_tokens: u64,
    pr: &crate::config::Pricing,
) -> Option<u64> {
    let spread = (pr.input_per_m - pr.cached_per_m).max(0.0);
    let rewrite = summary_tokens as f64 / 1e6 * spread;
    let save = folded_tokens as f64 / 1e6 * pr.cached_per_m;
    if save > 0.0 && rewrite > 0.0 {
        Some((rewrite / save).ceil() as u64)
    } else {
        None
    }
}

/// Automatic-compaction payback gate (L6 §2): a summary is only worth its
/// rewrite cost when the session is expected to live long enough to
/// amortize it. Remaining life heuristic: max(8, half of the completed
/// user turns) — fresh sessions are assumed short-lived, long sessions
/// have proven longevity. No pricing data ⇒ pressure-only (gate open);
/// cached reads free ⇒ folding saves nothing (gate closed).
fn auto_compact_payback_ok(
    pricing: Option<&crate::config::Pricing>,
    folded_tokens: u64,
    summary_tokens: u64,
    completed_turns: u64,
) -> bool {
    let Some(pr) = pricing else { return true };
    let save = folded_tokens as f64 / 1e6 * pr.cached_per_m;
    if save <= 0.0 {
        return false;
    }
    match payback_turns(folded_tokens, summary_tokens, pr) {
        // spread = 0 ⇒ the rewrite itself is free, folding always wins
        None => true,
        Some(payback) => payback <= (completed_turns / 2).max(8),
    }
}

/// Auto boundary compaction: called at the user boundary inside run_send.
async fn maybe_auto_compact(
    state: &State<'_, AppState>,
    session_id: &str,
    cfg: &AppConfig,
    stop: &AtomicBool,
) {
    let Ok(sf) = state.store.load(session_id) else { return };
    if sf.meta.kind != "chat" || sf.messages.len() < COMPACT_MIN_MESSAGES {
        return;
    }
    let binding = match sf.meta.bindings.first() {
        Some(b) => b.clone(),
        None => return,
    };
    let Some(provider) = resolve_provider(cfg, &binding) else { return };
    let window = provider.context_window.unwrap_or(131_072) as f64;
    let last_input = sf
        .telemetry
        .iter()
        .filter_map(|r| r.input_tokens)
        .next_back()
        .unwrap_or(0) as f64;
    let completed_turns = sf.messages.iter().filter(|m| m.role == "user").count() as u64;
    // thrash guard (L6 §6.2): while boosted, this session's trigger line
    // sits at 80% so rapid re-compaction can't loop
    let trigger_line = if sf.meta.compact_boost_until_turn > completed_turns {
        0.80
    } else {
        COMPACT_AT_FRACTION
    };
    if last_input < window * trigger_line {
        return;
    }
    // the summary call is a full one-shot completion (up to 120s) — never
    // start it for a turn the user already stopped
    if stop.load(Ordering::Relaxed) {
        return;
    }
    // Compaction ladder L1 — free retro-prune: recover oversized tool
    // results without any model call. When that alone projects the request
    // back under the compaction line, skip the summary entirely; otherwise
    // fall through and let the summarizer read the now-smaller input.
    let saved = prune_oversized_tool_records(&state.data_dir, session_id, cfg.settings.spill_max_chars);
    // free rung 2 (L6 §3): superseded stale tool outputs → stubs. The
    // recent window is everything from the last user turn onward.
    let recent_from = sf
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| m.ts)
        .unwrap_or(0);
    let (elided, _stubs) =
        elide_stale_tool_records(&state.data_dir, session_id, cfg.settings.spill_max_chars, recent_from);
    let saved = saved + elided;
    if saved > 0 {
        let projected = last_input - (saved as f64 / 4.0);
        // hysteresis: free rungs count as "rescued" only below the TARGET
        // line, not merely back under the trigger — otherwise the session
        // sits a hair under 0.70 and re-triggers every turn
        if projected < window * COMPACT_TARGET_FRACTION {
            return;
        }
    }
    // payback gate (L6 §2): only summarize when the session's expected
    // remaining life amortizes the rewrite cost. No pricing data ⇒ the
    // gate stays open (pressure-only, previous behavior).
    let folded_tokens = {
        let bytes: usize = sf
            .messages
            .iter()
            .filter(|m| m.role == "user" || m.role == "assistant" || m.role == "tool")
            .map(|m| m.content.len() + 96)
            .sum();
        (bytes as u64) / 4
    };
    if !auto_compact_payback_ok(
        provider.pricing.get(&binding.model),
        folded_tokens,
        800,
        completed_turns,
    ) {
        return;
    }
    // thrash engage (L6 §6.2): this is the ≥2nd auto compaction within the
    // last 10 user turns → raise the trigger line for 30 turns and surface
    // a one-time notice alongside the compaction
    let recent_auto = sf
        .meta
        .compactions
        .iter()
        .filter(|c| c.trigger == "auto" && c.completed_turns + 10 > completed_turns)
        .count();
    if recent_auto >= 1 {
        let _guard = state.save_lock.lock().await;
        if let Ok(mut s) = state.store.load(session_id) {
            s.meta.compact_boost_until_turn = completed_turns + 30;
            s.messages.push(MessageRecord {
                id: uuid::Uuid::new_v4().to_string(),
                lane: 0,
                role: "notice".into(),
                content: format!(
                    "缓存压缩抖动：近 10 轮内第 {} 次自动压缩。上下文增长过快——建议检查是否有循环读取大文件，或调高设置中的溢出阈值。已将本会话压缩触发线临时上调至 80%，30 轮后自动回落。",
                    recent_auto + 1
                ),
                reasoning: None,
                ts: next_record_ts(),
                model: None,
                status: "ok".into(),
                usage: None,
                cost_usd: None,
                confidence: None,
                tool_calls: None,
                tool_call_id: None,
                skill_calls: None,
                workflow: None,
                images: Vec::new(),
            });
            s.meta.updated_at = now_ms();
            let _ = state.store.save(&s);
        }
    }
    let _ =
        compact_now(&state.client, &state.data_dir, session_id, provider, &binding.model, "auto").await;
}

#[tauri::command]
pub fn stop_generation(state: State<'_, AppState>, session_id: String) {
    if let Some(flag) = state.stops.lock().unwrap().get(&session_id) {
        flag.store(true, Ordering::Relaxed);
    }
}

/// Resolves as soon as the session's stop flag flips. Paired with
/// `tokio::select!` around the pre-stream phases (MCP warm-up, auto
/// compaction, memory recall, deep/review rehearsals) so a stop click takes
/// effect immediately instead of being ignored until the first streamed
/// token — previously those phases left the turn looking frozen with a dead
/// stop button.
async fn wait_stopped(stop: &AtomicBool) {
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    }
}

/// Emit the turn-closing event for a lane aborted before any streamed
/// content (stop during a pre-stream phase). No assistant record exists yet
/// — nothing was generated — so the transcript keeps only the user message.
/// The digest-span marker is dropped (mirroring the error path): the aborted
/// turn's bytes never went upstream, so the next turn's totals legitimately
/// shrink and must not be judged against the stale span.
async fn abort_lane_pre_stream(
    channel: &tauri::ipc::Channel<StreamEvent>,
    session_id: &str,
    lane: u32,
    message_id: &str,
) {
    if let Some(map) = LAST_SPAN.lock().unwrap().as_mut() {
        map.remove(&(session_id.to_string(), lane));
    }
    let _ = channel.send(StreamEvent::Done {
        lane,
        message_id: message_id.to_string(),
        status: "stopped".into(),
        confidence: None,
    });
}

/// Edit-and-resend support: drop the user message at `from_ts` and everything
/// after it (its replies, tool records, telemetry), and invalidate the prefix
/// state so the next request rebuilds from the trimmed transcript.
#[tauri::command]
pub fn rollback_session(state: State<'_, AppState>, session_id: String, from_ts: u64) -> Result<usize, String> {
    let mut sf = state.store.load(&session_id)?;
    let before = sf.messages.len();
    sf.messages.retain(|m| m.ts < from_ts);
    let removed = before - sf.messages.len();
    sf.telemetry.retain(|r| r.ts < from_ts);
    sf.meta.updated_at = now_ms();
    state.store.save(&sf)?;
    if let Some(map) = prefixes_lock().as_mut() {
        map.remove(&(session_id.clone(), 0));
    }
    if let Some(map) = LAST_SPAN.lock().unwrap().as_mut() {
        map.remove(&(session_id, 0));
    }
    Ok(removed)
}

/// User's answer to a write-tool approval card. `remember` records a
/// session-level grant for that tool (only a session — never global).
#[tauri::command]
pub fn resolve_approval(
    approval_id: String,
    session_id: String,
    tool: String,
    approved: bool,
    remember: bool,
) -> Result<(), String> {
    let tx = take_approval(&approval_id).ok_or("审批已不存在（可能已超时）")?;
    let _ = tx.send(approved);
    if approved && remember {
        if let Some(set) = GRANTS.lock().unwrap().as_mut() {
            set.insert(grant_key(&session_id, &tool));
        }
    }
    Ok(())
}

#[tauri::command]
pub async fn send_message(
    state: State<'_, AppState>,
    session_id: String,
    content: String,
    skill_calls: Option<Vec<String>>,
    images: Option<Vec<crate::prefix::ChatImage>>,
    channel: tauri::ipc::Channel<StreamEvent>,
) -> Result<SendResult, String> {
    let bindings = {
        let sf = state.store.load(&session_id)?;
        sf.meta.bindings.clone()
    };
    run_send(
        &state,
        session_id,
        content,
        skill_calls,
        images.unwrap_or_default(),
        bindings,
        channel,
        false,
    )
    .await?;
    Ok(SendResult { ok: true })
}

#[tauri::command]
pub async fn arena_send(
    state: State<'_, AppState>,
    session_id: String,
    content: String,
    skill_calls: Option<Vec<String>>,
    images: Option<Vec<crate::prefix::ChatImage>>,
    lanes: Vec<SessionBinding>,
    channel: tauri::ipc::Channel<StreamEvent>,
) -> Result<SendResult, String> {
    // persist lanes first so they survive restarts
    state.store.set_bindings(&session_id, lanes.clone())?;
    run_send(
        &state,
        session_id,
        content,
        skill_calls,
        images.unwrap_or_default(),
        lanes,
        channel,
        true,
    )
    .await?;
    Ok(SendResult { ok: true })
}

/// Group chat / round-table: the SAME prompt goes to N members SEQUENTIALLY.
/// Default (sequential) order is lane order; with `moderated`, after each
/// member speaks an LLM host picks the next speaker (or ends the table), so
/// the discussion is scheduled by relevance instead of fixed order. Each
/// later member's user message carries the earlier members' replies as a
/// quoted block; every member reuses the arena lane machinery (persistence,
/// StreamEvent, per-lane prefix cache) with lane = member index. A member
/// that fails or is stopped ends the round-table with the replies gathered
/// so far. Attached images ride only on the first member's turn.
#[tauri::command]
pub async fn group_send(
    state: State<'_, AppState>,
    session_id: String,
    content: String,
    skill_calls: Option<Vec<String>>,
    images: Option<Vec<crate::prefix::ChatImage>>,
    moderated: Option<bool>,
    lanes: Vec<SessionBinding>,
    channel: tauri::ipc::Channel<StreamEvent>,
) -> Result<SendResult, String> {
    if lanes.is_empty() {
        return Err("圆桌成员为空 —— 请先添加要参与的模型".into());
    }
    let moderated = moderated.unwrap_or(false);
    // persist lanes first so they survive restarts (same as arena)
    state.store.set_bindings(&session_id, lanes.clone())?;
    let host_binding = lanes[0].clone();
    // remaining speaker queue; default order = lane order
    let mut queue: Vec<usize> = (0..lanes.len()).collect();
    let mut round_text = String::new();
    let mut pending_images = images.unwrap_or_default();
    while let Some(idx) = queue.first().copied() {
        queue.remove(0);
        let binding = &lanes[idx];
        let member_content = if round_text.is_empty() {
            content.clone()
        } else {
            format!(
                "{content}\n\n---\n【圆桌讨论】其他成员已就上述问题发表看法，请阅读后发表你的观点（可以补充、反驳或修正他人，不要重复他人已说的内容）：\n{round_text}"
            )
        };
        // run_send awaits its (single) lane task internally, so this loop is
        // strictly sequential — each member sees all earlier replies.
        run_send(
            &state,
            session_id.clone(),
            member_content,
            skill_calls.clone(),
            std::mem::take(&mut pending_images),
            vec![binding.clone()],
            channel.clone(),
            true,
        )
        .await?;
        // read this member's final reply back from the persisted transcript
        let said: String = {
            let _guard = state.save_lock.lock().await;
            state
                .store
                .load(&session_id)
                .ok()
                .and_then(|sf| {
                    sf.messages
                        .iter()
                        .rev()
                        .find(|m| m.lane == idx as u32 && m.role == "assistant" && m.status == "ok")
                        .map(|m| m.content.clone())
                })
                .unwrap_or_default()
        };
        let said = said.trim().to_string();
        if said.is_empty() {
            break; // member failed or the user stopped it — end the table
        }
        round_text.push_str(&format!("\n【成员 {} · {}】\n{}\n", idx + 1, binding.model, said));
        // ---- LLM-host scheduling: pick the most valuable next speaker ----
        if moderated && !queue.is_empty() {
            let members = queue
                .iter()
                .map(|&j| format!("{}. {}", j + 1, lanes[j].model))
                .collect::<Vec<_>>()
                .join("\n");
            let ask = format!(
                "圆桌讨论进行中。议题：\n{content}\n\n已有发言：\n{round_text}\n\n剩余成员（序号. 模型）：\n{members}\n\n你是主持人。请选出最适合下一个发言的成员（能补充新视角、对抗验证或推进讨论者）。只输出一个成员序号（例如 2）；若认为讨论已充分，只输出：结束"
            );
            let next = match resolve_provider(&config::load(&state.data_dir), &host_binding) {
                Some(p) => chat::ask_once(
                    &state.client,
                    p,
                    &host_binding.model,
                    "你是圆桌讨论的主持人，负责调度发言顺序。只输出序号或「结束」。",
                    &ask,
                    60,
                )
                .await
                .ok(),
                None => None,
            };
            let mut picked: Option<usize> = None;
            if let Some(reply) = next {
                let r = reply.trim();
                if r.contains("结束") || r.eq_ignore_ascii_case("end") {
                    break;
                }
                // first digit in the reply names the member (1-based over
                // the listed remaining set)
                if let Some(d) = r.chars().find(|c| c.is_ascii_digit()).and_then(|c| c.to_digit(10)) {
                    let pos = (d as usize).saturating_sub(1);
                    if pos < queue.len() {
                        picked = Some(queue[pos]);
                    }
                }
                // parse failure ⇒ sequential fallback (queue order stands)
            }
            if let Some(p) = picked {
                if let Some(pos) = queue.iter().position(|&j| j == p) {
                    queue.remove(pos);
                    queue.insert(0, p);
                }
            }
        }
    }
    Ok(SendResult { ok: true })
}

fn resolve_provider<'a>(cfg: &'a AppConfig, binding: &SessionBinding) -> Option<&'a Provider> {
    cfg.providers
        .iter()
        .find(|p| p.id == binding.provider_id && p.enabled && !p.api_key.is_empty())
}

/// A named profile overrides provider+model; without one the sub inherits
/// the parent session's binding.
fn binding_for_sub(
    profile: Option<&crate::config::SubagentProfile>,
    parent: &SessionBinding,
) -> SessionBinding {
    match profile {
        Some(p) => SessionBinding {
            provider_id: p.provider_id.clone(),
            model: p.model.clone(),
        },
        None => parent.clone(),
    }
}

/// Look up an enabled named subagent profile by the delegate call's
/// `agent` argument (settings-managed + file-defined).
fn find_subagent_profile(
    cfg: &AppConfig,
    data_dir: &std::path::Path,
    args: &str,
) -> Option<crate::config::SubagentProfile> {
    let name = serde_json::from_str::<Value>(args)
        .ok()
        .and_then(|a| a.get("agent").and_then(|v| v.as_str()).map(|s| s.trim().to_string()))
        .unwrap_or_default();
    if name.is_empty() {
        return None;
    }
    all_subagents(cfg, data_dir).into_iter().find(|p| p.enabled && p.name == name)
}

/// Parse `<data_dir>/agents/*.md` into extra named-subagent profiles
/// (ZCode-style file definitions): frontmatter carries name / description /
/// model / tools (comma list) / max_turns / enabled; the markdown body is
/// the role system prompt. The model must belong to an enabled provider —
/// otherwise the file is skipped. Resolved on the fly, never persisted.
fn file_agent_profiles(cfg: &AppConfig, data_dir: &std::path::Path) -> Vec<crate::config::SubagentProfile> {
    let dir = data_dir.join("agents");
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else { return out };
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("md") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&p) else { continue };
        let Some(rest) = raw.strip_prefix("---") else { continue };
        let Some(i) = rest.find("\n---") else { continue };
        let fm = &rest[..i];
        let body = rest[i + 4..].trim_start_matches(['\r', '\n']).to_string();
        let mut name = String::new();
        let mut description = String::new();
        let mut model = String::new();
        let mut tools: Vec<String> = Vec::new();
        let mut max_turns: Option<u32> = None;
        let mut enabled = true;
        for line in fm.lines() {
            let Some((k, v)) = line.split_once(':') else { continue };
            let v = v.trim().trim_matches('"').trim_matches('\'').to_string();
            match k.trim() {
                "name" => name = v,
                "description" => description = v,
                "model" => model = v,
                "enabled" => enabled = v != "false",
                "max_turns" => max_turns = v.parse::<u32>().ok(),
                "tools" => {
                    let v = v.trim_start_matches('[').trim_end_matches(']').replace('，', ",");
                    tools = v
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                }
                _ => {}
            }
        }
        if name.is_empty() {
            name = p.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
        }
        if model.is_empty() {
            continue;
        }
        let Some(provider_id) = cfg
            .providers
            .iter()
            .find(|pr| pr.enabled && pr.models.iter().any(|m| *m == model))
            .map(|pr| pr.id.clone())
        else {
            continue;
        };
        out.push(crate::config::SubagentProfile {
            id: format!("file:{}", p.file_stem().and_then(|s| s.to_str()).unwrap_or(&name)),
            name,
            description,
            provider_id,
            model,
            system_prompt: body,
            enabled,
            tools,
            max_turns,
            source: "file".into(),
        });
    }
    out
}

/// All usable named-subagent profiles: settings-managed ones first, then
/// file-defined ones (<data_dir>/agents/*.md).
fn all_subagents(cfg: &AppConfig, data_dir: &std::path::Path) -> Vec<crate::config::SubagentProfile> {
    let mut v: Vec<crate::config::SubagentProfile> = cfg.subagents.clone();
    v.extend(file_agent_profiles(cfg, data_dir));
    v
}

/// Max tool rounds inside one sub-agent run. Sub-agents are read-only, so
/// the loop is naturally bounded; this guards pathological repetition.
const SUB_MAX_ROUNDS: usize = 12;
/// Result text handed back to the parent model (chars).
const SUB_RESULT_CAP: usize = 4_000;

/// Run a background sub-agent for `delegate_subagent`: its own hidden
/// session (kind "sub"), own prefix state, read-only tools, no MCP, no UI
/// streaming (events go to a discard channel). Returns the final conclusion
/// text for the parent's tool-result message. Sub-sessions are kept on disk
/// (inspectable) but excluded from sidebar lists by the frontend.
async fn run_subagent(
    client: &reqwest::Client,
    data_dir: &std::path::Path,
    parent_session: &str,
    task: &str,
    profile: Option<&crate::config::SubagentProfile>,
    parent_binding: &SessionBinding,
    cfg: &AppConfig,
    parent_channel: &tauri::ipc::Channel<StreamEvent>,
    parent_lane: u32,
    call_id: &str,
) -> Result<String, String> {
    let store = SessionStore::new(data_dir);
    let parent_ws = store.load(parent_session).ok().and_then(|sf| sf.meta.workspace.clone());
    let title: String = task.chars().take(20).collect();
    // live progress: the sub lane's deltas are forwarded to the parent UI,
    // tagged with the delegate tool-call id so the card can stream them
    let tap = chat::ProgressTap {
        lane: parent_lane,
        call_id: call_id.to_string(),
        title: title.clone(),
        channel: parent_channel.clone(),
    };
    let sf = store.create("sub", vec![binding_for_sub(profile, parent_binding)], &format!("🤖 子任务 · {title}"))?;
    let sub_id = sf.meta.id.clone();
    store.set_workspace(&sub_id, parent_ws.clone())?;

    let mut system_full = crate::sysprompt::assemble(&cfg.settings.system_prompt, parent_ws.as_deref());
    if let Some(p) = profile {
        let sp = p.system_prompt.trim();
        if !sp.is_empty() {
            system_full = format!("{system_full}\n\n# Subagent Role — {}\n{sp}", p.name);
        }
    }
    let binding = binding_for_sub(profile, parent_binding);
    let provider = resolve_provider(cfg, &binding).cloned().ok_or("Provider 未配置或未填 API Key")?;
    let model = binding.model.clone();
    // Tool surface: whitelist from the profile when given (write tools are
    // opt-in per profile), otherwise the safe read-only default.
    let tools = match profile {
        Some(p) if !p.tools.is_empty() => crate::agent_tools::schema_filtered(&p.tools),
        _ => crate::agent_tools::schema_readonly(),
    };
    let max_rounds = profile
        .and_then(|p| p.max_turns)
        .map(|n| (n as usize).clamp(1, 40))
        .unwrap_or(SUB_MAX_ROUNDS);
    // stable per-(parent, role) routing shard: repeated runs of the same
    // profile replay identical Zone S bytes and hit the warm shard instead
    // of cold-starting on a fresh per-run key
    let cache_key =
        crate::prefix::subagent_cache_key(parent_session, profile.map(|p| p.name.as_str()));

    // the task record carries workflow="subagent" so transcript_for_lane
    // injects SUBAGENT_DIRECTIVE — identical bytes live and after restart
    let task_record = MessageRecord {
        id: Uuid::new_v4().to_string(),
        lane: 0,
        role: "user".into(),
        content: task.to_string(),
        reasoning: None,
        ts: next_record_ts(),
        model: None,
        status: "ok".into(),
        usage: None,
        cost_usd: None,
        confidence: None,
        tool_calls: None,
        tool_call_id: None,
        skill_calls: None,
        workflow: Some("subagent".into()),
        images: Vec::new(),
    };
    {
        let mut s = store.load(&sub_id)?;
        s.messages.push(task_record.clone());
        store.save(&s)?;
    }

    // prefix state for the sub lane (same static map, distinct key)
    let owned_prefix = {
        let mut guard = prefixes_lock();
        let map = guard.get_or_insert_with(HashMap::new);
        let lp = map
            .entry((sub_id.clone(), 0u32))
            .or_insert_with(|| LanePrefix::new(&system_full, &cache_key));
        lp.bind_model(&model);
        // sub agents honor the same per-model sampling params (epoch-gated)
        let beh = provider.behavior.get(&model);
        lp.bind_behavior(beh.and_then(|b| b.temperature), beh.and_then(|b| b.max_output));
        // head-byte discipline: cache-tier toggle and MCP loadout changes are
        // expected rebuilds and must move the epoch, not fake an upstream miss
        lp.bind_cache_tier(provider.cache_tier());
        lp.bind_tools_hash(tools_hash(Some(&tools)));
        lp.clone()
    };

    let task_msg = ChatMessage::plain("user", format!("{}{task}", chat::SUBAGENT_DIRECTIVE));
    let mut sent_this_turn: Vec<ChatMessage> = vec![task_msg.clone()];
    let channel = tauri::ipc::Channel::<StreamEvent>::new(|_| Ok(()));
    let stop = Arc::new(AtomicBool::new(false));

    let mut final_text: Option<String> = None;
    let mut turn_status = "error".to_string();
    for _round in 1..=max_rounds {
        let body = chat::build_body(&provider, &model, &owned_prefix, &sent_this_turn, &system_full, Some(&tools));
        let message_id = Uuid::new_v4().to_string();
        let ctx = chat::SendCtx {
            client,
            provider: &provider,
            model: &model,
            lane: 0,
            message_id: message_id.clone(),
            channel: channel.clone(),
            stop: stop.clone(),
            progress_tap: Some(tap.clone()),
            // pi gating: the affinity header only makes sense while the tier
            // still routes via prompt_cache_key (tier None sends no marks)
            affinity: if provider.cache_tier() == crate::config::CacheTier::None {
                None
            } else {
                Some(cache_key.clone())
            },
        };
        let outcome = match chat::stream_lane(&ctx, body, chat::auth_for(&provider)).await {
            Ok(o) => o,
            Err(e) => return Err(e),
        };
        let usage = outcome.usage.clone();
        let cost = chat::cost_of(&usage, &provider, &model);
        let stat = RequestStat {
            seq: SEQ.fetch_add(1, Ordering::Relaxed),
            ts: now_ms(),
            lane: 0,
            model: model.clone(),
            epoch: owned_prefix.epoch,
            prefix_bytes: owned_prefix.prefix_bytes_public(),
            added_bytes: sent_this_turn.iter().map(|m| message_json(m).len() + 1).sum::<usize>(),
            chain_ok: true,
            input_tokens: usage.input,
            cached_tokens: usage.cached,
            output_tokens: usage.output,
            cost_usd: cost,
            significant_miss: false,
            rebilled_tokens: 0,
            rebilled_cost: None,
            miss_cause: None,
        };
        let has_tools = !outcome.tool_calls.is_empty();
        let tool_wire: Option<Vec<crate::types_rs::ToolCallWire>> = if has_tools {
            Some(
                outcome
                    .tool_calls
                    .iter()
                    .map(|tc| crate::types_rs::ToolCallWire {
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        arguments: tc.arguments.clone(),
                    })
                    .collect(),
            )
        } else {
            None
        };
        let asst_msg = ChatMessage {
            role: "assistant".into(),
            content: outcome.content.clone(),
            tool_calls: tool_wire.as_ref().map(|w| chat::tool_calls_wire_value(w)),
            tool_call_id: None,
            images: Vec::new(),
        };
        let record = MessageRecord {
            id: message_id,
            lane: 0,
            role: "assistant".into(),
            reasoning: if outcome.reasoning.is_empty() { None } else { Some(outcome.reasoning.clone()) },
            content: outcome.content.clone(),
            ts: next_record_ts(),
            model: Some(model.clone()),
            status: outcome.status.clone(),
            usage: Some(usage),
            cost_usd: cost,
            confidence: outcome.confidence,
            tool_calls: tool_wire,
            tool_call_id: None,
            skill_calls: None,
            workflow: None,
            images: Vec::new(),
        };
        sent_this_turn.push(asst_msg.clone());
        {
            let mut s = store.load(&sub_id)?;
            s.messages.push(record);
            s.telemetry.push(stat);
            s.meta.updated_at = now_ms();
            store.save(&s)?;
        }
        if outcome.status != "ok" {
            turn_status = outcome.status;
            break;
        }
        if !has_tools {
            final_text = Some(outcome.content);
            turn_status = "ok".into();
            break;
        }
        // execute read tools, feed results back
        for tc in &outcome.tool_calls {
            let result = match serde_json::from_str::<Value>(&tc.arguments) {
                Err(e) => format!("ERROR: 参数不是合法 JSON: {e}"),
                Ok(args) => crate::agent_tools::execute(parent_ws.as_deref().unwrap_or(""), &tc.name, &args),
            };
            // context-volume: same trim as the main lane, so a delegation
            // whose result is later promoted into the parent replay keeps
            // identical bytes
            let result = crate::spill::maybe_spill(
                &sub_id,
                &tc.name,
                &result,
                cfg.settings.spill_max_chars,
            );
            let tool_record = MessageRecord {
                id: Uuid::new_v4().to_string(),
                lane: 0,
                role: "tool".into(),
                content: result.clone(),
                reasoning: None,
                ts: next_record_ts(),
                model: None,
                status: "ok".into(),
                usage: None,
                cost_usd: None,
                confidence: None,
                tool_calls: None,
                tool_call_id: Some(tc.id.clone()),
                skill_calls: None,
                workflow: None,
                images: Vec::new(),
            };
            sent_this_turn.push(ChatMessage {
                role: "tool".into(),
                content: result,
                tool_calls: None,
                tool_call_id: Some(tc.id.clone()),
                images: Vec::new(),
            });
            let mut s = store.load(&sub_id)?;
            s.messages.push(tool_record);
            s.meta.updated_at = now_ms();
            store.save(&s)?;
        }
    }

    // fold the sub-turn into its lane's Zone H (mirrors run_send semantics)
    if turn_status != "error" {
        let mut guard = prefixes_lock();
        if let Some(map) = guard.as_mut() {
            if let Some(lp) = map.get_mut(&(sub_id, 0)) {
                for m in &sent_this_turn {
                    lp.append(m);
                }
            }
        }
    }
    // flip the parent's delegate card out of its streaming state
    tap.forward("", true);

    let text = final_text
        .ok_or("子任务在轮数上限内未产出结论")?
        .trim()
        .to_string();
    if text.is_empty() {
        return Err("子任务返回了空结论".into());
    }
    Ok(text.chars().take(SUB_RESULT_CAP).collect())
}

// ── /wiki 仓库导读 + # 历史会话引用（ZCode 上下文层 parity）─────────────

/// Where the generated repo digest lives for a workspace: co-located under
/// `.ccharness/` (same convention as the AGENTS.md fallback) so it is
/// visible, regenerable and trivially gitignored. sysprompt::assemble picks
/// it up as a Zone S layer for NEW sessions (same cache discipline as
/// AGENTS.md — mid-session generation never mutates a live prefix).
fn wiki_path(workspace: &str) -> std::path::PathBuf {
    std::path::Path::new(workspace).join(".ccharness").join("wiki.md")
}

/// Generate the repo digest (`/wiki`): a read-only Explore-style subagent
/// (own hidden session, parent's model binding, read tools only) scans the
/// workspace tree and key files, then writes a fixed five-section Markdown
/// overview. Saved to <workspace>/.ccharness/wiki.md; the inline result is
/// returned so the frontend can toast path + size.
#[tauri::command]
pub async fn wiki_generate(state: State<'_, AppState>, session_id: String) -> Result<Value, String> {
    let data_dir = state.data_dir.clone();
    let store = SessionStore::new(&data_dir);
    let sf = store.load(&session_id).map_err(|e| format!("会话不存在: {e}"))?;
    let ws = sf
        .meta
        .workspace
        .clone()
        .filter(|w| !w.trim().is_empty())
        .ok_or("当前会话未绑定工作区 —— 先用 /workspace <路径> 绑定，再生成仓库导读")?;
    let binding = sf.meta.bindings.first().cloned().ok_or("会话没有模型绑定")?;
    let cfg = config::load(&data_dir);
    let task = "为当前工作区生成一份「仓库导读」。步骤：\
1) 用 list_dir 浏览根目录与关键子目录（跳过 node_modules / target / dist / .git 等依赖与产物目录）；\
2) 读 README、清单文件（package.json / Cargo.toml / pyproject.toml 等）与 3-6 个核心源码入口；\
3) 输出 Markdown 导读，固定包含五节：\
## 项目定位（一句话）；## 技术栈与运行方式（构建/测试命令）；## 目录结构（带注释的树，只列关键目录）；\
## 核心模块与数据流（从入口到落点，谁调用谁）；## 改动须知（约定、边界、易踩的坑）。\
全文 500-900 字，全部基于实际读到的内容，禁止臆测；文件路径一律相对工作区根目录。只输出导读本身。";
    let channel = tauri::ipc::Channel::<StreamEvent>::new(|_| Ok(()));
    let text = run_subagent(
        &state.client,
        &data_dir,
        &session_id,
        task,
        None,
        &binding,
        &cfg,
        &channel,
        0,
        "wiki",
    )
    .await?;
    let path = wiki_path(&ws);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("无法创建 .ccharness 目录: {e}"))?;
    }
    let stamped = format!(
        "<!-- CCHarness /wiki 生成于 {}；重新生成会覆盖本文件 -->\n\n{text}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M")
    );
    fs::write(&path, &stamped).map_err(|e| format!("写入 wiki.md 失败: {e}"))?;
    Ok(serde_json::json!({
        "path": path.to_string_lossy(),
        "chars": text.chars().count(),
    }))
}

/// Deterministic digest of a past session for the `#` history-reference
/// picker: title / time / workspace / goal / user asks (first+last three) /
/// last conclusion. Pure text extraction — no model call, instant and free.
/// Capped so several references cannot blow up the prompt.
#[tauri::command]
pub fn session_digest(state: State<'_, AppState>, session_id: String) -> Result<String, String> {
    let store = SessionStore::new(&state.data_dir);
    let sf = store.load(&session_id).map_err(|e| format!("会话不存在: {e}"))?;
    let clip = |s: &str, n: usize| {
        let t = s.trim().replace('\n', " ");
        let mut out: String = t.chars().take(n).collect();
        if t.chars().count() > n {
            out.push('…');
        }
        out
    };
    let date = chrono::DateTime::from_timestamp_millis(sf.meta.updated_at as i64)
        .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default();
    let mut lines = vec![format!("会话「{}」({})", clip(&sf.meta.title, 60), date)];
    if let Some(ws) = sf.meta.workspace.as_deref().filter(|w| !w.trim().is_empty()) {
        lines.push(format!("工作区: {ws}"));
    }
    if let Some(g) = &sf.meta.goal {
        lines.push(format!("目标({}): {}", g.status, clip(&g.objective, 160)));
    }
    let asks: Vec<String> = sf
        .messages
        .iter()
        .filter(|m| m.role == "user" && m.workflow.is_none() && !m.content.trim().is_empty())
        .map(|m| clip(&m.content, 120))
        .collect();
    let n = asks.len();
    if n > 0 {
        let shown: Vec<String> = if n <= 6 {
            asks
        } else {
            let mut v: Vec<String> = asks[..3].to_vec();
            v.push("……".into());
            v.extend(asks[n - 3..].iter().cloned());
            v
        };
        lines.push(format!("用户要求（共 {n} 条）:\n- {}", shown.join("\n- ")));
    }
    if let Some(last) = sf
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "assistant" && !m.content.trim().is_empty())
    {
        lines.push(format!("最近结论: {}", clip(&last.content, 240)));
    }
    let digest = lines.join("\n");
    Ok(digest.chars().take(2400).collect())
}

/// Garnish cap per ToT candidate (chars) — keeps the rehearsal block small
/// even when a model is verbose.
const TOT_CANDIDATE_CAP: usize = 1_500;

/// Deep mode (ToT-style rehearsal): three candidate approaches are drafted
/// in parallel (safe / rigorous / creative angles), then a judge ask_once
/// picks the most promising one. Returns a text block to append to the user
/// message actually SENT this turn — same garnish mechanism as vector-memory
/// recall (persisted record stays clean; restarts rebuild a new epoch
/// without it). Any failure returns None: deep mode must never block a
/// normal send.
async fn run_tot_rehearsal(
    client: &reqwest::Client,
    provider: &crate::config::Provider,
    model: &str,
    task: &str,
) -> Option<String> {
    let clip = |s: &str, n: usize| s.chars().take(n).collect::<String>();
    let angles = [
        ("A", "最直接稳妥的方案：步骤最少、依赖最少、最快给出可用结果"),
        ("B", "最周全严谨的方案：覆盖边界情况、风险与备选路径"),
        ("C", "最有创意的方案：换一个不寻常但可能更优的切入点"),
    ];
    let prompts: Vec<String> = angles
        .iter()
        .map(|(tag, angle)| {
            format!(
                "你是方案规划专家。针对用户任务，只输出「方案{tag}」：{angle}。用简洁的编号步骤（最多 5 步），不要寒暄，不要输出方案 {tag} 以外的内容。"
            )
        })
        .collect();
    let mut futs = Vec::new();
    for p in &prompts {
        futs.push(chat::ask_once(client, provider, model, p, task, 700));
    }
    let results = futures_util::future::join_all(futs).await;
    let mut cands: Vec<(char, String)> = Vec::new();
    for ((tag, _), r) in angles.iter().zip(results) {
        if let Ok(text) = r {
            let text = text.trim();
            if !text.is_empty() {
                let tag_ch = tag.chars().next().unwrap_or('A');
                cands.push((tag_ch, clip(text, TOT_CANDIDATE_CAP)));
            }
        }
    }
    if cands.is_empty() {
        return None;
    }
    // judge: pick the best candidate; parse failure falls back to A
    let listing = cands
        .iter()
        .map(|(tag, text)| format!("方案{tag}：{text}"))
        .collect::<Vec<_>>()
        .join("\n\n");
    let judge_prompt = format!(
        "任务：{}\n\n三个候选方案：\n{listing}\n\n你是评审专家。选出最有希望成功的方案，只输出 JSON：{{\"best\":\"A\"|\"B\"|\"C\",\"reason\":\"一句话理由\"}}",
        clip(task, 1_200)
    );
    let verdict = chat::ask_once(
        client,
        provider,
        model,
        "你是评审专家，只输出 JSON，不要输出其他内容。",
        &judge_prompt,
        200,
    )
    .await
    .ok();
    let (best, reason) = match verdict.as_deref().map(str::trim) {
        Some(v) => {
            // tolerate markdown fences / surrounding prose: take {..}
            let json_txt = match (v.find('{'), v.rfind('}')) {
                (Some(s), Some(e)) if e > s => &v[s..=e],
                _ => v,
            };
            match serde_json::from_str::<serde_json::Value>(json_txt) {
                Ok(j) => {
                    let b = j
                        .get("best")
                        .and_then(|x| x.as_str())
                        .and_then(|s| s.chars().next())
                        .map(|c| c.to_ascii_uppercase())
                        .unwrap_or('A');
                    let b = if cands.iter().any(|(tag, _)| *tag == b) { b } else { 'A' };
                    let r = j.get("reason").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
                    (b, r)
                }
                Err(_) => ('A', String::new()),
            }
        }
        None => ('A', String::new()),
    };
    let mut block = String::from("\n\n[深度推理预演（ToT）— 三方案并行生成，评审选优]\n");
    for (tag, text) in &cands {
        block.push_str(&format!("方案{tag}：{text}\n\n"));
    }
    if reason.is_empty() {
        block.push_str(&format!("评审结论：采用方案{best}。\n\n请按选中的方案作答（可吸收其他方案的优点）。"));
    } else {
        block.push_str(&format!("评审结论：采用方案{best} —— {reason}\n\n请按选中的方案作答（可吸收其他方案的优点）。"));
    }
    Some(block)
}

/// Garnish cap per review expert (chars).
const REVIEW_EXPERT_CAP: usize = 1_500;

/// Review mode (better-harness style findings pre-review): three
/// mutually-exclusive read-only experts review in parallel — correctness,
/// security boundaries, maintainability/test gaps — each emitting one-line
/// findings (severity / consequence / root cause / location / verifier).
/// The model later acts as the Lead (dedupe + grade + publish), so there is
/// deliberately NO judge call here. Returns a text block to append to the
/// user message actually SENT this turn (same garnish mechanism as ToT).
/// Any failure returns None: review mode must never block a normal send.
async fn run_review_rehearsal(
    client: &reqwest::Client,
    provider: &crate::config::Provider,
    model: &str,
    task: &str,
    diff: Option<&str>,
    data_dir: &std::path::Path,
) -> Option<String> {
    let clip = |s: &str, n: usize| s.chars().take(n).collect::<String>();
    // Expert lenses can be overridden by <data_dir>/agents/review-*.md
    // (frontmatter optional; body = role instructions). Missing/empty file
    // falls back to the built-in lens.
    let lens_or_file = |stem: &str, fallback: &str| -> String {
        std::fs::read_to_string(data_dir.join("agents").join(format!("{stem}.md")))
            .ok()
            .map(|raw| {
                let body = match raw.strip_prefix("---") {
                    Some(rest) => match rest.find("\n---") {
                        Some(i) => rest[i + 4..].to_string(),
                        None => raw.clone(),
                    },
                    None => raw.clone(),
                };
                let b = body.trim().to_string();
                if b.is_empty() {
                    fallback.to_string()
                } else {
                    b
                }
            })
            .unwrap_or_else(|| fallback.to_string())
    };
    let experts = [
        ("A", "正确性", lens_or_file("review-correctness", "正确性与逻辑缺陷：边界条件、错误处理、并发/时序、空值与溢出")),
        ("B", "安全边界", lens_or_file("review-security", "安全边界：注入、SSRF、密钥泄露、越权访问、不可信输入未校验")),
        ("C", "可维护性", lens_or_file("review-maintainability", "可维护性与测试缺口：重复逻辑、复杂度失控、命名误导、缺失测试")),
    ];
    let subject = match diff {
        Some(d) => format!(
            "{}\n\n--- 待审阅的未提交变更（git diff HEAD）---\n{}",
            clip(task, 1_200),
            d
        ),
        None => clip(task, 6_000),
    };
    let prompts: Vec<String> = experts
        .iter()
        .map(|(_tag, _label, prompt)| {
            format!(
                "你是代码审阅专家（只读视角，只负责：{prompt}）。针对待审阅内容逐条输出发现，每条一行，格式严格为：\n- [严重度] 后果 ← 根因 @ 位置 → 验证方式\n其中严重度 ∈ 严重/主要/次要；位置写 文件:行号（无文件上下文时写 任务级）；验证方式一句话说明如何证实或复现。只输出本视角的发现，不要涉及其他视角，不要寒暄；没有发现则只输出「无发现」。"
            )
        })
        .collect();
    let mut futs = Vec::new();
    for p in &prompts {
        futs.push(chat::ask_once(client, provider, model, p, &subject, 800));
    }
    let results = futures_util::future::join_all(futs).await;
    let mut block = String::from("\n\n[三专家审阅预演 — 并行只读评审，待汇合定级]\n");
    let mut any = false;
    for ((tag, label, _), r) in experts.iter().zip(results) {
        let text = r.ok().map(|s| s.trim().to_string()).unwrap_or_default();
        if text.is_empty() {
            continue;
        }
        any = true;
        block.push_str(&format!("专家{tag}（{label}）：{}\n\n", clip(&text, REVIEW_EXPERT_CAP)));
    }
    if !any {
        return None;
    }
    block.push_str(
        "请按审阅模式规则汇合：先用只读工具核实，再输出去重定级后的最终发现表；无法核实的条目标注「待核」。",
    );
    Some(block)
}

async fn run_send(
    state: &State<'_, AppState>,
    session_id: String,
    content: String,
    skill_calls: Option<Vec<String>>,
    images: Vec<crate::prefix::ChatImage>,
    lanes: Vec<SessionBinding>,
    channel: tauri::ipc::Channel<StreamEvent>,
    arena: bool,
) -> Result<(), String> {
    if content.trim().is_empty() {
        return Err("空消息".into());
    }

    // cancellation flag for this session
    let stop = Arc::new(AtomicBool::new(false));
    state.stops.lock().unwrap().insert(session_id.clone(), stop.clone());

    // persist the user message once (lane 0); skill names are validated
    // against what is actually installed so stale chips cannot persist
    let cfg = config::load(&state.data_dir);
    let system = cfg.settings.system_prompt.clone();
    let ws = state.store.load(&session_id).ok().and_then(|sf| sf.meta.workspace.clone());
    let valid_skills: std::collections::HashSet<String> = crate::skills::scan(ws.as_deref())
        .into_iter()
        .map(|s| s.name)
        .collect();
    let skill_calls: Vec<String> = skill_calls
        .unwrap_or_default()
        .into_iter()
        .filter(|n| valid_skills.contains(n))
        .collect();
    // attached images land in the session's attachment dir BEFORE the record
    // is written — the record only carries immutable relative filenames, so
    // rebuilds re-reading the files produce byte-identical requests
    let image_names = if images.is_empty() {
        Vec::new()
    } else {
        crate::sessions::save_attachments(&state.data_dir, &session_id, &images)?
    };
    let user_record = MessageRecord {
        id: Uuid::new_v4().to_string(),
        lane: 0,
        role: "user".into(),
        content: content.clone(),
        reasoning: None,
        ts: next_record_ts(),
        model: None,
        status: "ok".into(),
        usage: None,
        cost_usd: None,
        confidence: None,
        tool_calls: None,
        tool_call_id: None,
        skill_calls: if skill_calls.is_empty() { None } else { Some(skill_calls) },
        workflow: match workflow_of_in(&session_id, &state.data_dir).as_str() {
            "plan" => Some("plan".into()),
            "goal" => Some("goal".into()),
            "deep" => Some("deep".into()),
            // the full gate "sm:<def>:<state>" rides on the record so every
            // later rebuild injects the directive of the state the message
            // was actually sent under (byte-stable per record)
            w if w.starts_with("sm:") => Some(w.into()),
            _ => None,
        },
        images: image_names,
    };
    {
        let _guard = state.save_lock.lock().await;
        let mut sf = state.store.load(&session_id)?;
        sf.messages.push(user_record.clone());
        sf.meta.updated_at = now_ms();
        state.store.save(&sf)?;
    }

    // boundary compaction fires here — only at the user boundary, only for
    // chat sessions, only when the last request approached the window
    if !arena {
        maybe_auto_compact(state, &session_id, &cfg, &stop).await;
    }
    // MCP handshakes are lazy — warm them before lanes spawn so the first
    // model turn sees the full tool surface
    if cfg.mcp_servers.iter().any(|s| s.enabled) {
        ensure_mcp_servers(&state.data_dir, &stop).await;
    }

    // one task per lane
    let mut handles = Vec::new();
    for (lane_idx, binding) in lanes.iter().enumerate() {
        let lane = lane_idx as u32;
        let binding = binding.clone();
        let session_id = session_id.clone();
        let channel = channel.clone();
        let stop = stop.clone();
        let save_lock = state.save_lock.clone();
        let data_dir = state.data_dir.clone();
        let client = state.client.clone();
        let system = system.clone();

        let handle = tauri::async_runtime::spawn(async move {
            let cfg = config::load(&data_dir);
            let provider = match resolve_provider(&cfg, &binding) {
                Some(p) => p.clone(),
                None => {
                    let _ = channel.send(StreamEvent::Error {
                        lane,
                        message: "Provider 未配置或未填 API Key".into(),
                    });
                    return;
                }
            };
            let model = binding.model.clone();
            // a new turn cancels any pending keepalive probe on this lane —
            // it must never fire between our pre-stream phases and requests
            crate::warmer::cancel(&session_id, lane);
            let store = SessionStore::new(&data_dir);

            // workspace + layered Zone S (identity → user global → AGENTS.md → env).
            // Worktree isolation: while active, every tool — read, write,
            // approval preview, write-log snapshot — operates inside the
            // session's worktree; the main checkout is untouched until the
            // user merges (or discards) from the composer capsule.
            let sf_loaded = store.load(&session_id).ok();
            let mut workspace = sf_loaded.as_ref().and_then(|sf| sf.meta.workspace.clone());
            if let Some(wt) = sf_loaded.as_ref().and_then(|sf| sf.meta.wt.clone()) {
                if std::path::Path::new(&wt.path).is_dir() {
                    workspace = Some(wt.path);
                }
            }
            let perm_base = permission_of(&session_id);
            // sandbox: the risky "auto" write tier degrades to per-action
            // approval — fail-safe rather than convenient. 文件白名单的可信
            // 路径在自动模式下保持免审批（见写路径的 trusted 判定）。
            let sb = crate::agent_tools::sandbox_policy();
            let perm_mode = if sb.on && perm_base == "auto" { "approve" } else { perm_base };
            // plan gate: read-only tool surface, no MCP, no writes — the
            // directive itself rides on the user message (see transcript_for_lane).
            // goal gate: full surface, but more tool rounds per turn.
            // image gate: handled below via chat::image_generate — the turn
            // never enters the chat pipeline at all.
            let wf = workflow_of_in(&session_id, &data_dir);
            let plan_mode = wf == "plan";
            let goal_mode = wf == "goal";
            let deep_mode = wf == "deep";
            let review_mode = wf == "review";
            let image_mode = wf == "image";
            // declarative state machine: resolve the current state (if the
            // def was deleted or redefined since the gate was set, degrade
            // gracefully to an unrestricted agent turn)
            let sm_state = match wf.strip_prefix("sm:").and_then(|r| r.split_once(':')) {
                Some((def_id, state_name)) => {
                    chat::resolve_sm(&cfg.workflows, def_id, state_name)
                }
                None => None,
            };
            let sm_tools: Option<&str> = sm_state.as_ref().map(|(_, st)| st.tools.as_str());
            let sm_no_tools = sm_tools == Some("none");
            let sm_ro_tools = sm_tools == Some("readonly");
            let system_full = crate::sysprompt::assemble(&system, workspace.as_deref());
            let tools_on = cfg.settings.agent_tools && workspace.is_some() && !sm_no_tools;
            let mcp_enabled = cfg.settings.agent_tools
                && !plan_mode
                && !review_mode
                && !sm_no_tools
                && !sm_ro_tools
                && cfg.mcp_servers.iter().any(|s| s.enabled);
            let mut schema_all: Vec<Value> = Vec::new();
            if tools_on {
                schema_all.extend(
                    match perm_mode == "readonly" || plan_mode || review_mode || sm_ro_tools {
                        true => crate::agent_tools::schema_readonly().as_array().cloned().unwrap_or_default(),
                        false => crate::agent_tools::schema().as_array().cloned().unwrap_or_default(),
                    },
                );
            }
            if mcp_enabled {
                schema_all.extend(state_mcp_tools(&data_dir));
            }
            // advertise enabled named subagents in the delegate tool so the
            // model can pick `agent` meaningfully (settings + file-defined)
            if tools_on && !plan_mode {
                let names: Vec<String> = all_subagents(&cfg, &data_dir)
                    .iter()
                    .filter(|p| p.enabled)
                    .map(|p| {
                        if p.description.trim().is_empty() {
                            p.name.clone()
                        } else {
                            format!("{}（{}）", p.name, p.description.trim())
                        }
                    })
                    .collect();
                if !names.is_empty() {
                    if let Some(tool) = schema_all.iter_mut().find(|t| {
                        t.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str())
                            == Some("delegate_subagent")
                    }) {
                        tool["function"]["description"] = Value::String(format!(
                            "把一个相对独立的调研/分析子任务委派给后台子智能体（独立上下文、只读工具），完成后以结论回报。task 需自包含。当前可用的具名子智能体（agent 参数填名字）：{}。不指定 agent 时使用主会话的模型配置。",
                            names.join("；")
                        ));
                    }
                }
            }
            // goal lifecycle tools (Codex /goal parity): the model can read
            // the goal, create one (flips the gate), and declare achieved /
            // unmet. Session-scoped — they work even without a workspace,
            // so they ride regardless of tools_on.
            if goal_mode {
                schema_all.push(serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": "get_goal",
                        "description": "查看当前会话的目标状态：目标内容、生命周期状态（active/paused/achieved/unmet/budget_limited）与验收清单进度（x/y、是否已全部满足）。",
                        "parameters": {"type": "object", "properties": {}, "required": []}
                    }
                }));
                schema_all.push(serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": "create_goal",
                        "description": "创建（或替换）当前会话的目标并进入目标模式。objective 需自包含且可映射为可验证的验收清单：一句话目标 + 范围（Scope）+ 硬性约束（Constraints）+ 验收标准（Done when，每条可验证）+ 停止条件（Stop if）。避免「全部/所有/彻底/improve」这类无法映射成清单的虚词。",
                        "parameters": {
                            "type": "object",
                            "properties": {
                                "objective": {
                                    "type": "string",
                                    "description": "目标全文（含范围/约束/验收标准/停止条件）"
                                }
                            },
                            "required": ["objective"]
                        }
                    }
                }));
                schema_all.push(serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": "update_goal",
                        "description": "声明目标达成（status=achieved）或判定无法达成（status=unmet）。调用前必须完成审计：逐条核对验收标准并确认每条 ✅ 都有可核验证据（文件路径/命令输出/测试名）；代理信号（测试通过、代码写完）不能单独作为依据；不确定视作未达成。暂停/恢复/清除不可通过此工具操作。",
                        "parameters": {
                            "type": "object",
                            "properties": {
                                "status": {"type": "string", "enum": ["achieved", "unmet"], "description": "achieved=目标达成；unmet=判定无法达成（需说明原因）"}
                            },
                            "required": ["status"]
                        }
                    }
                }));
            }
            let tools_schema = if schema_all.is_empty() {
                None
            } else {
                Some(Value::Array(schema_all))
            };

            // ---- prefix state (synchronous, brief lock) ----
            let cache_key = format!("ccharness-{session_id}-{lane}");
            // privacy scrub context: outbound messages get type-consistent
            // surrogates; records keep the originals (see privacy.rs)
            let privacy_on = cfg.settings.privacy_mode;
            let pseed = crate::privacy::session_seed(&data_dir, &session_id);
            let scrub_outbound = |msgs: &mut [crate::prefix::ChatMessage]| {
                if privacy_on {
                    for m in msgs.iter_mut() {
                        m.content = crate::privacy::outbound(&session_id, &pseed, &m.content);
                    }
                }
            };
            let sf_snapshot = match store.load(&session_id) {
                Ok(sf) => sf,
                Err(e) => {
                    let _ = channel.send(StreamEvent::Error { lane, message: e });
                    return;
                }
            };

            // ---- image gate: this turn goes to POST {base}/images/generations,
            // not the chat pipeline — no prefix state, no tools, no telemetry
            // ledger entry (the image API has no chat usage to record) ----
            if image_mode {
                let prompt = sf_snapshot
                    .messages
                    .iter()
                    .rev()
                    .find(|m| m.role == "user")
                    .map(|m| m.content.clone())
                    .unwrap_or_default();
                let message_id = Uuid::new_v4().to_string();
                let ctx = SendCtx {
                    client: &client,
                    provider: &provider,
                    model: &model,
                    lane,
                    message_id: message_id.clone(),
                    channel: channel.clone(),
                    stop: stop.clone(),
                    progress_tap: None,
                    // image generation has no prefix cache — no affinity
                    affinity: None,
                };
                let outcome = match chat::image_generate(&ctx, &prompt).await {
                    Ok(o) => o,
                    Err(e) => {
                        let _ = channel.send(StreamEvent::Error { lane, message: e });
                        let _ = channel.send(StreamEvent::Done {
                            lane,
                            message_id,
                            status: "error".into(),
                            confidence: None,
                        });
                        return;
                    }
                };
                let record = MessageRecord {
                    id: message_id.clone(),
                    lane,
                    role: "assistant".into(),
                    reasoning: None,
                    content: outcome.content.clone(),
                    ts: next_record_ts(),
                    model: Some(model.clone()),
                    status: outcome.status.clone(),
                    usage: None,
                    cost_usd: None,
                    confidence: None,
                    tool_calls: None,
                    tool_call_id: None,
                    skill_calls: None,
                    workflow: None,
                    images: Vec::new(),
                };
                {
                    let _guard = save_lock.lock().await;
                    if let Ok(mut sf) = store.load(&session_id) {
                        sf.messages.push(record);
                        sf.meta.updated_at = now_ms();
                        let _ = store.save(&sf);
                    }
                }
                let _ = channel.send(StreamEvent::Done {
                    lane,
                    message_id,
                    status: outcome.status,
                    confidence: None,
                });
                return;
            }

            let (owned_prefix, system_injection, memo_injection) = {
                let mut guard = prefixes_lock();
                let map = guard.get_or_insert_with(HashMap::new);
                let key = (session_id.clone(), lane);
                let lp = map
                    .entry(key)
                    .or_insert_with(|| LanePrefix::new(&system_full, &cache_key));
                lp.bind_model(&model);
                // per-model behavior overrides (temperature / max_tokens /
                // reasoning) with global-thinking fallback, all epoch-gated
                let beh = provider.behavior.get(&model);
                lp.bind_behavior(beh.and_then(|b| b.temperature), beh.and_then(|b| b.max_output));
                let reasoning = beh
                    .and_then(|b| b.reasoning.as_deref())
                    .filter(|r| !r.is_empty() && *r != "default")
                    .or(if cfg.settings.thinking_level == "default" {
                        None
                    } else {
                        Some(cfg.settings.thinking_level.as_str())
                    });
                lp.bind_thinking(reasoning);
                // head-byte discipline: cache-tier toggle and MCP loadout
                // changes rewrite the head without touching Zone H — they
                // must bump the epoch (expected rebuild) rather than masquer
                // -ade as an upstream cache miss in telemetry
                lp.bind_cache_tier(provider.cache_tier());
                lp.bind_tools_hash(tools_hash(tools_schema.as_ref()));
                // system change (settings/workspace/AGENTS.md) or privacy-mode
                // toggle or restart recovery ⇒ rebuild Zone H from the
                // persisted transcript (re-scrubbed under the new flag) —
                // UNLESS in-history updates are on: then a system change
                // rides as an injected system message (below) and the head
                // keeps its cache identity
                let privacy_changed = lp.bind_privacy(privacy_on);
                let system_changed = !lp.system_is(&system_full);
                let in_history_ok = cfg.settings.system_update_mode == "in-history"
                    && provider.kind == crate::config::ProviderKind::OpenaiCompatible
                    && !privacy_on;
                let needs_rebuild = privacy_changed
                    || lp_is_empty(lp)
                    || (system_changed && !in_history_ok);
                // adopt only when NOT rebuilding: a fresh/rebuilt prefix
                // carries the current system text in Zone S already —
                // injecting it again would duplicate the prompt in the
                // same request (and then in Zone H at turn end)
                let mut system_injection: Option<String> = None;
                if !needs_rebuild
                    && system_changed
                    && in_history_ok
                    && lp.adopt_system_in_history(&system_full)
                {
                    system_injection = Some(system_full.clone());
                }
                if needs_rebuild {
                    let mut hist = chat::transcript_for_lane(&sf_snapshot, lane, &cfg.workflows, &data_dir);
                    scrub_outbound(&mut hist);
                    if hist.len() > 1 {
                        lp.rebuild(&system_full, &hist[..hist.len() - 1]);
                    }
                }
                // RollingMemo injection (L6 §4.3): after a rebuild the
                // re-created Zone H carries no memo messages, so re-inject
                // once; otherwise inject when the memo changed since the
                // last injection (rev watermark). The message is not
                // persisted — it falls into Zone H at turn end, and a
                // restart re-injects via the rebuild branch above. Privacy
                // mode scrubs the rendered block like any outbound text.
                let memo_injection = if lane == 0 {
                    let rev = sf_snapshot.meta.rolling_memo_rev;
                    let injected = sf_snapshot.meta.rolling_memo_injected_rev;
                    match sf_snapshot.meta.rolling_memo.as_ref().and_then(render_memo) {
                        Some(text) if needs_rebuild || rev > injected => Some(if privacy_on {
                            crate::privacy::outbound(&session_id, &pseed, &text)
                        } else {
                            text
                        }),
                        _ => None,
                    }
                } else {
                    None
                };
                (lp.clone(), system_injection, memo_injection)
            };
            // advance the injection watermark when a memo block went out,
            // so the same revision never injects twice (a later rebuild
            // re-injects via the needs_rebuild branch regardless)
            if memo_injection.is_some() {
                let _guard = save_lock.lock().await;
                if let Ok(mut s) = store.load(&session_id) {
                    s.meta.rolling_memo_injected_rev = s.meta.rolling_memo_rev;
                    s.meta.updated_at = now_ms();
                    let _ = store.save(&s);
                }
            }

            let mut transcript = chat::transcript_for_lane(&sf_snapshot, lane, &cfg.workflows, &data_dir);
            scrub_outbound(&mut transcript);
            if transcript.is_empty() {
                let _ = channel.send(StreamEvent::Error { lane, message: "会话内容为空".into() });
                return;
            }

            // Everything sent beyond Zone H this turn, in order. The next
            // request replays it verbatim; at turn end it falls into Zone H.
            let mut sent_this_turn: Vec<ChatMessage> = vec![transcript.last().unwrap().clone()];
            // RollingMemo rides ahead of the user turn (L6 §4.3). Inserted
            // BEFORE the system update below so the fixed wire order is
            // [system update, memo, user] — deterministic across replays.
            // Not persisted; falls into Zone H at turn end via the append.
            if let Some(text) = &memo_injection {
                sent_this_turn.insert(0, ChatMessage::plain("system", text.clone()));
            }
            // in-history system update: the new system text rides ahead of
            // the user turn. The message is NOT persisted — restarts rebuild
            // Zone S from the current settings instead — and it falls into
            // Zone H at turn end via the append below, keeping the head
            // byte-stable for the whole run.
            if let Some(sys) = &system_injection {
                sent_this_turn.insert(0, ChatMessage::plain("system", sys.clone()));
            }
            // Files API image reuse (opt-in per provider): resolve every
            // attached image to a server-side file id once, then reference
            // it by id — the base64 payload never re-enters the request.
            // Upload failures fall back to inline data URIs.
            if provider.images_via_files
                && provider.kind == crate::config::ProviderKind::OpenaiCompatible
                && sent_this_turn.iter().any(|m| !m.images.is_empty())
            {
                crate::deepfiles::ensure_file_refs(
                    &client,
                    &data_dir,
                    &provider.id,
                    &provider.base_url,
                    &provider.api_key,
                    &mut sent_this_turn,
                )
                .await;
            }
            let mut last_confidence: Option<u32> = None;
            // The assistant bubble is created BEFORE the slow pre-stream
            // phases below (memory recall / deep rehearsal / review experts)
            // so those turns show a message card immediately instead of
            // looking frozen. The first stream round reuses this id and the
            // frontend dedupes Started events per lane, so exactly one
            // bubble appears per lane per turn.
            let first_message_id = Uuid::new_v4().to_string();
            let mut last_message_id = first_message_id.clone();
            let has_pre_phase = (cfg.settings.vector_memory
                && !cfg.settings.embeddings_url.trim().is_empty())
                || deep_mode
                || review_mode;
            if has_pre_phase {
                let _ = channel.send(StreamEvent::Started {
                    lane,
                    model: model.clone(),
                    message_id: first_message_id.clone(),
                });
            }
            let mut turn_status = "error".to_string();
            // per-turn delegation budget for delegate_subagent
            let mut delegations = 0usize;
            // SM conditional-branch + parallel-fanout bookkeeping: tools
            // invoked this turn, accumulated reply text (for contains:
            // predicates), and the one-shot fan-out guard
            let mut turn_tools: Vec<String> = Vec::new();
            let mut turn_text = String::new();
            let mut fanout_done = false;

            // Vector memory recall: attach the top-k semantically relevant
            // memories as a suffix on the user message actually SENT this
            // turn. The persisted record stays clean; within the turn every
            // replay uses the same suffixed bytes, and at turn end the
            // suffixed message falls into Zone H — restarts simply rebuild a
            // new epoch without the garnish.
            if cfg.settings.vector_memory && !cfg.settings.embeddings_url.trim().is_empty() {
                // find the user message by role: with an in-history system
                // injection, sent_this_turn[0] may be the injected system
                // message instead
                let user_text = sent_this_turn
                    .iter()
                    .find(|m| m.role == "user")
                    .map(|m| m.content.clone())
                    .unwrap_or_default();
                let ws = sf_snapshot.meta.workspace.clone().unwrap_or_default();
                if !user_text.is_empty() && !ws.is_empty() {
                    let _ = channel.send(StreamEvent::Reasoning {
                        lane,
                        message_id: first_message_id.clone(),
                        text: "正在检索相关的长期记忆…\n\n".to_string(),
                    });
                    let recall =
                        crate::memvector::recall(&client, &cfg, &data_dir, &ws, &user_text, 3);
                    // stop-aware: a click during the recall ends the turn
                    // instead of being ignored until the first token
                    tokio::select! {
                        _ = wait_stopped(&stop) => {
                            abort_lane_pre_stream(&channel, &session_id, lane, &first_message_id).await;
                            return;
                        }
                        mems = recall => {
                            if !mems.is_empty() {
                                let block = format!(
                                    "\n\n[长期记忆参考 — 与本条消息语义相关的既往记忆]\n{}",
                                    mems.iter().map(|m| format!("- {m}")).collect::<Vec<_>>().join("\n")
                                );
                                if let Some(user_msg) = sent_this_turn.iter_mut().find(|m| m.role == "user") {
                                    user_msg.content.push_str(&block);
                                }
                            }
                        }
                    }
                }
            }

            // Deep reasoning (ToT-style rehearsal): three candidate
            // approaches are generated in parallel, an LLM judge picks one,
            // and the whole rehearsal is appended to the user message
            // actually SENT this turn — same garnish mechanism as the memory
            // recall above (persisted record stays clean; restarts rebuild
            // a new epoch without it). Any rehearsal failure just proceeds
            // without it.
            if deep_mode && !sent_this_turn.is_empty() {
                let task_text = sent_this_turn
                    .iter()
                    .find(|m| m.role == "user")
                    .map(|m| m.content.clone())
                    .unwrap_or_default();
                let _ = channel.send(StreamEvent::Reasoning {
                    lane,
                    message_id: first_message_id.clone(),
                    text: "正在并行预演三个候选方案（深度推理两阶段：三方案生成 + 评审选优）…\n\n".to_string(),
                });
                tokio::select! {
                    _ = wait_stopped(&stop) => {
                        abort_lane_pre_stream(&channel, &session_id, lane, &first_message_id).await;
                        return;
                    }
                    block = run_tot_rehearsal(&client, &provider, &model, &task_text) => {
                        if let Some(block) = block {
                            if let Some(user_msg) = sent_this_turn.iter_mut().find(|m| m.role == "user") {
                                user_msg.content.push_str(&block);
                            }
                        }
                    }
                }
            }

            // Review mode (better-harness style): three mutually-exclusive
            // read-only experts pre-review in parallel (correctness / security
            // / maintainability); the model itself reconciles their findings
            // into the final graded table. Same garnish mechanism as ToT.
            if review_mode && !sent_this_turn.is_empty() {
                let task_text = sent_this_turn
                    .iter()
                    .find(|m| m.role == "user")
                    .map(|m| m.content.clone())
                    .unwrap_or_default();
                let diff = workspace.as_deref().and_then(|ws| {
                    crate::worktree::git(std::path::Path::new(ws), &["diff", "HEAD"])
                        .ok()
                        .map(|d| d.chars().take(6_000).collect::<String>())
                        .filter(|d| !d.trim().is_empty())
                });
                let _ = channel.send(StreamEvent::Reasoning {
                    lane,
                    message_id: first_message_id.clone(),
                    text: "正在并行预审三个只读专家（正确性 / 安全性 / 可维护性）…\n\n".to_string(),
                });
                // stop-aware: a click during the pre-review ends the turn
                // instead of being ignored until the first token
                tokio::select! {
                    _ = wait_stopped(&stop) => {
                        abort_lane_pre_stream(&channel, &session_id, lane, &first_message_id).await;
                        return;
                    }
                    block = run_review_rehearsal(&client, &provider, &model, &task_text, diff.as_deref(), &data_dir) => {
                        if let Some(block) = block {
                            if let Some(user_msg) = sent_this_turn.iter_mut().find(|m| m.role == "user") {
                                user_msg.content.push_str(&block);
                            }
                        }
                    }
                }
            }

            // Stop clicked during (or between) the pre-stream phases: end the
            // turn before any request goes upstream. Covers the race where a
            // phase future resolves before the 120ms stop poll notices.
            if stop.load(Ordering::Relaxed) {
                abort_lane_pre_stream(&channel, &session_id, lane, &first_message_id).await;
                return;
            }

            let max_rounds = if goal_mode { GOAL_MAX_TOOL_ROUNDS } else { MAX_TOOL_ROUNDS };
            for _round in 1..=max_rounds {
                let body = chat::build_body(
                    &provider,
                    &model,
                    &owned_prefix,
                    &sent_this_turn,
                    &system_full,
                    tools_schema.as_ref(),
                );
                let prefix_bytes = owned_prefix.prefix_bytes_public();
                // tail bytes measured exactly like the request: serialized
                // message JSON + separator — same units as prefix_bytes
                let added_bytes = sent_this_turn.iter().map(|m| message_json(m).len() + 1).sum::<usize>();
                let total_bytes = prefix_bytes + added_bytes;
                // digest-chain continuity: total bytes sent must grow
                // monotonically (append-only) within an epoch; a new epoch is
                // an expected rebuild, not a break
                let chain_ok = {
                    let mut guard = LAST_SPAN.lock().unwrap();
                    let map = guard.get_or_insert_with(HashMap::new);
                    let prev = map.insert((session_id.clone(), lane), (owned_prefix.epoch, total_bytes));
                    match prev {
                        None => true,
                        Some((prev_epoch, prev_total)) => {
                            prev_epoch != owned_prefix.epoch || total_bytes >= prev_total
                        }
                    }
                };

                let message_id = Uuid::new_v4().to_string();
                last_message_id = message_id.clone();
                let ctx = SendCtx {
                    client: &client,
                    provider: &provider,
                    model: &model,
                    lane,
                    message_id: message_id.clone(),
                    channel: channel.clone(),
                    stop: stop.clone(),
                    progress_tap: None,
                    // pi gating: the affinity header only makes sense while
                    // the tier still routes via prompt_cache_key (tier None
                    // sends no cache marks at all)
                    affinity: if provider.cache_tier() == crate::config::CacheTier::None {
                        None
                    } else {
                        Some(cache_key.clone())
                    },
                };
                let outcome = match chat::stream_lane(&ctx, body, chat::auth_for(&provider)).await {
                    Ok(o) => o,
                    Err(e) => {
                        let _ = channel.send(StreamEvent::Error { lane, message: e });
                        turn_status = "error".into();
                        break;
                    }
                };

                // per-round telemetry + significant-miss analysis (pi-runtime
                // parity): the request legitimately re-bills only its new
                // tail (added_bytes); everything beyond that was supposed to
                // be a cache read. Needs the provider to report cached tokens.
                let usage = outcome.usage.clone();
                let cost = chat::cost_of(&usage, &provider, &model);
                let mut stat = RequestStat {
                    seq: SEQ.fetch_add(1, Ordering::Relaxed),
                    ts: now_ms(),
                    lane,
                    model: model.clone(),
                    epoch: owned_prefix.epoch,
                    prefix_bytes,
                    added_bytes,
                    chain_ok,
                    input_tokens: usage.input,
                    cached_tokens: usage.cached,
                    output_tokens: usage.output,
                    cost_usd: cost,
                    significant_miss: false,
                    rebilled_tokens: 0,
                    rebilled_cost: None,
                    miss_cause: None,
                };
                let epoch_bumped = store
                    .load(&session_id)
                    .ok()
                    .and_then(|sf| sf.telemetry.iter().rev().find(|r| r.lane == lane).map(|r| r.epoch != owned_prefix.epoch))
                    .unwrap_or(false);
                let miss =
                    chat::analyze_cache_miss(&usage, &provider.kind, added_bytes, chain_ok, epoch_bumped);
                stat.miss_cause = Some(miss.cause.to_string());
                if miss.significant {
                    stat.significant_miss = true;
                    stat.rebilled_tokens = miss.rebilled_tokens;
                    stat.rebilled_cost = chat::rebill_cost(miss.rebilled_tokens, &usage, &provider, &model);
                }
                let _ = channel.send(StreamEvent::Usage {
                    lane,
                    message_id: message_id.clone(),
                    usage: usage.clone(),
                    request: stat.clone(),
                });

                // accumulate the turn's reply text for SM branch predicates
                if !turn_text.is_empty() {
                    turn_text.push_str("\n\n");
                }
                turn_text.push_str(&outcome.content);

                // persist this round's assistant record
                let has_tools = !outcome.tool_calls.is_empty();
                // SM parallel fan-out decision: on the clean lane-0 answer of
                // a workflow state whose `parallel` list is configured (and
                // not yet fanned out this turn), synthesize parallel_branch
                // tool calls so the assistant record/transcript stay coherent
                // with the subagent runs that follow — the same wire shape as
                // delegate_subagent, preserving prefix-cache alignment.
                let mut fan_calls: Vec<crate::types_rs::ToolCallWire> = Vec::new();
                if !has_tools
                    && lane == 0
                    && !fanout_done
                    && outcome.status == "ok"
                    && wf.starts_with("sm:")
                {
                    if let Some((def_id, state_name)) = wf[3..].split_once(':') {
                        if let Some((def, st)) = chat::resolve_sm(&cfg.workflows, def_id, state_name) {
                            if !st.terminal {
                                for (i, tgt) in st.parallel.iter().enumerate() {
                                    if def.states.iter().any(|s| &s.name == tgt) {
                                        fan_calls.push(crate::types_rs::ToolCallWire {
                                            id: format!("par-{message_id}-{i}"),
                                            name: "parallel_branch".into(),
                                            arguments: serde_json::json!({ "state": tgt }).to_string(),
                                        });
                                    }
                                }
                                fanout_done = !fan_calls.is_empty();
                            }
                        }
                    }
                }
                let tool_wire: Option<Vec<crate::types_rs::ToolCallWire>> = if has_tools {
                    Some(
                        outcome
                            .tool_calls
                            .iter()
                            .map(|tc| crate::types_rs::ToolCallWire {
                                id: tc.id.clone(),
                                name: tc.name.clone(),
                                arguments: tc.arguments.clone(),
                            })
                            .collect(),
                    )
                } else if !fan_calls.is_empty() {
                    Some(fan_calls.clone())
                } else {
                    None
                };
                let asst_msg = ChatMessage {
                    role: "assistant".into(),
                    content: outcome.content.clone(),
                    tool_calls: tool_wire.as_ref().map(|w| chat::tool_calls_wire_value(w)),
                    tool_call_id: None,
                    images: Vec::new(),
                };
                // privacy: the record keeps the RESTORED text (user reads
                // real values); the live wire message above keeps the model's
                // own bytes (surrogates) so Zone H stays byte-stable — a
                // restart rebuild re-scrubs the restored text to the very
                // same surrogates (deterministic mapping).
                let (restored_content, restored_reasoning) = if privacy_on {
                    (
                        crate::privacy::restore(&session_id, &outcome.content),
                        if outcome.reasoning.is_empty() {
                            None
                        } else {
                            Some(crate::privacy::restore(&session_id, &outcome.reasoning))
                        },
                    )
                } else {
                    (outcome.content.clone(), if outcome.reasoning.is_empty() { None } else { Some(outcome.reasoning.clone()) })
                };
                let record = MessageRecord {
                    id: message_id,
                    lane,
                    role: "assistant".into(),
                    reasoning: restored_reasoning,
                    content: restored_content,
                    ts: next_record_ts(),
                    model: Some(model.clone()),
                    status: outcome.status.clone(),
                    usage: Some(usage),
                    cost_usd: cost,
                    confidence: outcome.confidence,
                    tool_calls: tool_wire,
                    tool_call_id: None,
                    skill_calls: None,
                    workflow: None,
                    images: Vec::new(),
                };
                last_confidence = outcome.confidence;
                {
                    let _guard = save_lock.lock().await;
                    if let Ok(mut sf) = store.load(&session_id) {
                        sf.messages.push(record);
                        // significant-miss notice: a user-facing record that
                        // transcript_for_lane excludes from model context —
                        // the model never sees it, so Zone H stays stable
                        if stat.significant_miss && sf.meta.kind == "chat" {
                            let cost_part = stat
                                .rebilled_cost
                                .map(|c| format!("，冤枉钱 ≈ ${:.4}", c))
                                .unwrap_or_default();
                            let hint = if stat.miss_cause.as_deref() == Some("client") {
                                "本地前缀链断裂——有组件改写了历史请求字节，请检查本轮的模式/上下文变更。"
                            } else {
                                "可能原因：闲置超过缓存 TTL（命中会续期）、上游逐出或网关路由变化。持续对话命中率会回升。"
                            };
                            sf.messages.push(MessageRecord {
                                id: Uuid::new_v4().to_string(),
                                lane,
                                role: "notice".into(),
                                content: format!(
                                    "缓存显著未命中：本轮输入 {} tokens 仅命中 {}，重计费 ≈ {} tokens{}。{}",
                                    stat.input_tokens.unwrap_or(0),
                                    stat.cached_tokens.unwrap_or(0),
                                    stat.rebilled_tokens,
                                    cost_part,
                                    hint
                                ),
                                reasoning: None,
                                ts: next_record_ts(),
                                model: None,
                                status: "ok".into(),
                                usage: None,
                                cost_usd: None,
                                confidence: None,
                                tool_calls: None,
                                tool_call_id: None,
                                skill_calls: None,
                                workflow: None,
                                images: Vec::new(),
                            });
                        }
                        sf.telemetry.push(stat);
                        sf.meta.updated_at = now_ms();
                        let _ = store.save(&sf);
                    }
                }

                if outcome.status != "ok" {
                    turn_status = outcome.status;
                    break;
                }
                if !has_tools {
                    sent_this_turn.push(asst_msg);
                    if fan_calls.is_empty() {
                        turn_status = "ok".into();
                        break;
                    }
                    // ---- SM parallel fan-out: surface the synthesized
                    // parallel_branch calls, run every configured parallel
                    // state concurrently as a subagent, report the results as
                    // tool messages, then loop for a synthesis round (the
                    // model sees its "calls" answered).
                    let def_id_fan = wf[3..].split_once(':').map(|(d, _)| d.to_string()).unwrap_or_default();
                    for fc in &fan_calls {
                        let st_name = serde_json::from_str::<Value>(&fc.arguments)
                            .ok()
                            .and_then(|a| a.get("state").and_then(|t| t.as_str()).map(|s| s.to_string()))
                            .unwrap_or_default();
                        let _ = channel.send(StreamEvent::ToolCall {
                            lane,
                            call_id: fc.id.clone(),
                            name: fc.name.clone(),
                            args: format!("state={st_name}"),
                        });
                    }
                    let mut fan_tasks: Vec<(String, String)> = Vec::new(); // (call_id, task)
                    for fc in &fan_calls {
                        let st_name = serde_json::from_str::<Value>(&fc.arguments)
                            .ok()
                            .and_then(|a| a.get("state").and_then(|t| t.as_str()).map(|s| s.to_string()))
                            .unwrap_or_default();
                        let directive = chat::resolve_sm(&cfg.workflows, &def_id_fan, &st_name)
                            .and_then(|(_, st2)| {
                                let d = st2.directive.trim();
                                if d.is_empty() { None } else { Some(d.to_string()) }
                            })
                            .unwrap_or_default();
                        let task = if directive.is_empty() {
                            format!("【工作流并行分支 · {st_name}】请执行该分支状态的工作流任务，完成后输出结果摘要。")
                        } else {
                            format!("【工作流并行分支 · {st_name}】{directive}\n\n完成后输出结果摘要。")
                        };
                        fan_tasks.push((fc.id.clone(), task));
                    }
                    let mut futs = Vec::new();
                    for (cid, task) in &fan_tasks {
                        futs.push(run_subagent(
                            &client,
                            &data_dir,
                            &session_id,
                            task,
                            None,
                            &binding,
                            &cfg,
                            &channel,
                            lane,
                            cid,
                        ));
                    }
                    let results = futures_util::future::join_all(futs).await;
                    for (fc, res) in fan_calls.iter().zip(results.into_iter()) {
                        let st_name = serde_json::from_str::<Value>(&fc.arguments)
                            .ok()
                            .and_then(|a| a.get("state").and_then(|t| t.as_str()).map(|s| s.to_string()))
                            .unwrap_or_default();
                        let mut result = match res {
                            Ok(summary) => format!("【并行分支 {st_name} 完成】\n\n{summary}"),
                            Err(e) => format!("ERROR: 并行分支 {st_name} 失败: {e}"),
                        };
                        // context-volume: spill before the privacy scrub so
                        // the file keeps real values while the record keeps
                        // the trimmed form the model actually sees
                        result = crate::spill::maybe_spill(
                            &session_id,
                            &fc.name,
                            &result,
                            cfg.settings.spill_max_chars,
                        );
                        if privacy_on {
                            result = crate::privacy::outbound(&session_id, &pseed, &result);
                        }
                        let preview: String =
                            result.lines().next().unwrap_or("").chars().take(120).collect();
                        let _ = channel.send(StreamEvent::ToolResult {
                            lane,
                            call_id: fc.id.clone(),
                            name: fc.name.clone(),
                            result: preview,
                        });
                        let tool_record = MessageRecord {
                            id: Uuid::new_v4().to_string(),
                            lane,
                            role: "tool".into(),
                            content: result.clone(),
                            reasoning: None,
                            ts: next_record_ts(),
                            model: None,
                            status: "ok".into(),
                            usage: None,
                            cost_usd: None,
                            confidence: None,
                            tool_calls: None,
                            tool_call_id: Some(fc.id.clone()),
                            skill_calls: None,
                            workflow: None,
                            images: Vec::new(),
                        };
                        {
                            let _guard = save_lock.lock().await;
                            if let Ok(mut sf) = store.load(&session_id) {
                                sf.messages.push(tool_record);
                                sf.meta.updated_at = now_ms();
                                let _ = store.save(&sf);
                            }
                        }
                        sent_this_turn.push(ChatMessage {
                            role: "tool".into(),
                            content: result,
                            tool_calls: None,
                            tool_call_id: Some(fc.id.clone()),
                            images: Vec::new(),
                        });
                    }
                    continue; // synthesis round: the model summarizes branches
                }

                // ---- execute tools, feed results back ----
                sent_this_turn.push(asst_msg);
                let mut tool_msgs: Vec<ChatMessage> = Vec::new();

                // ---- pre-scan: this round's delegate_subagent calls run
                // concurrently. Futures are lazy: they start when joined at
                // the first delegation's wire position, and results fill back
                // in that same order. Budget is consumed here so an overflow
                // call still reports the cap error at its inline position.
                let mut sub_calls: Vec<(String, String)> = Vec::new(); // (call_id, task)
                let mut sub_profiles: HashMap<String, Option<crate::config::SubagentProfile>> =
                    HashMap::new();
                if tools_on && !plan_mode {
                    for tc in &outcome.tool_calls {
                        if tc.name != "delegate_subagent" || delegations >= MAX_DELEGATIONS_PER_TURN {
                            continue;
                        }
                        let task = serde_json::from_str::<Value>(&tc.arguments)
                            .ok()
                            .and_then(|a| a.get("task").and_then(|t| t.as_str()).map(|s| s.trim().to_string()))
                            .unwrap_or_default();
                        if task.is_empty() {
                            continue; // reported inline below
                        }
                        delegations += 1;
                        sub_profiles.insert(
                            tc.id.clone(),
                            find_subagent_profile(&cfg, &data_dir, &tc.arguments),
                        );
                        sub_calls.push((tc.id.clone(), task));
                    }
                }
                let mut sub_slots: HashMap<String, usize> = HashMap::new();
                let mut sub_futs: Option<Vec<_>> = None;
                if !sub_calls.is_empty() {
                    let mut futs: Vec<_> = Vec::new();
                    for (cid, task) in &sub_calls {
                        sub_slots.insert(cid.clone(), futs.len());
                        futs.push(run_subagent(
                            &client,
                            &data_dir,
                            &session_id,
                            task,
                            sub_profiles.get(cid).and_then(|p| p.as_ref()),
                            &binding,
                            &cfg,
                            &channel,
                            lane,
                            cid,
                        ));
                    }
                    sub_futs = Some(futs);
                }
                let mut sub_results: Option<Vec<Result<String, String>>> = None;

                for tc in &outcome.tool_calls {
                    turn_tools.push(tc.name.clone());
                    // images produced by THIS tool call (take_screenshot):
                    // filenames for the persisted record + the deterministic
                    // synthetic user message that carries the payload
                    let mut rec_images: Vec<String> = Vec::new();
                    let mut img_user_msgs: Vec<ChatMessage> = Vec::new();
                    let args_preview: String = tc.arguments.chars().take(160).collect();
                    let _ = channel.send(StreamEvent::ToolCall {
                        lane,
                        call_id: tc.id.clone(),
                        name: tc.name.clone(),
                        args: args_preview,
                    });
                    let result = if !tools_on {
                        "ERROR: 工具未启用（会话未绑定工作区，或在设置中关闭了 Agent 工具）".to_string()
                    } else if plan_mode {
                        "DENIED: 规划模式下不执行任何工具写入或外部调用——请先输出 ```plan 方案等待用户批准".to_string()
                    } else if let Some((mcp_sid, mcp_tool)) = crate::mcp::split_tool_name(&tc.name) {
                        // ---- MCP tool route: trust flag OR per-call approval ----
                        let cfg2 = config::load(&data_dir);
                        let decision: Result<bool, String> =
                            match cfg2.mcp_servers.iter().find(|s| s.id == mcp_sid) {
                                None => Err("MCP 服务不存在或已移除".into()),
                                Some(server) => {
                                    if server.trusted {
                                        Ok(true)
                                    } else {
                                        match serde_json::from_str::<Value>(&tc.arguments) {
                                            Err(e) => Err(format!("参数不是合法 JSON: {e}")),
                                            Ok(args) => {
                                                let approval_id = Uuid::new_v4().to_string();
                                                let rx = open_approval(&approval_id);
                                                let _ = channel.send(StreamEvent::ApprovalRequest {
                                                    lane,
                                                    approval_id,
                                                    tool: tc.name.clone(),
                                                    path: server.name.clone(),
                                                    preview: format!(
                                                        "调用 MCP 服务「{}」的工具 {mcp_tool}\n参数：{}",
                                                        server.name,
                                                        serde_json::to_string_pretty(&args)
                                                            .unwrap_or_default()
                                                    ),
                                                });
                                                Ok(matches!(
                                                    tokio::time::timeout(
                                                        std::time::Duration::from_secs(APPROVAL_TIMEOUT_SECS),
                                                        rx
                                                    )
                                                    .await,
                                                    Ok(Ok(true))
                                                ))
                                            }
                                        }
                                    }
                                },
                            };
                        match decision {
                            Err(e) => format!("ERROR: {e}"),
                            Ok(false) => "DENIED: 用户拒绝或审批超时——MCP 工具未执行".to_string(),
                            Ok(true) => {
                                let cfg3 = config::load(&data_dir);
                                match cfg3.mcp_servers.iter().find(|s| s.id == mcp_sid) {
                                    None => "ERROR: MCP 服务不存在或已移除".to_string(),
                                    Some(server) => {
                                        match serde_json::from_str::<Value>(&tc.arguments) {
                                            Ok(args) => crate::mcp::global()
                                                .call_tool(server, &mcp_tool, &args)
                                                .await,
                                            Err(e) => format!("ERROR: 参数不是合法 JSON: {e}"),
                                        }
                                    }
                                }
                            }
                        }
                    } else {
                        match serde_json::from_str::<Value>(&tc.arguments) {
                            Err(e) => format!("ERROR: 参数不是合法 JSON: {e}"),
                            Ok(args) => {
                                if tc.name == "delegate_subagent" {
                                    if let Some(&slot) = sub_slots.get(&tc.id) {
                                        // pre-spawned concurrent delegation:
                                        // join once, then read this call's slot
                                        if sub_results.is_none() {
                                            let futs = sub_futs.take().expect("delegation futures");
                                            sub_results = Some(futures_util::future::join_all(futs).await);
                                        }
                                        match &sub_results.as_ref().expect("joined results")[slot] {
                                            Ok(summary) => format!("子任务完成，回报如下：\n\n{summary}"),
                                            Err(e) => format!("ERROR: 子任务失败: {e}"),
                                        }
                                    } else if delegations >= MAX_DELEGATIONS_PER_TURN {
                                        format!("ERROR: 本轮委派已达上限（{MAX_DELEGATIONS_PER_TURN} 个子任务）")
                                    } else {
                                        // pre-scan skipped it ⇒ empty task
                                        "ERROR: task 不能为空".to_string()
                                    }
                                } else if tc.name == "memory_save" || tc.name == "memory_search" {
                                    // vector long-term memory (async: embeddings
                                    // API) — workspace-scoped store in data_dir
                                    let ws = workspace.clone().unwrap_or_default();
                                    if !cfg.settings.vector_memory {
                                        "ERROR: 向量长期记忆未启用（设置 → 向量长期记忆）".to_string()
                                    } else if ws.is_empty() {
                                        "ERROR: 会话未绑定工作区，无法定位记忆存储".to_string()
                                    } else if tc.name == "memory_save" {
                                        let text = args
                                            .get("text")
                                            .and_then(|t| t.as_str())
                                            .unwrap_or("")
                                            .trim()
                                            .to_string();
                                        if text.is_empty() {
                                            "ERROR: text 不能为空".to_string()
                                        } else {
                                            match crate::memvector::remember(&client, &cfg, &data_dir, &ws, &text).await {
                                                Ok(()) => "OK: 已写入长期记忆".to_string(),
                                                Err(e) => format!("ERROR: {e}"),
                                            }
                                        }
                                    } else {
                                        let query = args
                                            .get("query")
                                            .and_then(|t| t.as_str())
                                            .unwrap_or("")
                                            .trim()
                                            .to_string();
                                        let k = args
                                            .get("k")
                                            .and_then(|v| v.as_u64())
                                            .map(|v| (v as usize).clamp(1, 20))
                                            .unwrap_or(5);
                                        if query.is_empty() {
                                            "ERROR: query 不能为空".to_string()
                                        } else {
                                            match crate::memvector::embed(
                                                &client,
                                                &cfg.settings.embeddings_url.as_str(),
                                                &cfg.settings.embeddings_key.as_str(),
                                                &cfg.settings.embeddings_model.as_str(),
                                                &query,
                                            )
                                            .await
                                            {
                                                Err(e) => format!("ERROR: {e}"),
                                                Ok(qv) => {
                                                    let hits = crate::memvector::search(&data_dir, &ws, &qv, k);
                                                    if hits.is_empty() {
                                                        "OK: 记忆库中没有匹配的条目".to_string()
                                                    } else {
                                                        let body = hits
                                                            .iter()
                                                            .map(|(s, t)| format!("- [{s:.3}] {t}"))
                                                            .collect::<Vec<_>>()
                                                            .join("\n");
                                                        format!("OK:\n{body}")
                                                    }
                                                }
                                            }
                                        }
                                    }
                                } else if tc.name == "todo_write" {
                                    // session-scoped task list for the preview
                                    // panel — no workspace access, no approval
                                    match serde_json::from_value::<Vec<TodoItem>>(
                                        args.get("todos").cloned().unwrap_or(Value::Null),
                                    ) {
                                        Err(e) => format!("ERROR: todos 参数不合法: {e}"),
                                        Ok(raw) => {
                                            if raw.is_empty() {
                                                save_todos(&data_dir, &session_id, &[]);
                                                "OK: 任务清单已清空".to_string()
                                            } else if raw.len() > 20 {
                                                "ERROR: 任务数超过 20 项上限".to_string()
                                            } else {
                                                let todos: Vec<TodoItem> = raw
                                                    .into_iter()
                                                    .take(20)
                                                    .map(|mut t| {
                                                        t.text = t.text.trim().chars().take(200).collect();
                                                        if t.text.is_empty() {
                                                            t.text = "（未命名任务）".into();
                                                        }
                                                        match t.status.as_str() {
                                                            "in_progress" | "done" => {}
                                                            _ => t.status = "pending".into(),
                                                        }
                                                        t
                                                    })
                                                    .collect();
                                                let done = todos.iter().filter(|t| t.status == "done").count();
                                                save_todos(&data_dir, &session_id, &todos);
                                                format!("OK: 任务清单已更新（共 {} 项，已完成 {done}）", todos.len())
                                            }
                                        }
                                    }
                                } else if tc.name == "take_screenshot" {
                                    // Computer Use "see" primitive: capture
                                    // the screen, save it into the session's
                                    // attachment dir and attach the image to
                                    // the tool result. Privacy: same
                                    // fail-closed approval gate as write
                                    // tools (auto/grant bypasses the card).
                                    let granted = perm_mode == "auto"
                                        || GRANTS
                                            .lock()
                                            .unwrap()
                                            .as_ref()
                                            .is_some_and(|s| {
                                                s.contains(&grant_key(&session_id, "take_screenshot"))
                                            });
                                    let approved = if granted {
                                        true
                                    } else {
                                        let approval_id = Uuid::new_v4().to_string();
                                        let rx = open_approval(&approval_id);
                                        let _ = channel.send(StreamEvent::ApprovalRequest {
                                            lane,
                                            approval_id: approval_id.clone(),
                                            tool: "take_screenshot".into(),
                                            path: "（整个屏幕）".into(),
                                            preview: crate::agent_tools::approval_preview(
                                                "",
                                                "take_screenshot",
                                                &args,
                                            ),
                                        });
                                        match tokio::time::timeout(
                                            std::time::Duration::from_secs(APPROVAL_TIMEOUT_SECS),
                                            rx,
                                        )
                                        .await
                                        {
                                            Ok(Ok(true)) => true,
                                            _ => false,
                                        }
                                    };
                                    if approved {
                                        match crate::agent_tools::take_screenshot(&data_dir, &session_id) {
                                            Ok((text, shots)) => {
                                                for s in &shots {
                                                    rec_images.push(s.filename.clone());
                                                }
                                                img_user_msgs.push(ChatMessage::with_images(
                                                    "user",
                                                    chat::TOOL_IMAGE_NOTE,
                                                    shots
                                                        .into_iter()
                                                        .map(|s| crate::prefix::ChatImage {
                                                            mime: s.mime,
                                                            b64: s.b64,
                                                            file_ref: None,
                                                        })
                                                        .collect(),
                                                ));
                                                text
                                            }
                                            Err(e) => format!("ERROR: {e}"),
                                        }
                                    } else {
                                        "DENIED: 用户拒绝或审批超时（120 秒）——未截取屏幕".to_string()
                                    }
                                } else if goal_mode
                                    && matches!(
                                        tc.name.as_str(),
                                        "get_goal" | "create_goal" | "update_goal"
                                    )
                                {
                                    handle_goal_tool(&data_dir, &session_id, &tc.name, &args)
                                } else if !crate::agent_tools::is_write_tool(&tc.name) {
                                    // sandbox policy: read tools may be refused
                                    // (delete-class rules don't apply here, but
                                    // deny-listed paths / network lists do)
                                    if let crate::agent_tools::SandboxVerdict::Block(reason) =
                                        crate::agent_tools::sandbox_check(&tc.name, &args)
                                    {
                                        reason
                                    } else {
                                        crate::agent_tools::execute(
                                            workspace.as_deref().unwrap_or(""),
                                            &tc.name,
                                            &args,
                                        )
                                    }
                                } else if perm_mode == "readonly" || plan_mode {
                                    "DENIED: 当前为只读或规划模式，写入工具不可用".to_string()
                                } else {
                                    // sandbox guard: refuse before the approval
                                    // card is ever raised (delete / deny-listed
                                    // paths / denied programs / blocked network)
                                    let verdict = crate::agent_tools::sandbox_check(&tc.name, &args);
                                    if let crate::agent_tools::SandboxVerdict::Block(reason) = &verdict {
                                        reason.clone()
                                    } else {
                                    // privacy: tool arguments may carry
                                    // surrogates the model echoed back — the
                                    // tool must operate on REAL values
                                    let mut exec_args = args.clone();
                                    if privacy_on {
                                        restore_args(&session_id, &pseed, &mut exec_args);
                                    }
                                    // ---- approval gate (fail-closed) ----
                                    // sandbox ForceAsk（需逐次确认名单）强制弹出
                                    // 审批卡，即使自动模式 / 已记住授权；
                                    // 文件白名单的可信路径 keep the auto tier.
                                    let trusted = sb.on
                                        && perm_base == "auto"
                                        && verdict == crate::agent_tools::SandboxVerdict::Allow
                                        && crate::agent_tools::file_paths_trusted(&tc.name, &args);
                                    let granted = trusted
                                        || (verdict != crate::agent_tools::SandboxVerdict::ForceAsk
                                            && (perm_mode == "auto"
                                                || GRANTS
                                                    .lock()
                                                    .unwrap()
                                                    .as_ref()
                                                    .is_some_and(|s| {
                                                        s.contains(&grant_key(&session_id, &tc.name))
                                                    })));
                                    let approved = if granted {
                                        true
                                    } else {
                                        let approval_id = Uuid::new_v4().to_string();
                                        let path = exec_args
                                            .get("path")
                                            .and_then(|p| p.as_str())
                                            .or_else(|| exec_args.get("command").and_then(|c| c.as_str()))
                                            .or_else(|| exec_args.get("from").and_then(|f| f.as_str()))
                                            .unwrap_or("?")
                                            .to_string();
                                        let rx = open_approval(&approval_id);
                                        let _ = channel.send(StreamEvent::ApprovalRequest {
                                            lane,
                                            approval_id: approval_id.clone(),
                                            tool: tc.name.clone(),
                                            path,
                                            preview: crate::agent_tools::approval_preview(
                                                workspace.as_deref().unwrap_or(""),
                                                &tc.name,
                                                &exec_args,
                                            ),
                                        });
                                        // 120s deny — a dropped channel denies too
                                        match tokio::time::timeout(
                                            std::time::Duration::from_secs(APPROVAL_TIMEOUT_SECS),
                                            rx,
                                        )
                                        .await
                                        {
                                            Ok(Ok(true)) => true,
                                            _ => false,
                                        }
                                    };
                                    if approved {
                                        // review-panel capture: before/after
                                        // snapshots of the target file (only
                                        // for file tools — run_command has none)
                                        let rel =
                                            exec_args.get("path").and_then(|p| p.as_str()).unwrap_or("").to_string();
                                        let abs = if rel.is_empty() {
                                            None
                                        } else {
                                            crate::agent_tools::resolve_in_workspace(
                                                workspace.as_deref().unwrap_or(""),
                                                &rel,
                                            )
                                            .ok()
                                        };
                                        // sandbox auto-backup: snapshot the
                                        // existing file BEFORE it is modified
                                        if cfg.settings.sandbox_backup {
                                            if let Some(p) = abs.as_ref().filter(|p| p.is_file()) {
                                                if let Err(e) = backup_snapshot(
                                                    &data_dir,
                                                    &session_id,
                                                    p,
                                                    cfg.settings.sandbox_backup_cap_mb,
                                                ) {
                                                    eprintln!("[sandbox-backup] {e}");
                                                }
                                            }
                                        }
                                        let snap = |p: &std::path::Path| -> Option<String> {
                                            std::fs::read_to_string(p).ok().map(|s| {
                                                s.chars().take(WRITE_LOG_CAP).collect::<String>()
                                            })
                                        };
                                        let before = abs.as_ref().and_then(|p| snap(p));
                                        let mut exec = crate::agent_tools::execute_write(
                                            workspace.as_deref().unwrap_or(""),
                                            &tc.name,
                                            &exec_args,
                                        );
                                        if exec.starts_with("OK") && !rel.is_empty() {
                                            let after = abs.as_ref().and_then(|p| snap(p));
                                            let log = crate::types_rs::WriteLog {
                                                ts: next_record_ts(),
                                                tool: tc.name.clone(),
                                                path: rel,
                                                before,
                                                after,
                                            };
                                            let _guard = save_lock.lock().await;
                                            if let Ok(mut sf) = store.load(&session_id) {
                                                sf.writes.push(log);
                                                if sf.writes.len() > 200 {
                                                    sf.writes.drain(..sf.writes.len() - 200);
                                                }
                                                sf.meta.updated_at = now_ms();
                                                let _ = store.save(&sf);
                                            }
                                        }
                                        // post-write verification hook: run the
                                        // configured command and append its
                                        // report so the model sees verification
                                        // output in this very tool result
                                        if exec.starts_with("OK")
                                            && matches!(
                                                tc.name.as_str(),
                                                "write_file"
                                                    | "edit_file"
                                                    | "apply_patch"
                                                    | "delete_file"
                                                    | "move_path"
                                            )
                                        {
                                            if let (Some(ws), Some(cmd)) = (
                                                workspace.as_deref(),
                                                cfg.settings.post_write_command.as_deref(),
                                            ) {
                                                if let Some(report) =
                                                    crate::agent_tools::post_write_verify(ws, cmd)
                                                {
                                                    exec.push_str(&report);
                                                }
                                            }
                                        }
                                        exec
                                    } else {
                                        "DENIED: 用户拒绝或审批超时（120 秒）——本次写入未执行".into()
                                    }
                                    } // sandbox_check passed
                                }
                            }
                        }
                    };
                    // guardrails: fence untrusted external content (fetched
                    // pages, MCP tool results) as data — opt-in via settings
                    let result = if cfg.settings.guardrails
                        && (tc.name == "web_fetch" || crate::mcp::split_tool_name(&tc.name).is_some())
                        && !result.starts_with("ERROR:")
                        && !result.starts_with("DENIED:")
                    {
                        let source = match crate::mcp::split_tool_name(&tc.name) {
                            Some((sid, tool)) => {
                                let sname = config::load(&data_dir)
                                    .mcp_servers
                                    .iter()
                                    .find(|s| s.id == sid)
                                    .map(|s| s.name.clone())
                                    .unwrap_or_else(|| sid.clone());
                                format!("MCP 服务「{sname}」的 {tool} 结果")
                            }
                            None => format!(
                                "网页 {}",
                                serde_json::from_str::<Value>(&tc.arguments)
                                    .ok()
                                    .and_then(|a| {
                                        a.get("url").and_then(|u| u.as_str()).map(|s| s.to_string())
                                    })
                                    .unwrap_or_default()
                            ),
                        };
                        crate::guard::wrap_untrusted(&source, &result, &cfg.settings.guardrails_extra)
                    } else {
                        result
                    };
                    // context-volume: oversized results spill to disk — the
                    // record (and from there Zone H replay) keeps the trimmed
                    // form; the full output stays readable via read_file at
                    // the locator path
                    let result = crate::spill::maybe_spill(
                        &session_id,
                        &tc.name,
                        &result,
                        cfg.settings.spill_max_chars,
                    );
                    let result_preview: String = result.lines().next().unwrap_or("").chars().take(120).collect();
                    let _ = channel.send(StreamEvent::ToolResult {
                        lane,
                        call_id: tc.id.clone(),
                        name: tc.name.clone(),
                        result: result_preview,
                    });
                    let tool_record = MessageRecord {
                        id: Uuid::new_v4().to_string(),
                        lane,
                        role: "tool".into(),
                        content: result.clone(),
                        reasoning: None,
                        ts: next_record_ts(),
                        model: None,
                        status: "ok".into(),
                        usage: None,
                        cost_usd: None,
                        confidence: None,
                        tool_calls: None,
                        tool_call_id: Some(tc.id.clone()),
                        skill_calls: None,
                        workflow: None,
                        images: rec_images,
                    };
                    {
                        let _guard = save_lock.lock().await;
                        if let Ok(mut sf) = store.load(&session_id) {
                            sf.messages.push(tool_record);
                            sf.meta.updated_at = now_ms();
                            let _ = store.save(&sf);
                        }
                    }
                    // live wire: the record above stores the REAL tool
                    // output (user-readable); the outbound copy is scrubbed
                    // so the model keeps seeing surrogates. Restart rebuilds
                    // produce the same bytes (deterministic mapping).
                    let mut result = result;
                    if privacy_on {
                        result = crate::privacy::outbound(&session_id, &pseed, &result);
                    }
                    tool_msgs.push(ChatMessage {
                        role: "tool".into(),
                        content: result,
                        tool_calls: None,
                        tool_call_id: Some(tc.id.clone()),
                        images: Vec::new(),
                    });
                    tool_msgs.extend(img_user_msgs);
                }
                sent_this_turn.extend(tool_msgs);
                // continue to the next round: the model sees tool results
            }

            // Zone T falls into Zone H for the next user turn
            if turn_status != "error" {
                let mut warm_slot: Option<crate::warmer::WarmSlot> = None;
                {
                    let mut guard = prefixes_lock();
                    if let Some(map) = guard.as_mut() {
                        if let Some(lp) = map.get_mut(&(session_id.clone(), lane)) {
                            for m in &sent_this_turn {
                                lp.append(m);
                            }
                            // cache warmer (opt-in): snapshot the post-append
                            // prefix — exactly the span the next real request
                            // wants cached — and schedule one keepalive probe
                            if turn_status == "ok"
                                && crate::warmer::eligible(
                                    cfg.settings.cache_warmup,
                                    provider.kind.clone(),
                                    provider.cache_tier(),
                                    lp.is_reasoning_bound(),
                                )
                            {
                                warm_slot = Some(crate::warmer::WarmSlot {
                                    prefix: lp.clone(),
                                    model: model.clone(),
                                    tools: tools_schema.clone(),
                                });
                            }
                        }
                    }
                }
                if let Some(slot) = warm_slot {
                    crate::warmer::schedule(client.clone(), provider.clone(), session_id.clone(), lane, slot);
                }
            } else {
                // an errored turn's tail never enters Zone H — drop the
                // span marker so the next request isn't judged against bytes
                // we deliberately discarded
                if let Some(map) = LAST_SPAN.lock().unwrap().as_mut() {
                    map.remove(&(session_id.clone(), lane));
                }
            }

            // RollingMemo (L6 §4.2): rule-based durable-fact extraction from
            // this finished turn — file targets (last action wins), user
            // corrections, error→fix pairs. Pure functions, zero model
            // calls; the memo reaches the context via the injection path on
            // a later turn (rev watermark + rebuild branch).
            if lane == 0 && turn_status == "ok" {
                let _guard = save_lock.lock().await;
                if let Ok(mut s) = store.load(&session_id) {
                    let mut memo = s.meta.rolling_memo.clone().unwrap_or_default();
                    if rolling_memo_apply(&mut memo, &sent_this_turn) {
                        s.meta.rolling_memo = Some(memo);
                        s.meta.rolling_memo_rev += 1;
                        s.meta.updated_at = now_ms();
                        let _ = store.save(&s);
                    }
                }
            }

            // State-machine auto-advance: only lane 0 (the shared user lane —
            // arena side-lanes must not move the machine), only on a fully ok
            // turn, and only from the state this turn actually ran under.
            // Terminal states stop; a state without `next` holds position
            // (the user can jump manually from the progress bar).
            if lane == 0 && turn_status == "ok" && wf.starts_with("sm:") {
                if let Some((def_id, state_name)) = wf[3..].split_once(':') {
                    if let Some((def, st)) = chat::resolve_sm(&cfg.workflows, def_id, state_name) {
                        if !st.terminal {
                            // conditional branches first: the FIRST matching
                            // rule wins (evaluated against this turn's reply
                            // text, tools used, and status); fall back to
                            // `next` when no rule matches.
                            let hit = st
                                .branches
                                .iter()
                                .find(|b| {
                                    chat::eval_when(&b.when, &turn_text, &turn_tools, &turn_status)
                                })
                                .map(|b| b.goto.clone());
                            let target = hit.or_else(|| st.next.clone());
                            if let Some(nx) = target {
                                if def.states.iter().any(|s| s.name == nx) {
                                    let gate = format!("sm:{def_id}:{nx}");
                                    sm_put(&session_id, &gate);
                                    // checkpoint so a restart resumes the
                                    // machine at the auto-advanced state
                                    persist_gate(&data_dir, &session_id, Some(&gate));
                                }
                            }
                        }
                    }
                }
            }

            // Auto-reflection gate decided BEFORE Done moves turn_status.
            let do_reflect = lane == 0
                && turn_status == "ok"
                && cfg.settings.vector_memory
                && cfg.settings.auto_reflect
                && !cfg.settings.embeddings_url.trim().is_empty();

            // Goal iteration timeline (ZCode parity): one snapshot per
            // completed turn while the goal gate is active. Title = first
            // pending criterion, so the round list reads as the task's
            // progression story. Capped at the last 60 entries.
            if goal_mode && lane == 0 && turn_status == "ok" && sf_snapshot.meta.goal.is_some() {
                let (ok, total, done, claimed) = parse_goal_summary_ext(&turn_text);
                let pending: String = turn_text
                    .lines()
                    .find(|l| l.trim_start().starts_with('⬜'))
                    .map(|l| {
                        l.trim_start()
                            .trim_start_matches('⬜')
                            .trim()
                            .chars()
                            .take(48)
                            .collect::<String>()
                    })
                    .unwrap_or_default();
                let title = if !pending.is_empty() {
                    pending
                } else if done {
                    "已申报 GOAL_DONE，等待校验收尾".to_string()
                } else {
                    "本轮清单全部达成".to_string()
                };
                let _guard = save_lock.lock().await;
                if let Ok(mut sfr) = store.load(&session_id) {
                    let round = sfr.meta.goal_rounds.len() as u32 + 1;
                    sfr.meta.goal_rounds.push(crate::types_rs::GoalRound {
                        round,
                        title,
                        done: ok,
                        total,
                        claimed,
                        ts: now_ms(),
                    });
                    if sfr.meta.goal_rounds.len() > 60 {
                        let overflow = sfr.meta.goal_rounds.len() - 60;
                        sfr.meta.goal_rounds.drain(..overflow);
                    }
                    sfr.meta.updated_at = now_ms();
                    let _ = store.save(&sfr);
                }
            }

            let _ = channel.send(StreamEvent::Done {
                lane,
                message_id: last_message_id,
                status: turn_status,
                confidence: last_confidence,
            });

            // Auto-reflection (#settings.auto_reflect): on a successful main
            // lane turn, distill durable facts in the background and write
            // them into vector memory. Fully async + silent: never blocks or
            // errors the finished turn.
            if do_reflect {
                let ws = sf_snapshot.meta.workspace.clone().unwrap_or_default();
                let user_text = sf_snapshot
                    .messages
                    .iter()
                    .rev()
                    .find(|m| m.role == "user")
                    .map(|m| m.content.clone())
                    .unwrap_or_default();
                if !ws.is_empty() && !user_text.is_empty() && !turn_text.trim().is_empty() {
                    let client = client.clone();
                    let cfg2 = cfg.clone();
                    let binding2 = binding.clone();
                    let data_dir2 = data_dir.clone();
                    let reply_text = turn_text.clone();
                    tauri::async_runtime::spawn(async move {
                        if let Some(provider) = resolve_provider(&cfg2, &binding2) {
                            crate::memvector::reflect_and_remember(
                                &client,
                                &cfg2,
                                provider,
                                &binding2.model,
                                &user_text,
                                &reply_text,
                                &data_dir2,
                                &ws,
                            )
                            .await;
                        }
                    });
                }
            }
        });
        handles.push(handle);
    }

    for h in handles {
        let _ = h.await;
    }
    state.stops.lock().unwrap().remove(&session_id);

    // AuxMemo application: auto-title (whitelist kind "title"). Fires once
    // per session when the first exchange finished; served from the exact
    // cache when the same first exchange was titled before.
    if !arena {
        let cfg = config::load(&state.data_dir);
        let client = state.client.clone();
        let data_dir = state.data_dir.clone();
        let sid = session_id.clone();
        tauri::async_runtime::spawn(async move {
            let store = SessionStore::new(&data_dir);
            let Ok(sf) = store.load(&sid) else { return };
            if sf.meta.title != "新会话" || sf.messages.len() < 2 {
                return;
            }
            // image sessions: the binding is an image model — chat title-gen
            // would call chat/completions with it; derive from the prompt
            if workflow_of_in(&sid, &data_dir) == "image" {
                let t: String = sf
                    .messages
                    .iter()
                    .find(|m| m.role == "user")
                    .map(|m| m.content.clone())
                    .unwrap_or_default()
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(16)
                    .collect();
                if !t.is_empty() {
                    let _ = store.rename(&sid, &t);
                }
                return;
            }
            let Some(binding) = sf.meta.bindings.first().cloned() else { return };
            let Some(provider) = resolve_provider(&cfg, &binding) else { return };
            let provider = provider.clone();
            let ns = crate::auxmemo::namespace(sf.meta.workspace.as_deref());
            let u = sf.messages.iter().find(|m| m.role == "user").map(|m| m.content.clone()).unwrap_or_default();
            let a = sf.messages.iter().find(|m| m.role == "assistant").map(|m| m.content.clone()).unwrap_or_default();
            let clip = |s: String, n: usize| s.chars().take(n).collect::<String>();
            let input = format!("用户: {}\n\n助手: {}", clip(u, 600), clip(a, 600));
            let key = crate::auxmemo::compute_key("title", &binding.model, &provider.base_url, &input, &ns);
            let title = match crate::auxmemo::get(&data_dir, &key, &ns) {
                Some((e, origin)) => {
                    crate::auxmemo::record_hit(&data_dir, "title", &e, origin);
                    e.text
                }
                None => {
                    let Ok(outcome) = chat::complete_once(
                        &client,
                        &provider,
                        &binding.model,
                        "你是标题生成器。根据对话节选生成一个不超过 12 个字的中文标题，概括主题。只输出标题本身，不要引号、句号或任何前后缀。",
                        &input,
                    )
                    .await
                    else {
                        return; // silent: titles are best-effort
                    };
                    let cost = chat::cost_of(&outcome.usage, &provider, &binding.model);
                    let t: String = outcome
                        .text
                        .trim()
                        .chars()
                        .filter(|c| !c.is_whitespace() && *c != '"' && *c != '“' && *c != '”')
                        .take(24)
                        .collect();
                    if t.is_empty() {
                        return;
                    }
                    crate::auxmemo::put(
                        &data_dir,
                        &key,
                        &ns,
                        &crate::auxmemo::EntryMeta {
                            text: t.clone(),
                            model: binding.model.clone(),
                            in_tok: outcome.usage.input,
                            out_tok: outcome.usage.output,
                            cost_usd: cost,
                        },
                    );
                    crate::auxmemo::record_miss(
                        &data_dir,
                        "title",
                        &binding.model,
                        outcome.usage.input,
                        outcome.usage.output,
                        cost,
                    );
                    t
                }
            };
            let _ = store.rename(&sid, &title);
        });
    }
    Ok(())
}

// ---------- Review panel ----------

#[derive(Serialize)]
pub struct WriteLogEntry {
    pub ts: u64,
    pub tool: String,
    pub path: String,
}

/// Metadata list of this session's successful workspace writes (newest
/// first). Full before/after snapshots are fetched per entry on demand.
#[tauri::command]
pub fn list_session_writes(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<Vec<WriteLogEntry>, String> {
    let sf = state.store.load(&session_id)?;
    Ok(sf
        .writes
        .iter()
        .rev()
        .map(|w| WriteLogEntry { ts: w.ts, tool: w.tool.clone(), path: w.path.clone() })
        .collect())
}

#[tauri::command]
pub fn get_write_diff(
    state: State<'_, AppState>,
    session_id: String,
    ts: u64,
) -> Result<Option<crate::types_rs::WriteLog>, String> {
    let sf = state.store.load(&session_id)?;
    Ok(sf.writes.into_iter().find(|w| w.ts == ts))
}

/// Aggregate +/- line changes over every logged write of a session
/// (ZCode-style change meter for the chat header). Per write: a fresh file
/// (no `before`) counts all lines as added; otherwise a multiset line diff
/// approximates added/removed without a full O(n²) LCS — accurate enough
/// for a header badge, cheap for truncated snapshots. Returns None when the
/// session has no writes (no badge).
#[tauri::command]
pub fn session_change_lines(state: State<'_, AppState>, session_id: String) -> Result<Option<(usize, usize)>, String> {
    use std::collections::HashMap;
    fn multiset_change(before: &str, after: &str) -> (usize, usize) {
        let mut counts: HashMap<&str, i64> = HashMap::new();
        for l in before.lines() {
            *counts.entry(l).or_insert(0) += 1;
        }
        for l in after.lines() {
            *counts.entry(l).or_insert(0) -= 1;
        }
        let (mut added, mut removed) = (0usize, 0usize);
        for v in counts.values() {
            if *v > 0 {
                removed += *v as usize; // lines present only in `before`
            } else if *v < 0 {
                added += (-*v) as usize; // lines present only in `after`
            }
        }
        (added, removed)
    }
    let sf = state.store.load(&session_id)?;
    if sf.writes.is_empty() {
        return Ok(None);
    }
    let mut total = (0usize, 0usize);
    for w in &sf.writes {
        let (a, r) = match (&w.before, &w.after) {
            (None, Some(after)) => (after.lines().count(), 0), // fresh file
            (Some(before), Some(after)) => multiset_change(before, after),
            _ => (0, 0),
        };
        total.0 += a;
        total.1 += r;
    }
    Ok(Some(total))
}

// ---------- worktree isolation ----------

/// One changed file inside the session's isolation worktree.
#[derive(Serialize)]
pub struct WtFileInfo {
    /// Porcelain status letter: M/A/D/R/U/?.
    pub status: String,
    /// Workspace-relative path.
    pub path: String,
}

/// Isolation status for the composer capsule: the active WtState (if any)
/// plus the worktree's changed-file list (badge count).
#[derive(Serialize)]
pub struct WtInfo {
    pub wt: Option<crate::types_rs::WtState>,
    pub files: Vec<WtFileInfo>,
}

/// Enable worktree isolation: create a git worktree on its own branch off
/// the workspace's current HEAD. Every agent tool is redirected into it
/// until the user merges (wt_merge) or discards (wt_discard). The main
/// workspace must be clean first, so a later `git apply` can't collide.
#[tauri::command]
pub fn wt_start(state: State<'_, AppState>, session_id: String) -> Result<crate::types_rs::WtState, String> {
    let sf = state.store.load(&session_id)?;
    if sf.meta.wt.is_some() {
        return Err("该会话已处于 worktree 隔离中".into());
    }
    let Some(ws) = sf.meta.workspace.clone() else {
        return Err("会话未绑定工作区 —— 请先绑定一个目录再开启隔离".into());
    };
    if !crate::worktree::is_git_repo(&ws) {
        return Err("工作区不是 git 仓库 —— 请先在目录中执行 git init 并提交".into());
    }
    if !crate::worktree::is_clean(&ws) {
        return Err("主工作区有未提交的改动 —— 请先提交或 stash 再开启隔离".into());
    }
    let st = crate::worktree::create(&ws, &state.data_dir, &session_id)?;
    state.store.set_wt(&session_id, Some(st.clone()))?;
    Ok(st)
}

/// Read-only isolation status: active WtState + changed-file list.
#[tauri::command]
pub fn wt_info(state: State<'_, AppState>, session_id: String) -> WtInfo {
    let wt = state.store.load(&session_id).ok().and_then(|sf| sf.meta.wt);
    let files = match &wt {
        Some(w) => crate::worktree::changed_files(w)
            .unwrap_or_default()
            .into_iter()
            .map(|(status, path)| WtFileInfo { status, path })
            .collect(),
        None => Vec::new(),
    };
    WtInfo { wt, files }
}

/// Full plain-text diff of the isolation branch vs its base commit
/// (stages untracked files first; binary files show as "differ" lines).
#[tauri::command]
pub fn wt_diff(state: State<'_, AppState>, session_id: String) -> Result<String, String> {
    let wt = state
        .store
        .load(&session_id)?
        .meta
        .wt
        .ok_or("当前未开启 worktree 隔离")?;
    crate::worktree::diff_text(&wt)
}

/// Merge the isolation branch back into the main workspace as working-tree
/// edits (git apply of the full binary diff), then remove worktree + branch.
/// Fail-closed: on any apply error the worktree is kept untouched.
#[tauri::command]
pub fn wt_merge(state: State<'_, AppState>, session_id: String) -> Result<String, String> {
    let sf = state.store.load(&session_id)?;
    let ws = sf.meta.workspace.clone().ok_or("会话未绑定工作区")?;
    let wt = sf.meta.wt.ok_or("当前未开启 worktree 隔离")?;
    let summary = crate::worktree::merge(&wt, &ws)?;
    state.store.set_wt(&session_id, None)?;
    Ok(summary)
}

/// Discard the isolation branch and worktree — every change made inside the
/// worktree is thrown away (the frontend confirms before calling this).
#[tauri::command]
pub fn wt_discard(state: State<'_, AppState>, session_id: String) -> Result<(), String> {
    let sf = state.store.load(&session_id)?;
    let ws = sf.meta.workspace.clone().ok_or("会话未绑定工作区")?;
    let wt = sf.meta.wt.ok_or("当前未开启 worktree 隔离")?;
    crate::worktree::discard(&wt, &ws)?;
    state.store.set_wt(&session_id, None)?;
    Ok(())
}

// ---------- AuxMemo: prompt enhancement (whitelist kind "enhance") ----------

#[derive(Serialize)]
pub struct EnhanceOutcome {
    pub text: String,
    /// "l1" | "l2" | "miss"
    pub origin: String,
    pub model: String,
}

const ENHANCE_INPUT_CAP: usize = 4_000;
const ENHANCE_SYSTEM: &str = "你是提示词工程师。把用户的草稿改写为一条更清晰的最终提示词：补全缺失的上下文与目标、明确输出要求与格式、去除口语化和冗余；但不得添加草稿中不存在的新事实或新要求，保持用户原意与原语言。只输出改写后的提示词本身，不要任何解释、引号或前后缀。";

/// Enhance the composer draft with the session's bound model. Whitelisted
/// for AuxMemo: input fully determines output, no side effects, the result
/// never enters model context without an explicit user "apply" in the UI.
#[tauri::command]
pub async fn enhance_prompt(
    state: State<'_, AppState>,
    session_id: String,
    draft: String,
) -> Result<EnhanceOutcome, String> {
    let draft: String = draft.trim().chars().take(ENHANCE_INPUT_CAP).collect();
    if draft.is_empty() {
        return Err("草稿为空".into());
    }
    let sf = state.store.load(&session_id)?;
    let binding = sf.meta.bindings.first().cloned().ok_or("会话未绑定模型")?;
    let cfg = config::load(&state.data_dir);
    let provider = resolve_provider(&cfg, &binding).cloned().ok_or("Provider 未配置或未填 API Key")?;
    let ns = crate::auxmemo::namespace(sf.meta.workspace.as_deref());
    let key = crate::auxmemo::compute_key("enhance", &binding.model, &provider.base_url, &draft, &ns);

    if let Some((e, origin)) = crate::auxmemo::get(&state.data_dir, &key, &ns) {
        crate::auxmemo::record_hit(&state.data_dir, "enhance", &e, origin);
        return Ok(EnhanceOutcome { text: e.text, origin: origin.as_str().into(), model: e.model });
    }

    let outcome = chat::complete_once(&state.client, &provider, &binding.model, ENHANCE_SYSTEM, &draft).await?;
    let text = outcome.text.trim().to_string();
    if text.is_empty() {
        return Err("增强结果为空".into());
    }
    let cost = chat::cost_of(&outcome.usage, &provider, &binding.model);
    crate::auxmemo::put(
        &state.data_dir,
        &key,
        &ns,
        &crate::auxmemo::EntryMeta {
            text: text.clone(),
            model: binding.model.clone(),
            in_tok: outcome.usage.input,
            out_tok: outcome.usage.output,
            cost_usd: cost,
        },
    );
    crate::auxmemo::record_miss(
        &state.data_dir,
        "enhance",
        &binding.model,
        outcome.usage.input,
        outcome.usage.output,
        cost,
    );
    Ok(EnhanceOutcome { text, origin: "miss".into(), model: binding.model })
}

/// Aggregated AuxMemo ledger for the telemetry panel (per-kind counters +
/// the most recent rows, newest first).
#[tauri::command]
pub fn get_aux_stats(state: State<'_, AppState>) -> crate::auxmemo::AuxStats {
    crate::auxmemo::stats(&state.data_dir)
}

#[cfg(test)]
mod goal_tests {
    use super::parse_goal_summary_ext as pgs;

    #[test]
    fn parses_last_goal_block_only() {
        let content = "开头一个旧清单 ```goal\n✅ 旧一\n⬜ 旧二\n``` 中间说明，最终交付清单：\n```goal\n✅ 标准一\n⬜ 标准二\n⬜ 标准三\n```";
        assert_eq!(pgs(content), (1, 3, false, 1));
    }

    #[test]
    fn detects_goal_done_marker() {
        let content = "全部完成：\n```goal\n✅ a\n✅ b\nGOAL_DONE\n```";
        assert_eq!(pgs(content), (2, 2, true, 2));
    }

    #[test]
    fn unterminated_block_counts_to_end() {
        // 缺收尾围栏时取块尾之后的所有内容（与实现一致）。
        let content = "```goal\n✅ a\n⬜ b\n";
        assert_eq!(pgs(content), (1, 2, false, 1));
    }

    #[test]
    fn no_block_yields_zeroes() {
        assert_eq!(pgs("没有任何清单"), (0, 0, false, 0));
        assert_eq!(pgs(""), (0, 0, false, 0));
    }

    #[test]
    fn progress_notes_without_marker_not_counted() {
        // 无 ✅/⬜ 前缀的进展说明行不计入总数。
        let content = "```goal\n已完成编译检查\n测试全部通过\n```";
        assert_eq!(pgs(content), (0, 0, false, 0));
    }

    #[test]
    fn claimed_counts_ok_rows_without_inline_evidence() {
        // ✅ 行内无反引号且无（…）括注 = claimed（未验证声明）。
        let content = "```goal\n✅ 实现增量计算\n✅ 通过测试 `cargo test 12 通过`\n✅ 覆盖边界（见 src/prefix.rs）\n⬜ 剩余项\n```";
        assert_eq!(pgs(content), (3, 4, false, 1));
    }

    #[test]
    fn claimed_zero_when_all_rows_evidenced() {
        let content = "```goal\n✅ 修复断裂 `git diff --check`\n✅ 复跑通过（cargo test 全绿）\nGOAL_DONE\n```";
        assert_eq!(pgs(content), (2, 2, true, 0));
    }
}

#[cfg(test)]
mod compact_tests {
    use super::*;

    fn pr(input: f64, cached: f64) -> crate::config::Pricing {
        crate::config::Pricing { input_per_m: input, cached_per_m: cached, output_per_m: 10.0 }
    }

    #[test]
    fn payback_math_matches_estimate_ui() {
        // DeepSeek-ish: input 4, cached 0.5 → spread 3.5
        let p = pr(4.0, 0.5);
        // 100k folded: rewrite 0.0028 vs save 0.05/turn → 1 turn
        assert_eq!(payback_turns(100_000, 800, &p), Some(1));
        // 10k folded: save 0.005 → 0.56 → 1 turn
        assert_eq!(payback_turns(10_000, 800, &p), Some(1));
        // 1600 folded: save 0.0008 → ratio 3.5 → 4 turns
        assert_eq!(payback_turns(1600, 800, &p), Some(4));
    }

    #[test]
    fn payback_none_when_no_spread_or_no_savings() {
        // cached == input → spread 0 → rewrite free → economics undefined
        assert_eq!(payback_turns(100_000, 800, &pr(2.0, 2.0)), None);
        // cached 0 → save 0 → nothing to amortize against
        assert_eq!(payback_turns(100_000, 800, &pr(4.0, 0.0)), None);
    }

    #[test]
    fn gate_opens_without_pricing() {
        assert!(auto_compact_payback_ok(None, 100_000, 800, 4));
    }

    #[test]
    fn gate_blocks_when_folding_saves_nothing() {
        // cached price 0 → every turn saves nothing → never summarize
        assert!(!auto_compact_payback_ok(Some(&pr(4.0, 0.0)), 100_000, 800, 100));
    }

    #[test]
    fn gate_blocks_when_payback_exceeds_expected_life() {
        let p = pr(4.0, 0.5);
        // fresh session (8 turns): life = max(4, 8) = 8
        assert!(auto_compact_payback_ok(Some(&p), 1600, 800, 8));
        // slow payback (19 turns) exceeds fresh-session life
        assert!(!auto_compact_payback_ok(Some(&p), 300, 800, 8));
        // long-lived session (40 turns → life 20) amortizes it
        assert!(auto_compact_payback_ok(Some(&p), 300, 800, 40));
    }
}

#[cfg(test)]
mod memo_tests {
    use super::*;
    use crate::types_rs::{MessageRecord, RollingMemo, ToolCallWire};

    fn call(id: &str, name: &str, path: &str) -> serde_json::Value {
        serde_json::json!([
            {"id": id, "type": "function",
             "function": {"name": name, "arguments": format!("{{\"path\":\"{path}\"}}")}}
        ])
    }

    fn rec(lane: u32, role: &str, content: &str, ts: u64) -> MessageRecord {
        MessageRecord {
            id: uuid::Uuid::new_v4().to_string(),
            lane,
            role: role.into(),
            content: content.into(),
            reasoning: None,
            ts,
            model: None,
            status: "ok".into(),
            usage: None,
            cost_usd: None,
            confidence: None,
            tool_calls: None,
            tool_call_id: None,
            skill_calls: None,
            workflow: None,
            images: Vec::new(),
        }
    }

    #[test]
    fn memo_extracts_files_decisions_and_error_fix() {
        let mut memo = RollingMemo::default();
        let turn = vec![
            ChatMessage::plain("user", "这个不对，改成用 tokio 实现"),
            ChatMessage {
                role: "assistant".into(),
                content: String::new(),
                tool_calls: Some(call("c1", "read_file", "/a/b.rs")),
                tool_call_id: None,
                images: Vec::new(),
            },
            ChatMessage {
                role: "tool".into(),
                content: "fn main() {}".into(),
                tool_calls: None,
                tool_call_id: Some("c1".into()),
                images: Vec::new(),
            },
        ];
        assert!(rolling_memo_apply(&mut memo, &turn));
        assert_eq!(memo.files.get("/a/b.rs").map(String::as_str), Some("read_file"));
        assert!(memo.decisions.iter().any(|d| d.contains("改成用 tokio")));
        // idempotent on replay
        let mut memo2 = memo.clone();
        assert!(!rolling_memo_apply(&mut memo2, &turn));
        assert_eq!(memo, memo2);
    }

    #[test]
    fn memo_records_error_then_fix_in_same_turn() {
        let mut memo = RollingMemo::default();
        let turn = vec![
            ChatMessage::plain("user", "继续"),
            ChatMessage {
                role: "assistant".into(),
                content: String::new(),
                tool_calls: Some(call("c2", "write_file", "/a/c.txt")),
                tool_call_id: None,
                images: Vec::new(),
            },
            ChatMessage {
                role: "tool".into(),
                content: "ERROR: 权限拒绝".into(),
                tool_calls: None,
                tool_call_id: Some("c2".into()),
                images: Vec::new(),
            },
            ChatMessage {
                role: "assistant".into(),
                content: String::new(),
                tool_calls: Some(call("c3", "write_file", "/a/c.txt")),
                tool_call_id: None,
                images: Vec::new(),
            },
            ChatMessage {
                role: "tool".into(),
                content: "written".into(),
                tool_calls: None,
                tool_call_id: Some("c3".into()),
                images: Vec::new(),
            },
        ];
        assert!(rolling_memo_apply(&mut memo, &turn));
        // last action wins, no failure tag on the final entry
        assert_eq!(memo.files.get("/a/c.txt").map(String::as_str), Some("write_file"));
        assert!(memo.errors_fixed.iter().any(|x| x == "/a/c.txt（write_file）"));
    }

    #[test]
    fn memo_render_is_deterministic_and_capped() {
        assert!(render_memo(&RollingMemo::default()).is_none());
        let mut memo = RollingMemo::default();
        memo.goal = Some("完成 L6".into());
        memo.decisions.push("改用 tokio".into());
        memo.files.insert("/a/b.rs".into(), "read_file".into());
        let r1 = render_memo(&memo).unwrap();
        let r2 = render_memo(&memo).unwrap();
        assert_eq!(r1, r2);
        assert!(r1.contains("[会话备忘·自动维护]"));
        assert!(r1.contains("goal: 完成 L6"));
        assert!(r1.contains("- /a/b.rs ← read_file"));
        // cap: 12 fat decisions must not blow the budget
        let mut big = RollingMemo::default();
        for i in 0..12 {
            big.decisions.push(format!("决定{i}:{}", "很长的决定内容".repeat(40)));
        }
        let rendered = render_memo(&big).unwrap();
        assert!(rendered.chars().count() < MEMO_CAP_CHARS + 60);
        assert!(rendered.contains("…[截断]"));
    }

    #[test]
    fn elide_stubs_superseded_stale_outputs_only() {
        let dir = std::env::temp_dir().join(format!("ccharness-elide-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = SessionStore::new(&dir);
        let sf = store.create("chat", vec![crate::types_rs::SessionBinding {
            provider_id: "p".into(),
            model: "m".into(),
        }], "elide test")
        .unwrap();
        let sid = sf.meta.id.clone();
        let big_old = "x".repeat(40_000);
        let big_keep = "y".repeat(30_000);
        let mut s = store.load(&sid).unwrap();
        // old big read of /big.txt (superseded below) → elidable
        s.messages.push(rec(0, "user", "第一轮", 1));
        s.messages.push(MessageRecord {
            tool_calls: Some(vec![ToolCallWire {
                id: "c1".into(),
                name: "read_file".into(),
                arguments: "{\"path\":\"/big.txt\"}".into(),
            }]),
            ..rec(0, "assistant", "", 2)
        });
        s.messages.push(MessageRecord {
            tool_call_id: Some("c1".into()),
            ..rec(0, "tool", &big_old, 3)
        });
        // big read of /keep.txt — never superseded → must stay verbatim
        s.messages.push(MessageRecord {
            tool_calls: Some(vec![ToolCallWire {
                id: "c3".into(),
                name: "read_file".into(),
                arguments: "{\"path\":\"/keep.txt\"}".into(),
            }]),
            ..rec(0, "assistant", "", 4)
        });
        s.messages.push(MessageRecord {
            tool_call_id: Some("c3".into()),
            ..rec(0, "tool", &big_keep, 5)
        });
        // superseding re-read of /big.txt: newer same-target record makes
        // the ts=3 output stale
        s.messages.push(MessageRecord {
            tool_calls: Some(vec![ToolCallWire {
                id: "c2".into(),
                name: "read_file".into(),
                arguments: "{\"path\":\"/big.txt\"}".into(),
            }]),
            ..rec(0, "assistant", "", 6)
        });
        s.messages.push(MessageRecord {
            tool_call_id: Some("c2".into()),
            ..rec(0, "tool", "new content", 7)
        });
        // recent window: last user turn from ts=9 onward
        s.messages.push(rec(0, "assistant", "新一轮开始", 8));
        s.messages.push(rec(0, "user", "第二轮", 9));
        store.save(&s).unwrap();

        let (saved, stubs) = elide_stale_tool_records(&dir, &sid, 24_000, 9);
        assert!(saved > 40_000 - 200);
        assert_eq!(stubs, 1);

        let s2 = store.load(&sid).unwrap();
        let stubbed = s2.messages.iter().find(|m| m.tool_call_id.as_deref() == Some("c1")).unwrap();
        assert!(stubbed.content.starts_with("[已降级] read_file /big.txt @ts=3"));
        assert!(stubbed.content.contains("原输出 40000 字符已过时"));
        // pairing survives (I1): still a tool record on the same call id
        assert_eq!(stubbed.role, "tool");
        let kept = s2.messages.iter().find(|m| m.tool_call_id.as_deref() == Some("c3")).unwrap();
        assert_eq!(kept.content, big_keep);
        // idempotent: second run saves nothing
        assert_eq!(elide_stale_tool_records(&dir, &sid, 24_000, 9), (0, 0));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod bucket_tests {
    use super::*;
    use crate::types_rs::{MessageRecord, SessionBinding};

    fn rec(role: &str, content: &str, ts: u64) -> MessageRecord {
        MessageRecord {
            id: uuid::Uuid::new_v4().to_string(),
            lane: 0,
            role: role.into(),
            content: content.into(),
            reasoning: None,
            ts,
            model: None,
            status: "ok".into(),
            usage: None,
            cost_usd: None,
            confidence: None,
            tool_calls: None,
            tool_call_id: None,
            skill_calls: None,
            workflow: None,
            images: Vec::new(),
        }
    }

    fn big_tool(ts: u64, n: usize) -> MessageRecord {
        MessageRecord {
            tool_call_id: Some(format!("t{ts}")),
            ..rec("tool", &"x".repeat(n), ts)
        }
    }

    #[test]
    fn keep_boundary_respects_budget_but_keeps_newest_turn() {
        let msgs = vec![
            rec("user", "第一轮", 1),
            rec("assistant", "回复一", 2),
            rec("user", "第二轮", 3),
            big_tool(4, 20_000),
            rec("user", "第三轮", 5),
            rec("assistant", "回复三", 6),
        ];
        // tiny budget: only the newest turn fits → boundary at its user ts
        assert_eq!(compute_keep_from_ts(&msgs, 1_000), 5);
        // generous budget: all three turns kept → boundary at the first ts
        assert_eq!(compute_keep_from_ts(&msgs, 1_000_000), 1);
    }

    #[test]
    fn keep_boundary_keeps_at_least_one_turn_even_when_it_alone_exceeds() {
        let msgs = vec![
            rec("user", "第一轮", 1),
            rec("user", "第二轮超大", 2),
            big_tool(3, 50_000),
        ];
        assert_eq!(compute_keep_from_ts(&msgs, 1_000), 2);
    }

    #[test]
    fn summarize_input_covers_fold_range_only_and_prepends_memo() {
        let dir = std::env::temp_dir().join(format!("ccharness-bucket-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = SessionStore::new(&dir);
        let sf = store
            .create("chat", vec![SessionBinding { provider_id: "p".into(), model: "m".into() }], "t")
            .unwrap();
        let sid = sf.meta.id.clone();
        let mut s = store.load(&sid).unwrap();
        s.messages.push(rec("user", "第一轮旧内容", 1));
        s.messages.push(rec("assistant", "旧回复", 2));
        s.messages.push(rec("user", "最新一轮保留", 9));
        store.save(&s).unwrap();
        let sf = store.load(&sid).unwrap();
        let input = summarize_input(&sf, 9, Some("[会话备忘]\nfiles:/n- /a ← read"));
        assert!(input.starts_with("[当前会话备忘]"));
        assert!(input.contains("第一轮旧内容"));
        assert!(!input.contains("最新一轮保留"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
