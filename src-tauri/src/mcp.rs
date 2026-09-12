// MCP (Model Context Protocol) client runtime.
//
// One stdio child process per server (JSON-RPC 2.0 over newline-delimited
// stdin/stdout) or one plain-POST HTTP endpoint. Handshake is lazy on first
// use; discovered tools are cached for the process lifetime. Discovered
// tools are exposed to the model as `mcp__<server_id>__<tool>` through the
// normal tool loop; execution is routed back here by name prefix.

use crate::config::McpServerConfig;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;

pub const CALL_TIMEOUT_SECS: u64 = 60;
const HANDSHAKE_TIMEOUT_SECS: u64 = 15;

/// Per-server override: `timeout_secs > 0` wins over the global default.
fn call_timeout(cfg: &McpServerConfig) -> u64 {
    if cfg.timeout_secs > 0 {
        cfg.timeout_secs
    } else {
        CALL_TIMEOUT_SECS
    }
}

struct ProcEntry {
    /// Shared so request paths can lock an async mutex without holding the
    /// std procs guard across .await (which would make futures non-Send).
    stdin: Option<std::sync::Arc<tokio::sync::Mutex<tokio::process::ChildStdin>>>,
    pending: std::sync::Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    /// Owned server process (stdio only). kill_on_drop is armed at spawn,
    /// so dropping the entry terminates the server even without start_kill.
    child: Option<tokio::process::Child>,
    /// OpenAI-format tool definitions contributed by this server.
    tools: Vec<Value>,
    /// Cached state for the status page: "ok" or "error: …".
    state: String,
}

pub struct McpManager {
    procs: Mutex<Option<HashMap<String, ProcEntry>>>,
    next_id: AtomicU64,
}

/// Process-global manager — spawned lane tasks cannot reach tauri::State,
/// and this app is single-window/single-engine by design.
static GLOBAL: McpManager = McpManager { procs: Mutex::new(None), next_id: AtomicU64::new(1) };

pub fn global() -> &'static McpManager {
    &GLOBAL
}

impl Default for McpManager {
    fn default() -> Self {
        Self { procs: Mutex::new(None), next_id: AtomicU64::new(1) }
    }
}

pub fn tool_name(server_id: &str, tool: &str) -> String {
    format!("mcp__{server_id}__{tool}")
}

/// Parse `mcp__<server>__<tool>` back into its parts.
pub fn split_tool_name(name: &str) -> Option<(String, String)> {
    let rest = name.strip_prefix("mcp__")?;
    let (sid, tool) = rest.split_once("__")?;
    if sid.is_empty() || tool.is_empty() {
        return None;
    }
    Some((sid.to_string(), tool.to_string()))
}

impl McpManager {
    /// Ensure a server is spawned + handshaked + tool list cached. Safe to
    /// call repeatedly; returns the number of tools on success.
    /// Spawn + insert if the entry is missing or provably dead. The
    /// dead-entry sweep (try_wait reap / error-entry respawn) lives here so
    /// the handshake retry path reuses the exact same logic.
    fn spawn_if_absent(&self, cfg: &McpServerConfig) -> Result<(), String> {
        let mut guard = self.procs.lock().unwrap();
        let procs = guard.get_or_insert_with(HashMap::new);
        let dead = match procs.get_mut(&cfg.id) {
            Some(e) => match e.child.as_mut() {
                Some(c) => matches!(c.try_wait(), Ok(Some(_))),
                None => e.stdin.is_none() && e.state.starts_with("error:"),
            },
            None => false,
        };
        if dead {
            procs.remove(&cfg.id);
        }
        if procs.contains_key(&cfg.id) {
            return Ok(());
        }
        match spawn_server(cfg) {
            Ok((stdin, pending, child)) => {
                procs.insert(
                    cfg.id.clone(),
                    ProcEntry {
                        stdin,
                        pending,
                        child,
                        tools: Vec::new(),
                        state: "connecting…".into(),
                    },
                );
                Ok(())
            }
            Err(e) => {
                procs.insert(
                    cfg.id.clone(),
                    ProcEntry {
                        stdin: None,
                        pending: std::sync::Arc::new(Mutex::new(HashMap::new())),
                        child: None,
                        tools: Vec::new(),
                        state: format!("error: {e}"),
                    },
                );
                Err(e)
            }
        }
    }

