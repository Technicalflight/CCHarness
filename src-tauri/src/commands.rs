// Tauri command layer: the glue between the frontend and the engine.
// All session-file mutations go through one async lock so parallel arena
// lanes cannot interleave read-modify-write cycles.
use crate::chat;
use crate::config::{self, AppConfig, Provider};
use crate::prefix::LanePrefix;
use crate::sessions::{now_ms, SessionStore};
use crate::types_rs::{
    MessageRecord, SessionBinding, SessionMeta, SessionTelemetry, StreamEvent,
    TelemetrySummary,
};
use crate::send_engine::{resolve_provider, run_subagent};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{Manager, State};
use uuid::Uuid;





/// Persist-or-complain: a swallowed save means a reply the user just watched
/// stream by may not survive a restart. Log with session context (P2: these
/// were all bare `let _ =`).
pub(crate) fn warn_save(session_id: &str, what: &str, r: Result<(), String>) {
    if let Err(e) = r {
        eprintln!("[save] {what} 落盘失败 session={session_id}: {e}");
    }
}





// ---- goal lifecycle (Codex /goal parity) ----













/// Guard against runaway tool loops; each round is one provider request.
pub(crate) const MAX_TOOL_ROUNDS: usize = 8;
/// Goal mode locks the result, not the path — allow a longer loop per turn.
pub(crate) const GOAL_MAX_TOOL_ROUNDS: usize = 24;
/// Sub-agent delegations allowed per parent turn (cost bound).
pub(crate) const MAX_DELEGATIONS_PER_TURN: usize = 3;
/// Per-side file snapshot cap for the review panel's write log (chars).
pub(crate) const WRITE_LOG_CAP: usize = 64_000;
/// Approval timeout — fail-closed like every other permission surface here.
pub(crate) const APPROVAL_TIMEOUT_SECS: u64 = 120;

pub(crate) fn grant_key(session_id: &str, tool: &str) -> String {
    format!("{session_id}:{tool}")
}

/// Register a pending approval and return its receiver.
pub(crate) fn open_approval(state: &AppState, id: &str) -> tokio::sync::oneshot::Receiver<bool> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    state
        .approvals
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(id.to_string(), tx);
    rx
}

fn take_approval(state: &AppState, id: &str) -> Option<tokio::sync::oneshot::Sender<bool>> {
    state.approvals.lock().unwrap_or_else(|p| p.into_inner()).as_mut()?.remove(id)
}

/// Monotonic per-record timestamp: equal-ms records keep their order.
pub(crate) fn next_record_ts(state: &AppState) -> u64 {
    loop {
        let now = now_ms();
        let prev = state.last_ts.load(Ordering::Relaxed);
        let next = now.max(prev + 1);
        if state
            .last_ts
            .compare_exchange_weak(prev, next, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return next;
        }
    }
}

pub(crate) fn prefixes_lock(
    state: &AppState,
) -> std::sync::MutexGuard<'_, Option<HashMap<(String, u32), LanePrefix>>> {
    state.prefixes.lock().unwrap_or_else(|p| p.into_inner())
}

/// Zone H empty (only the frozen system prompt, if any) — used to detect a
/// fresh process that must rebuild its prefix from the persisted transcript.
pub(crate) fn lp_is_empty(lp: &LanePrefix) -> bool {
    lp.prefix_bytes_public() <= 0 || lp.history_len_public() == 0
}