    pub async fn ensure(&self, cfg: &McpServerConfig) -> Result<usize, String> {
        if cfg.transport == "http" {
            if let crate::urlguard::UrlCheck::Refused(msg) =
                crate::urlguard::check_base_url(&cfg.url, cfg.allow_local)
            {
                self.mark_state(&cfg.id, &format!("error: {msg}"));
                return Err(msg);
            }
        }
        self.spawn_if_absent(cfg)?;
        // (re)handshake outside the map lock; one bounded retry: a failed
        // handshake poisons the entry (dead process / broken stdin pipe),
        // so drop it, respawn once and try again. 连接类操作幂等，重试
        // 安全；工具调用不在这里重试（副作用不可重放）。
        let tools = match self.handshake(cfg).await {
            Ok(t) => t,
            Err(first) => {
                {
                    let mut guard = self.procs.lock().unwrap();
                    if let Some(procs) = guard.as_mut() {
                        procs.remove(&cfg.id);
                    }
                }
                self.spawn_if_absent(cfg)?;
                self.handshake(cfg)
                    .await
                    .map_err(|second| format!("{first}；重试仍失败: {second}"))?
            }
        };
        let mut guard = self.procs.lock().unwrap();
        if let Some(procs) = guard.as_mut() {
            if let Some(entry) = procs.get_mut(&cfg.id) {
                entry.tools = tools.clone();
                entry.state = "ok".into();
            }
        }
        Ok(tools.len())
    }

    async fn handshake(&self, cfg: &McpServerConfig) -> Result<Vec<Value>, String> {
        let init = self
            .request(
                cfg,
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": { "name": "CCHarness", "version": env!("CARGO_PKG_VERSION") }
                }),
                HANDSHAKE_TIMEOUT_SECS,
            )
            .await?;
        let _ = init;
        if cfg.transport == "stdio" {
            // initialized notification (no response expected)
            let _ = self.notify(cfg, "notifications/initialized").await;
        }
        let listed = self
            .request(cfg, "tools/list", json!({}), HANDSHAKE_TIMEOUT_SECS)
            .await?;
        let mut out = Vec::new();
        if let Some(arr) = listed.get("tools").and_then(|t| t.as_array()) {
            for t in arr {
                let raw_name = t.get("name").and_then(|n| n.as_str()).unwrap_or_default();
                if raw_name.is_empty() {
                    continue;
                }
                out.push(json!({
                    "type": "function",
                    "function": {
                        "name": tool_name(&cfg.id, raw_name),
                        "description": t.get("description").and_then(|d| d.as_str())
                            .unwrap_or("MCP tool (no description)"),
                        "parameters": t.get("inputSchema").cloned()
                            .unwrap_or(json!({"type":"object","properties":{}})),
                    }
                }));
            }
        }
        Ok(out)
    }

    /// Cached OpenAI-format tool definitions of all connected servers.
    pub fn cached_tools(&self, enabled: &[McpServerConfig]) -> Vec<Value> {
        let guard = self.procs.lock().unwrap();
        let Some(procs) = guard.as_ref() else { return Vec::new() };
        enabled
            .iter()
            .filter(|s| s.enabled)
            .filter_map(|s| procs.get(&s.id))
            .flat_map(|e| e.tools.iter().cloned())
            .collect()
    }

    pub fn status(&self, servers: &[McpServerConfig]) -> Vec<Value> {
        let guard = self.procs.lock().unwrap();
        let procs = guard.as_ref();
        servers
            .iter()
            .map(|s| {
                let entry = procs.and_then(|p| p.get(&s.id));
                json!({
                    "id": s.id,
                    "name": s.name,
                    "enabled": s.enabled,
                    "transport": s.transport,
                    "trusted": s.trusted,
                    "state": entry.map(|e| e.state.clone()).unwrap_or_else(|| "未连接".into()),
                    "tools": entry.map(|e| e.tools.len()).unwrap_or(0),
                })
            })
            .collect()
    }

    pub fn mark_state(&self, id: &str, state: &str) {
        if let Some(procs) = self.procs.lock().unwrap().as_mut() {
            if let Some(e) = procs.get_mut(id) {
                e.state = state.to_string();
            }
        }
    }

    /// Drop a server's process (config removed / disabled).
    pub fn drop_server(&self, id: &str) {
        let mut guard = self.procs.lock().unwrap();
        if let Some(procs) = guard.as_mut() {
            if let Some(mut e) = procs.remove(id) {
                // 显式 start_kill 立即生效；kill_on_drop 只是兜底
                if let Some(c) = e.child.as_mut() {
                    let _ = c.start_kill();
                }
            }
        }
    }

    async fn request(
        &self,
        cfg: &McpServerConfig,
        method: &str,
        params: Value,
        timeout_secs: u64,
    ) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let msg = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        match cfg.transport.as_str() {
            "http" => request_http(cfg, msg, timeout_secs).await,
            _ => self.request_stdio(cfg, id, msg, timeout_secs).await,
        }
    }

    async fn notify(&self, cfg: &McpServerConfig, method: &str) -> Result<(), String> {
        let msg = json!({"jsonrpc":"2.0","method":method});
        if cfg.transport == "http" {
            let _ = request_http(cfg, msg, 5).await;
        } else {
            let line = format!("{}\n", serde_json::to_string(&msg).unwrap());
            let stdin_arc = {
                let guard = self.procs.lock().unwrap();
                guard
                    .as_ref()
                    .and_then(|p| p.get(&cfg.id))
                    .and_then(|e| e.stdin.clone())
                    .ok_or("进程不存在")?
            };
            let mut stdin = stdin_arc.lock().await;
            stdin
                .write_all(line.as_bytes())
                .await
                .map_err(|e| format!("写入失败: {e}"))?;
            stdin.flush().await.map_err(|e| format!("flush 失败: {e}"))?;
        }
        Ok(())
    }

    async fn request_stdio(
        &self,
        cfg: &McpServerConfig,
        id: u64,
        msg: Value,
        timeout_secs: u64,
    ) -> Result<Value, String> {
        let line = format!("{}\n", serde_json::to_string(&msg).unwrap());
        // register the waiter under the short std lock…
        let (tx, rx) = oneshot::channel();
        let (stdin_arc, pending_arc) = {
            let mut guard = self.procs.lock().unwrap();
            let procs = guard.get_or_insert_with(HashMap::new);
            let entry = procs.get_mut(&cfg.id).ok_or("进程不存在")?;
            entry.pending.lock().unwrap().insert(id, tx);
            (entry.stdin.clone(), entry.pending.clone())
        };
        // …then write through the async stdin lock (Send-safe across await)
        match stdin_arc {
            Some(s) => {
                let mut stdin = s.lock().await;
                if let Err(e) = stdin.write_all(line.as_bytes()).await {
                    pending_arc.lock().unwrap().remove(&id);
                    return Err(format!("写入失败: {e}"));
                }
                if let Err(e) = stdin.flush().await {
                    pending_arc.lock().unwrap().remove(&id);
                    return Err(format!("flush 失败: {e}"));
                }
            }
            None => {
                pending_arc.lock().unwrap().remove(&id);
                return Err("进程已退出".into());
            }
        }
        let resp = match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), rx).await
        {
            Ok(r) => r.map_err(|_| "连接关闭".to_string())?,
            Err(_) => {
                // 超时必须摘除等待者：否则 pending 表随失败请求无限增长，
                // 迟到的响应也只会撞上一个早已失效的 sender（S5）
                pending_arc.lock().unwrap().remove(&id);
                return Err("响应超时".to_string());
            }
        };
        if let Some(err) = resp.get("error") {
            return Err(format!("MCP 错误: {err}"));
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Execute one MCP tool call; returns the text content of the result.
    pub async fn call_tool(
        &self,
        cfg: &McpServerConfig,
        tool: &str,
        args: &Value,
    ) -> String {
        let res = self
            .request(
                cfg,
                "tools/call",
                json!({"name": tool, "arguments": args}),
                call_timeout(cfg),
            )
            .await;
        match res {
            Ok(v) => {
                let is_error = v.get("isError").and_then(|e| e.as_bool()).unwrap_or(false);
                let mut text = String::new();
                if let Some(arr) = v.get("content").and_then(|c| c.as_array()) {
                    for part in arr {
                        if part.get("type").and_then(|t| t.as_str()) == Some("text") {
                            if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                                if !text.is_empty() {
                                    text.push_str("\n");
                                }
                                text.push_str(t);
                            }
                        }
                    }
                }
                if text.is_empty() {
                    text = serde_json::to_string_pretty(&v).unwrap_or_default();
                }
                if is_error {
                    format!("ERROR: {}", crate::agent_tools::truncate_public(&text))
                } else {
                    crate::agent_tools::truncate_public(&text)
                }
            }
            Err(e) => format!("ERROR: {e}"),
        }
    }
}

fn spawn_server(
    cfg: &McpServerConfig,
) -> Result<
    (
        Option<std::sync::Arc<tokio::sync::Mutex<tokio::process::ChildStdin>>>,
        std::sync::Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
        Option<tokio::process::Child>,
    ),
    String,