pub struct AppState {
    pub data_dir: PathBuf,
    pub store: SessionStore,
    pub client: reqwest::Client,
    /// 流式专用：读空闲超时而非总时长上限 —— 长推理/大输出按分钟计，
    /// client 级 300s 总超时会掐断正常生成（上游 90s 无字节才算死）。
    pub stream_client: reqwest::Client,
    /// Cancellation flags per session.
    pub stops: Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// Serializes session-file read-modify-write across lanes.
    pub save_lock: Arc<tokio::sync::Mutex<()>>,
    /// Lane prefix state — one window, one engine. Key: (session_id, lane).
    pub prefixes: Mutex<Option<HashMap<(String, u32), LanePrefix>>>,
    /// Monotonic request sequence, seeded from the highest seq already
    /// recorded in any session file (old and new records must never share
    /// numbers across restarts).
    pub seq: AtomicU64,
    /// Monotonic per-record timestamp base: equal-ms records keep order.
    pub last_ts: AtomicU64,
    /// Pending write-tool approvals: approval_id → resolver. Dropped
    /// senders simply fail the await (deny, fail-closed).
    pub approvals: Mutex<Option<HashMap<String, tokio::sync::oneshot::Sender<bool>>>>,
    /// Session-level grants: "session_id:tool" remembered via the approval card.
    pub grants: Mutex<Option<std::collections::HashSet<String>>>,
    /// Last total sent bytes per lane for chain-continuity checks, with the
    /// epoch it belonged to (an epoch change is a legitimate rebuild).
    pub last_span: Mutex<Option<HashMap<(String, u32), (u32, usize)>>>,
    /// Session tool-permission mode: "readonly" (write tools not offered),
    /// "approve" (default — write tools gated by the approval card), "auto"
    /// (write tools run without per-execution approval, still workspace-bound).
    pub permissions: Mutex<Option<HashMap<String, String>>>,
    /// Session workflow gate: "agent" (default) | "plan" | "goal" | "deep" |
    /// "review" | "image" | "sm:<def_id>:<state>" (declarative state machine,
    /// see sm_state). In-memory, checkpointed onto SessionMeta.wf_gate so a
    /// restart resumes the same mode instead of falling back to agent.
    pub workflow: Mutex<Option<HashMap<String, String>>>,
    /// Declarative state-machine position per session: session_id → gate
    /// string "sm:<def_id>:<state_name>". Mirrored into workflow (the gate is
    /// the single source of truth for mode checks; sm_state marks that the
    /// session is actively running a state machine and remembers the resolved
    /// position for auto-advance). In-memory — a restart drops back to agent.
    pub sm_state: Mutex<Option<HashMap<String, String>>>,
    /// Set once the user confirmed a real exit (dialog / tray menu). The
    /// CloseRequested handler lets the window close only when this is set.
    pub force_quit: AtomicBool,
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
    // session_id joins a filesystem path — sanitize like sessions::path_for
    // does, so a hostile id ("../../x") cannot escape the todos directory
    let safe = crate::sessions::sanitize_id(session_id);
    data_dir.join("todos").join(format!("{safe}.json"))
}