> {
    if cfg.transport != "stdio" {
        // HTTP servers keep no process; pending map exists for symmetry.
        return Ok((None, std::sync::Arc::new(Mutex::new(HashMap::new())), None));
    }
    if cfg.command.trim().is_empty() {
        return Err("stdio 传输需要填写 command".into());
    }
    let mut cmd = tokio::process::Command::new(&cfg.command);
    cmd.args(&cfg.args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        // 进程归 ProcEntry 所有：entry 被 remove/drop 时必须连带终止，
        // 而不是留下孤儿服务器（tokio Child 默认 drop 并不杀进程）
        .kill_on_drop(true);
    // optional working directory — relative paths in args resolve against it
    if !cfg.cwd.trim().is_empty() {
        cmd.current_dir(cfg.cwd.trim());
    }
    // minimal env — provider keys must not leak into MCP servers. A child
    // process inherits the FULL parent environment by default (the comment
    // below used to claim otherwise while the code did nothing): clear it
    // and re-add only what stdio servers commonly need to run (runtimes,
    // temp dirs, locale), then the user-configured vars.
    cmd.env_clear();
    const KEEP: &[&str] = &[
        "PATH",
        "PATHEXT",
        "COMSPEC",
        "SYSTEMROOT",
        "SYSTEMDRIVE",
        "PROGRAMFILES",
        "PROGRAMFILES(X86)",
        "APPDATA",
        "LOCALAPPDATA",
        "TEMP",
        "TMP",
        "USERPROFILE",
        "HOME",
        "USER",
        "TMPDIR",
        "LANG",
        "LC_ALL",
    ];
    for (k, v) in std::env::vars_os() {
        let name = k.to_string_lossy();
        if KEEP.iter().any(|allow| name.eq_ignore_ascii_case(allow)) {
            cmd.env(k, v);
        }
    }
    for (k, v) in &cfg.env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().map_err(|e| format!("启动失败: {e}"))?;
    let stdin = child
        .stdin
        .take()
        .map(|s| std::sync::Arc::new(tokio::sync::Mutex::new(s)));
    let stdout = child.stdout.take().ok_or("无法获取 stdout")?;
    let pending: std::sync::Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> =
        std::sync::Arc::new(Mutex::new(HashMap::new()));
    let pending_reader = pending.clone();
    tokio::spawn(async move {
        let reader = BufReader::new(stdout);
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            let Some(id) = v.get("id").and_then(|i| i.as_u64()) else { continue };
            if let Some(tx) = pending_reader.lock().unwrap().remove(&id) {
                let _ = tx.send(v);
            }
        }
    });
    // child 移交 ProcEntry 持有（此前这里 spawn 的 wait 任务把 Child
    // 永久占走，drop_server 根本杀不到它 —— S4）；自然退出的收割由
    // ensure() 的 try_wait 自愈分支完成
    Ok((stdin, pending, Some(child)))
}

/// Extra request headers for http servers (Authorization, tenant ids, …).
fn headers_of(cfg: &McpServerConfig) -> reqwest::header::HeaderMap {
    let mut map = reqwest::header::HeaderMap::new();
    for (k, v) in &cfg.headers {
        if k.is_empty() || v.is_empty() {
            continue;
        }
        if let (Ok(name), Ok(val)) = (
            reqwest::header::HeaderName::from_bytes(k.as_bytes()),
            reqwest::header::HeaderValue::from_str(v),
        ) {
            map.insert(name, val);
        }
    }
    map
}

async fn request_http(cfg: &McpServerConfig, msg: Value, timeout_secs: u64) -> Result<Value, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        // user-configured endpoints are trusted-ish, but redirects must not
        // quietly walk a request into loopback/private space (P2 SSRF)
        .redirect(reqwest::redirect::Policy::custom(|att| {
            if att.previous().len() > 5 {
                return att.error("重定向过多");
            }
            let host = att.url().host_str().unwrap_or("");
            if crate::urlguard::is_loopback_or_private(host) {
                return att.error("重定向目标位于本机/内网，已拒绝");
            }
            att.follow()
        }))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .post(&cfg.url)
        .headers(headers_of(cfg))
        .json(&msg)
        .send()
        .await
        .map_err(|e| format!("网络错误: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("上游返回 {}", resp.status()));
    }
    let v: Value = resp.json().await.map_err(|e| format!("响应不是 JSON: {e}"))?;
    if let Some(err) = v.get("error") {
        if !err.is_null() {
            return Err(format!("MCP 错误: {err}"));
        }
    }
    Ok(v.get("result").cloned().unwrap_or(Value::Null))
}