fn load_todos(data_dir: &std::path::Path, session_id: &str) -> Vec<TodoItem> {
    std::fs::read_to_string(todos_path(data_dir, session_id))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub(crate) fn save_todos(data_dir: &std::path::Path, session_id: &str, todos: &[TodoItem]) {
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
        let stream_client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(20))
            .read_timeout(std::time::Duration::from_secs(90))
            .build()
            .expect("stream http client");
        let store = SessionStore::new(&data_dir);
        // mirror the sandbox policy into the tool guard before any turn runs
        let boot_settings = config::load(&data_dir).settings;
        sync_sandbox_policy(&boot_settings);
        crate::privacy::set_custom_patterns(boot_settings.privacy_custom_patterns.clone());
        // The request sequence must survive restarts: seed the counter from
        // the highest seq already recorded in any session file, or old and
        // new records collide on the same numbers.
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
        Self {
            store,
            data_dir,
            client,
            stream_client,
            stops: Mutex::new(HashMap::new()),
            save_lock: Arc::new(tokio::sync::Mutex::new(())),
            prefixes: Mutex::new(None),
            seq: AtomicU64::new(max_seq + 1),
            last_ts: AtomicU64::new(0),
            approvals: Mutex::new(None),
            grants: Mutex::new(None),
            last_span: Mutex::new(None),
            permissions: Mutex::new(None),
            workflow: Mutex::new(None),
            sm_state: Mutex::new(None),
            force_quit: AtomicBool::new(false),
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
    let mut cfg = config::load(&state.data_dir);
    // the renderer never sees plaintext keys — one invoke would otherwise
    // exfiltrate every provider key through any HTML-injection hole
    for p in &mut cfg.providers {
        p.api_key = config::mask_key(&p.api_key);
    }
    cfg.settings.embeddings_key = config::mask_key(&cfg.settings.embeddings_key);
    cfg
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
    // mask placeholders mean "unchanged" — restore the stored value before
    // sealing; empty = user cleared it, any other value = re-key
    let mut cfg = config;
    let stored = config::load(&state.data_dir);
    for p in &mut cfg.providers {
        if config::is_masked_key(&p.api_key) {
            if let Some(sp) = stored.providers.iter().find(|sp| sp.id == p.id) {
                p.api_key = sp.api_key.clone();
            } else {
                p.api_key = String::new();
            }
        }
    }
    if config::is_masked_key(&cfg.settings.embeddings_key) {
        cfg.settings.embeddings_key = stored.settings.embeddings_key.clone();
    }
    config::save(&state.data_dir, &cfg);
    sync_sandbox_policy(&cfg.settings);
    crate::privacy::set_custom_patterns(cfg.settings.privacy_custom_patterns.clone());
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
pub(crate) fn restore_args(session_id: &str, seed: &[u8; 32], v: &mut Value) {
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
pub(crate) fn backup_snapshot(
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
///     text → openai provider; the TOML's `wire_api` picks the protocol
///     kind ("responses" → openai_responses, "chat"/absent →
///     openai_compatible) — the two endpoints reject each other's calls,
///     so a wrong kind makes every imported key LOOK broken
/// Skips entries without a key. A (base_url, api_key) pair that already
/// exists keeps its stored entry — unless the parsed protocol kind
/// differs, in which case the stored kind is healed in place (re-import
/// repairs entries imported by older builds). Returns what was added.
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
            let (kind, base_url, key, models) = parse_codex_entry(&cfg);
            (kind, base_url, key, models)
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
        // dedupe against existing providers and this batch. If the same
        // (key, endpoint) pair already exists under a different protocol
        // kind (imported before wire_api support), heal the kind in place
        // instead of silently skipping — re-import then repairs old data.
        let mut healed = false;
        for p in config.providers.iter_mut() {
            if p.api_key == api_key && p.base_url == base_url {
                if p.kind != kind {
                    p.kind = kind.clone();
                    healed = true;
                }
                break;
            }
        }
        if healed {
            continue;
        }
        if imported
            .iter()
            .any(|p| p.api_key == api_key && p.base_url == base_url)
        {
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
    // the renderer must never receive plaintext keys (get_config masks for
    // the same reason) — mask the RETURN copy only; the persisted config
    // keeps the real values, sealed
    let mut masked = imported;
    for p in &mut masked {
        p.api_key = config::mask_key(&p.api_key);
    }
    Ok(masked)
}

/// `"...value..."` → `value` (tolerates trailing commas / whitespace).
fn unquote_toml_value(raw: &str) -> String {
    raw.trim()
        .trim_end_matches(',')
        .trim()
        .trim_matches('"')
        .to_string()
}

/// Parse a cc-switch codex entry: `{"auth": {"OPENAI_API_KEY": ...},
/// "config": "<toml text>"}`. The TOML carries the endpoint, the model and
/// — critically — `wire_api`, which decides the protocol kind: Responses
/// endpoints reject chat-completions calls and vice versa, so mapping a
/// "responses" provider to openai_compatible makes every request fail
/// while the key itself is perfectly fine.
fn parse_codex_entry(cfg: &Value) -> (crate::config::ProviderKind, String, String, Vec<String>) {
    let key = cfg
        .pointer("/auth/OPENAI_API_KEY")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let toml_text = cfg.get("config").and_then(|v| v.as_str()).unwrap_or("");
    let mut model = String::new();
    let mut url = String::new();
    let mut wire = String::new();
    for line in toml_text.lines() {
        let line = line.trim();
        if model.is_empty() && line.starts_with("model =") {
            model = unquote_toml_value(line.trim_start_matches("model ="));
        } else if url.is_empty() && line.starts_with("base_url =") {
            url = unquote_toml_value(line.trim_start_matches("base_url ="));
        } else if wire.is_empty() && line.starts_with("wire_api =") {
            wire = unquote_toml_value(line.trim_start_matches("wire_api ="));
        }
    }
    let url = url.trim_end_matches('/').to_string();
    let models = if model.is_empty() { Vec::new() } else { vec![model] };
    let kind = if wire.eq_ignore_ascii_case("responses") {
        crate::config::ProviderKind::OpenaiResponses
    } else {
        crate::config::ProviderKind::OpenaiCompatible
    };
    (kind, url, key, models)
}

fn home_dir() -> std::path::PathBuf {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
}

/// Renderer snapshots carry the ****last4 mask (get_config never ships real
/// keys). For commands that USE a stored provider — test / fetch models —
/// swap the mask back for the stored key, otherwise the literal asterisks
/// go out as the Bearer token and upstream answers 401 Invalid token.
/// New/edited providers (plaintext key just typed) pass through untouched.
fn unmask_provider_key(state: &AppState, mut provider: Provider) -> Provider {
    if config::is_masked_key(&provider.api_key) {
        let cfg = config::load(&state.data_dir);
        if let Some(stored) = cfg.providers.iter().find(|x| x.id == provider.id) {
            provider.api_key = stored.api_key.clone();
        }
    }
    provider
}

#[tauri::command]
pub async fn test_provider(
    state: State<'_, AppState>,
    provider: Provider,
) -> Result<TestResult, String> {
    let provider = unmask_provider_key(&state, provider);
    // same SSRF guard as save_config: these commands hit an arbitrary URL,
    // so loopback/private endpoints need the explicit allow_local consent
    if let crate::urlguard::UrlCheck::Refused(msg) =
        crate::urlguard::check_base_url(&provider.base_url, provider.allow_local)
    {
        return Ok(TestResult { ok: false, message: msg, models: vec![] });
    }
    match chat::fetch_models_async(&models_client(), &provider).await {
        Ok(models) => Ok(TestResult {
            ok: true,
            message: format!("连接成功，{} 个模型可用", models.len()),
            models: models.clone(),
        }),
        Err(e) => Ok(TestResult { ok: false, message: e, models: vec![] }),
    }
}

#[tauri::command]
pub async fn fetch_models(
    state: State<'_, AppState>,
    provider: Provider,
) -> Result<Vec<String>, String> {
    let provider = unmask_provider_key(&state, provider);
    if let crate::urlguard::UrlCheck::Refused(msg) =
        crate::urlguard::check_base_url(&provider.base_url, provider.allow_local)
    {
        return Err(msg);
    }
    chat::fetch_models_async(&models_client(), &provider).await
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

/// 模型列表拉取用的临时 client：connect/total 都有兜底（裸 Client::new
/// 没有任何超时，代理半死时能挂满操作系统级 TCP 超时）。
fn models_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("models http client")
}

/// In-memory and side-file teardown shared by a session and its cascade:
/// prefix lane entries, privacy surrogates, spill files. The session JSON
/// itself is removed by the caller via store.delete.
fn purge_session_traces(state: &AppState, session_id: &str) {
    if let Some(map) = prefixes_lock(state).as_mut() {
        map.retain(|(sid, _), _| sid != session_id);
    }
    // 生命周期随行：内存里的隐私替身库与磁盘上的溢出文件都随会话消亡，
    // 不再无限累积（privacy::forget 此前是死代码，spills 目录从未清理）
    crate::privacy::forget(session_id);
    crate::spill::purge_session(session_id);
}

#[tauri::command]
pub async fn delete_session(state: State<'_, AppState>, session_id: String) -> Result<(), String> {
    // teardown 与在途流式落盘互斥：lane 的整文件写回若排在删除之后会把
    // 会话“复活”。前置的停止守卫（前端先 stop）已收窄窗口，这里串行化
    // 掉剩余竞态。
    let _guard = state.save_lock.lock().await;
    // Cascade (P1-5): kind-"sub" transcripts have no sidebar entry — they
    // are reachable only through their parent, so they die with it. Their
    // prefix entries, privacy surrogates and spills go through the same
    // teardown as the parent.
    for sub in state.store.subs_of(&session_id) {
        purge_session_traces(&state, &sub);
        if let Err(e) = state.store.delete(&sub) {
            eprintln!("delete_session: 级联清理子会话 {sub} 失败: {e}");
        }
    }
    purge_session_traces(&state, &session_id);
    state.store.delete(&session_id)
}

#[tauri::command]
pub async fn rename_session(
    state: State<'_, AppState>,
    session_id: String,
    title: String,
) -> Result<SessionMeta, String> {
    // metadata writes are load-modify-save: hold save_lock so a concurrent
    // stream save that lands mid-write is not silently rolled back by a
    // stale snapshot (P1 — same discipline as update_bindings)
    let _guard = state.save_lock.lock().await;
    state.store.rename(&session_id, &title)
}

#[tauri::command]
pub async fn update_bindings(
    state: State<'_, AppState>,
    session_id: String,
    bindings: Vec<SessionBinding>,
) -> Result<SessionMeta, String> {
    // Lane re-tagging on removal: records carry the lane number they were
    // SENT under. When the list shrinks, current indices shift and older
    // replies would visually migrate onto a different model. Match old →
    // new by (provider_id, model) in order (the UI only removes or
    // appends — anything else is saved as-is) and renumber records so each
    // lane's history stays with its model. Runs under save_lock.
    let _guard = state.save_lock.lock().await;
    let mut sf = state.store.load(&session_id)?;
    let old = sf.meta.bindings.clone();
    let key = |b: &SessionBinding| (b.provider_id.clone(), b.model.clone());
    let mut old_to_new: Vec<Option<u32>> = vec![None; old.len()];
    let mut matched_all = true;
    {
        let mut cursor = 0usize;
        for (new_idx, nb) in bindings.iter().enumerate() {
            let k = key(nb);
            let mut found = None;
            while cursor < old.len() {
                if key(&old[cursor]) == k {
                    found = Some(cursor);
                    cursor += 1;
                    break;
                }
                cursor += 1;
            }
            match found {
                Some(oi) => old_to_new[oi] = Some(new_idx as u32),
                None => {
                    matched_all = false;
                    break;
                }
            }
        }
    }
    if matched_all && old_to_new.iter().any(|m| m.is_none()) {
        // some old lane was removed (its slot maps to nothing) — retag.
        // Removed lanes' records get the sentinel u32::MAX: they must not
        // display under any remaining lane, and must not collide with the
        // shifted indices either.
        let remap = |lane: u32| -> u32 {
            old_to_new.get(lane as usize).copied().flatten().unwrap_or(u32::MAX)
        };
        for m in sf.messages.iter_mut() {
            m.lane = remap(m.lane);
        }
        for r in sf.telemetry.iter_mut() {
            r.lane = remap(r.lane);
        }
    }
    sf.meta.bindings = bindings;
    sf.meta.updated_at = now_ms();
    state.store.save(&sf)?;
    Ok(sf.meta)
}

#[tauri::command]
pub async fn set_workspace(
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
    let _guard = state.save_lock.lock().await;
    state.store.set_workspace(&session_id, workspace)
}

#[tauri::command]
pub async fn set_session_pinned(
    state: State<'_, AppState>,
    session_id: String,
    pinned: bool,
) -> Result<SessionMeta, String> {
    let _guard = state.save_lock.lock().await;
    state.store.set_pinned(&session_id, pinned)
}

#[tauri::command]
pub async fn set_session_archived(
    state: State<'_, AppState>,
    session_id: String,
    archived: bool,
) -> Result<SessionMeta, String> {
    let _guard = state.save_lock.lock().await;
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
    // same size cap as import_scan: a huge file would be read wholesale
    // into memory (OOM) — and under an XSS this command is an arbitrary
    // file-read primitive, so the cap blunts that too
    let meta = std::fs::metadata(&path).map_err(|e| format!("读取文件信息失败: {e}"))?;
    if meta.len() > 64 * 1024 * 1024 {
        return Err(format!(
            "文件过大（{} MB，上限 64 MB）",
            meta.len() / 1024 / 1024
        ));
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
        completed_turns: sf.messages.iter().filter(|m| m.role == "user").count() as u64,
        boost_until_turn: sf.meta.compact_boost_until_turn,
        compactions: sf.meta.compactions.clone(),
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

pub fn is_force_quit(state: &State<'_, AppState>) -> bool {
    state.force_quit.load(std::sync::atomic::Ordering::Relaxed)
}

/// Mark exit-as-confirmed, then close the window for real.
pub fn request_quit(app: &tauri::AppHandle) {
    app.state::<AppState>()
        .force_quit
        .store(true, std::sync::atomic::Ordering::Relaxed);
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
pub async fn clear_session(state: State<'_, AppState>, session_id: String) -> Result<usize, String> {
    // load-modify-save under save_lock — same discipline as rollback; an
    // in-flight lane save landing after the wipe would resurrect content
    let _guard = state.save_lock.lock().await;
    let mut sf = state.store.load(&session_id)?;
    let removed = sf.messages.len();
    sf.messages.clear();
    sf.telemetry.clear();
    sf.compaction = None;
    sf.meta.updated_at = now_ms();
    state.store.save(&sf)?;
    // every lane's prefix and span fingerprint must go — arena/group lanes
    // would otherwise resend the just-deleted history to the provider on
    // the next turn (privacy and correctness both)
    if let Some(map) = prefixes_lock(&state).as_mut() {
        map.retain(|(sid, _), _| sid != &session_id);
    }
    if let Some(map) = state.last_span.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
        map.retain(|(sid, _), _| sid != &session_id);
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
















/// Edit-and-resend support: drop the user message at `from_ts` and everything
/// after it (its replies, tool records, telemetry), and invalidate the prefix
/// state so the next request rebuilds from the trimmed transcript. The
/// load-modify-save runs under save_lock — an in-flight lane save would
/// otherwise be rolled back by the trimmed full-file write.
#[tauri::command]
pub async fn rollback_session(state: State<'_, AppState>, session_id: String, from_ts: u64) -> Result<usize, String> {
    let _guard = state.save_lock.lock().await;
    let mut sf = state.store.load(&session_id)?;
    let before = sf.messages.len();
    sf.messages.retain(|m| m.ts < from_ts);
    let removed = before - sf.messages.len();
    // a rollback point at/before the fold boundary invalidates the summary:
    // it now covers messages that no longer exist — drop it so the next
    // request rebuilds from the full remaining transcript (P2)
    if sf.compaction.as_ref().is_some_and(|c| c.upto_ts >= from_ts) {
        sf.compaction = None;
    }
    sf.telemetry.retain(|r| r.ts < from_ts);
    sf.meta.updated_at = now_ms();
    state.store.save(&sf)?;
    // all lanes, not just lane 0 (see clear_session)
    if let Some(map) = prefixes_lock(&state).as_mut() {
        map.retain(|(sid, _), _| sid != &session_id);
    }
    if let Some(map) = state.last_span.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
        map.retain(|(sid, _), _| sid != &session_id);
    }
    Ok(removed)
}

/// User's answer to a write-tool approval card. `remember` records a
/// session-level grant for that tool (only a session — never global).
#[tauri::command]
pub fn resolve_approval(
    state: State<'_, AppState>,
    approval_id: String,
    session_id: String,
    tool: String,
    approved: bool,
    remember: bool,
) -> Result<(), String> {
    let tx = take_approval(&state, &approval_id).ok_or("审批已不存在（可能已超时）")?;
    let _ = tx.send(approved);
    if approved && remember {
        if let Some(set) = state.grants.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
            set.insert(grant_key(&session_id, &tool));
        }
    }
    Ok(())
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
        &state,
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
pub async fn wt_start(state: State<'_, AppState>, session_id: String) -> Result<crate::types_rs::WtState, String> {
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
    // meta 写入与流式落盘互斥（P1 同类纪律）：只锁元数据写，不横跨上面的
    // 秒级 git 操作 —— save_lock 是全局串行锁，长持锁会卡住所有会话保存
    let _guard = state.save_lock.lock().await;
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
pub async fn wt_merge(state: State<'_, AppState>, session_id: String) -> Result<String, String> {
    let sf = state.store.load(&session_id)?;
    let ws = sf.meta.workspace.clone().ok_or("会话未绑定工作区")?;
    let wt = sf.meta.wt.ok_or("当前未开启 worktree 隔离")?;
    let summary = crate::worktree::merge(&wt, &ws)?;
    // 只锁元数据写，不横跨 git apply（见 wt_start 注释）
    let _guard = state.save_lock.lock().await;
    state.store.set_wt(&session_id, None)?;
    Ok(summary)
}

/// Discard the isolation branch and worktree — every change made inside the
/// worktree is thrown away (the frontend confirms before calling this).
#[tauri::command]
pub async fn wt_discard(state: State<'_, AppState>, session_id: String) -> Result<(), String> {
    let sf = state.store.load(&session_id)?;
    let ws = sf.meta.workspace.clone().ok_or("会话未绑定工作区")?;
    let wt = sf.meta.wt.ok_or("当前未开启 worktree 隔离")?;
    crate::worktree::discard(&wt, &ws)?;
    // 只锁元数据写，不横跨 git 操作（见 wt_start 注释）
    let _guard = state.save_lock.lock().await;
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
mod ccswitch_tests {
    use super::*;

    #[test]
    fn codex_responses_wire_maps_to_responses_kind() {
        let cfg = serde_json::json!({
            "auth": {"OPENAI_API_KEY": "sk-test"},
            "config": "model_provider = \"custom\"\nmodel = \"grok-4.6\"\n\n[model_providers.custom]\nname = \"My Codex\"\nbase_url = \"https://ai.xmiaom.com/v1\"\nwire_api = \"responses\"\nrequires_openai_auth = false\n"
        });
        let (kind, url, key, models) = parse_codex_entry(&cfg);
        assert_eq!(kind, crate::config::ProviderKind::OpenaiResponses);
        assert_eq!(url, "https://ai.xmiaom.com/v1");
        assert_eq!(key, "sk-test");
        assert_eq!(models, vec!["grok-4.6".to_string()]);
    }

    #[test]
    fn codex_chat_wire_and_missing_wire_stay_compatible() {
        let chat = serde_json::json!({
            "auth": {"OPENAI_API_KEY": "sk-a"},
            "config": "[model_providers.custom]\nbase_url = \"https://x/v1\"\nwire_api = \"chat\"\n"
        });
        let (kind, url, _, _) = parse_codex_entry(&chat);
        assert_eq!(kind, crate::config::ProviderKind::OpenaiCompatible);
        assert_eq!(url, "https://x/v1");
        let none = serde_json::json!({
            "auth": {"OPENAI_API_KEY": "sk-b"},
            "config": "model = \"m\"\n"
        });
        let (kind, url, _, models) = parse_codex_entry(&none);
        assert_eq!(kind, crate::config::ProviderKind::OpenaiCompatible);
        assert_eq!(url, "");
        assert_eq!(models, vec!["m".to_string()]);
    }

    #[test]
    fn model_lines_do_not_shadow_the_model_key() {
        // model_provider / model_reasoning_effort must not be picked up as `model =`
        let cfg = serde_json::json!({
            "auth": {"OPENAI_API_KEY": "sk-c"},
            "config": "model_provider = \"custom\"\nmodel_reasoning_effort = \"high\"\nmodel = \"glm-5.3\"\n"
        });
        let (_, _, _, models) = parse_codex_entry(&cfg);
        assert_eq!(models, vec!["glm-5.3".to_string()]);
    }
}
