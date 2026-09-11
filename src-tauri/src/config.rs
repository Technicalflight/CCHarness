// App configuration: typed structures + load/save in the app data dir.
// serde field names are the wire contract with the TS side (src/types.ts).
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    OpenaiCompatible,
    Anthropic,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Pricing {
    pub input_per_m: f64,
    pub cached_per_m: f64,
    pub output_per_m: f64,
}

/// Per-model behavior overrides (keyed by model name alongside Pricing).
/// None / "default" fields mean "fall back to built-in or global behavior".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelBehavior {
    /// Output cap in tokens sent as `max_tokens`. None = provider default
    /// (Anthropic path falls back to 8192, its required minimum).
    #[serde(default)]
    pub max_output: Option<u32>,
    /// Sampling temperature. None = omit the field (provider default).
    #[serde(default)]
    pub temperature: Option<f64>,
    /// Reasoning effort override ("default" = follow the global setting).
    #[serde(default)]
    pub reasoning: Option<String>,
}

impl Default for ModelBehavior {
    fn default() -> Self {
        Self { max_output: None, temperature: None, reasoning: None }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub kind: ProviderKind,
    pub base_url: String,
    pub api_key: String,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub allow_local: bool,
    /// Model context window in tokens (used by the composer usage meter).
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub pricing: std::collections::BTreeMap<String, Pricing>,
    /// Per-model behavior overrides, keyed by model name (like `pricing`).
    #[serde(default)]
    pub behavior: std::collections::BTreeMap<String, ModelBehavior>,
}

fn default_true() -> bool {
    true
}

fn default_close_action() -> String {
    "ask".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppSettings {
    pub theme: String, // "dark" | "light"
    #[serde(default = "default_true")]
    pub send_on_enter: bool,
    #[serde(default)]
    pub system_prompt: String,
    /// Inject the read-only agent tool surface (needs a session workspace).
    #[serde(default = "default_true")]
    pub agent_tools: bool,
    /// Reasoning effort sent as `reasoning_effort` ("default" = omit field).
    #[serde(default)]
    pub thinking_level: String, // "default" | "low" | "medium" | "high"
    /// System notifications when a turn finishes / approval is needed while
    /// the window is hidden.
    #[serde(default = "default_true")]
    pub notify_done: bool,
    /// Window close-button behavior: "ask" (dialog) | "tray" (keep running
    /// in the tray) | "quit" (exit immediately).
    #[serde(default = "default_close_action")]
    pub close_action: String,
    /// Vector long-term memory master switch (memvector store + tools +
    /// per-turn recall injection).
    #[serde(default)]
    pub vector_memory: bool,
    /// OpenAI-compatible embeddings endpoint for vector memory, e.g.
    /// "https://api.openai.com/v1/embeddings" (the /embeddings suffix is
    /// appended when missing).
    #[serde(default)]
    pub embeddings_url: String,
    #[serde(default)]
    pub embeddings_key: String,
    #[serde(default)]
    pub embeddings_model: String,
    /// Automatic post-turn reflection (writes distilled notes into vector
    /// memory; requires vector_memory + an embeddings config).
    #[serde(default)]
    pub auto_reflect: bool,
    /// Prompt-injection guardrails: fence untrusted external content
    /// (fetched web pages, MCP tool results) as data and warn on
    /// injection-style phrases.
    #[serde(default)]
    pub guardrails: bool,
    /// User-defined extra injection patterns (UI: one per line).
    #[serde(default)]
    pub guardrails_extra: Vec<String>,
    /// Goal-mode soft budget (USD, per session): once a session's accumulated
    /// cost reaches it, the next auto-continue nudges the model to wrap up
    /// (progress summary + remaining work) instead of pushing onward.
    /// None = unlimited. Enforced frontend-side (cost data lives in records).
    #[serde(default)]
    pub goal_budget_usd: Option<f64>,
    /// Post-write verification hook (better-harness style feedback loop):
    /// after every successful file-mutating tool (write/edit/apply_patch/
    /// delete/move) the main lane runs this command at the workspace root
    /// and appends its output to the tool result, so the model sees the
    /// verification immediately. None/empty = disabled.
    #[serde(default)]
    pub post_write_command: Option<String>,
    /// 伪匿名化安全模式（pseudonymisation, NOT anonymisation）：所有出站
    /// 文本（用户消息/工具结果/历史重放）先经 privacy.rs 做类型一致的
    /// 替代值替换（HMAC 确定性映射，同会话同值同替身），模型回复写盘前
    /// 还原。映射表只存在本机。可能影响模型结果；并非 100% 安全。默认关。
    #[serde(default)]
    pub privacy_mode: bool,
    /// 伪匿名化·自定义脱敏规则（正则，UI 每行一条）：内置检测覆盖不了的
    /// 自由文本敏感信息（中文姓名、内部代号等）由用户自行补充，命中统一
    /// 替换为 [匿名-xxxxxxxx]，照常进映射日志、可还原。
    #[serde(default)]
    pub privacy_custom_patterns: Vec<String>,
    /// 沙箱模式总开关（默认关）：开启后下列子策略才生效。被拦截的操作
    /// 会以 ERROR 返回给模型——可能影响任务完成度。
    #[serde(default)]
    pub sandbox_mode: bool,
    /// 沙箱·文件安全：删除类工具（delete_file）一律拒绝。
    #[serde(default = "default_true")]
    pub sandbox_files: bool,
    /// 沙箱·命令安全：高危 shell 命令黑名单拦截（rm -rf / del /s / format
    /// / reg / shutdown / taskkill /f / git push --force / git reset --hard
    /// 等）。
    #[serde(default = "default_true")]
    pub sandbox_commands: bool,
    /// 沙箱·网络安全：web_fetch 外网抓取在会话内被拒绝（MCP 不受此控，
    /// 请按需自行关闭 MCP 开关）。
    #[serde(default = "default_true")]
    pub sandbox_network: bool,
    /// 沙箱·自动备份：每次 write/edit/apply_patch 落盘前，把目标文件原样
    /// 快照到 <data_dir>/backups/<session_id>/。
    #[serde(default)]
    pub sandbox_backup: bool,
    /// 自动备份总上限（MB）：超过后从最旧开始清理。
    #[serde(default = "default_backup_cap_mb")]
    pub sandbox_backup_cap_mb: u64,
    /// 沙箱·文件策略：禁止触碰的路径模式（支持 * 通配，不区分大小写，
    /// 优先级最高）。空 = 不做路径级拦截。
    #[serde(default)]
    pub sandbox_file_deny: Vec<String>,
    /// 沙箱·文件策略：可信路径白名单 —— 自动模式下对这些路径的写入不再
    /// 逐条审批（其余写操作仍逐条审批）。
    #[serde(default)]
    pub sandbox_file_allow: Vec<String>,
    /// 沙箱·命令策略：禁止运行的程序名（wsl / wmic / sc / reg / schtasks
    /// 等按名称匹配，命中即拒绝）。
    #[serde(default = "default_cmd_deny")]
    pub sandbox_cmd_deny: Vec<String>,
    /// 沙箱·命令策略：允许运行的程序名 —— 显式放行并跳过内置高危黑名单。
    #[serde(default)]
    pub sandbox_cmd_allow: Vec<String>,
    /// 沙箱·命令策略：需逐次确认的程序名 —— 即使自动模式/已记住授权也
    /// 强制弹出审批卡。
    #[serde(default)]
    pub sandbox_cmd_ask: Vec<String>,
    /// 沙箱·网络策略：禁止访问的域名（含子域名，支持 * 通配）。
    #[serde(default)]
    pub sandbox_net_deny: Vec<String>,
    /// 沙箱·网络策略：允许访问的域名（「阻止所有外部网络」开启时仍放行）。
    #[serde(default)]
    pub sandbox_net_allow: Vec<String>,
    /// 沙箱·网络策略：阻止所有外部网络（允许名单除外）。
    #[serde(default)]
    pub sandbox_net_block_all: bool,
    /// 沙箱·网络策略：恶意域名拦截（内置规则：非标准协议 / 带凭据的
    /// URL / punycode 仿冒域名）。
    #[serde(default = "default_true")]
    pub sandbox_net_malicious: bool,
}

fn default_backup_cap_mb() -> u64 {
    500
}

fn default_cmd_deny() -> Vec<String> {
    ["wsl", "wmic", "sc", "reg", "schtasks"]
        .into_iter()
        .map(String::from)
        .collect()
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            theme: "dark".into(),
            send_on_enter: true,
            system_prompt: String::new(),
            agent_tools: true,
            thinking_level: "default".into(),
            notify_done: true,
            close_action: "ask".into(),
            vector_memory: false,
            embeddings_url: String::new(),
            embeddings_key: String::new(),
            embeddings_model: String::new(),
            auto_reflect: false,
            guardrails: false,
            guardrails_extra: Vec::new(),
            goal_budget_usd: None,
            post_write_command: None,
            privacy_mode: false,
            privacy_custom_patterns: Vec::new(),
            sandbox_mode: false,
            sandbox_files: true,
            sandbox_commands: true,
            sandbox_network: true,
            sandbox_backup: false,
            sandbox_backup_cap_mb: 500,
            sandbox_file_deny: Vec::new(),
            sandbox_file_allow: Vec::new(),
            sandbox_cmd_deny: default_cmd_deny(),
            sandbox_cmd_allow: Vec::new(),
            sandbox_cmd_ask: Vec::new(),
            sandbox_net_deny: Vec::new(),
            sandbox_net_allow: Vec::new(),
            sandbox_net_block_all: false,
            sandbox_net_malicious: true,
        }
    }
}

/// A named subagent profile for `delegate_subagent`: dedicated model binding
/// and role prompt. Enabled profiles are advertised to the parent model via
/// the delegate tool description (`agent` parameter).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SubagentProfile {
    pub id: String,
    pub name: String,
    /// One-line "what is this agent for" shown in the manager + tool schema.
    #[serde(default)]
    pub description: String,
    pub provider_id: String,
    pub model: String,
    /// Extra role instructions appended to the assembled system prompt.
    #[serde(default)]
    pub system_prompt: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Optional tool whitelist (tool names as in the schema). Empty list =
    /// the safe read-only default surface. Non-empty = exactly these tools
    /// (write tools included only when explicitly listed).
    #[serde(default)]
    pub tools: Vec<String>,
    /// Optional cap on tool rounds for this profile (clamped 1..=40).
    /// None = the global SUB_MAX_ROUNDS default.
    #[serde(default)]
    pub max_turns: Option<u32>,
    /// Origin marker: "config" (settings-managed, persisted) or "file"
    /// (<data_dir>/agents/*.md — resolved on the fly, never written back).
    #[serde(default)]
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpServerConfig {
    pub id: String,
    pub name: String,
    /// Optional human note shown in the server list ("what is this for?").
    #[serde(default)]
    pub description: String,
    pub transport: String, // "stdio" | "http"
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub url: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Trusted servers skip the per-call approval card (still honor the
    /// session's readonly mode).
    #[serde(default)]
    pub trusted: bool,
    /// SSRF consent for http endpoints on loopback/private networks.
    #[serde(default)]
    pub allow_local: bool,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// stdio only: working directory for the child process (empty = inherit).
    #[serde(default)]
    pub cwd: String,
    /// Per-server tool-call timeout in seconds (0 = use the global default).
    #[serde(default)]
    pub timeout_secs: u64,
    /// http only: extra request headers (e.g. Authorization) sent with every
    /// JSON-RPC POST.
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MarketSkill {
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub description_zh: Option<String>,
    #[serde(default)]
    pub icon_url: Option<String>,
    #[serde(default)]
    pub downloads: Option<u64>,
    #[serde(default)]
    pub installs: Option<u64>,
    #[serde(default)]
    pub stars: Option<u64>,
    #[serde(default)]
    pub score: Option<f64>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub namespace: Option<serde_json::Value>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub verified: Option<bool>,
}

/// One state of a declarative workflow (state machine). `directive` is
/// injected verbatim ahead of every user message sent while the session sits
/// in this state; `tools` clamps the tool surface ("none" | "readonly" |
/// "full"); `next` names the state the session auto-advances to after a
/// successful lane-0 turn (ignored on terminal states).
/// One ordered conditional-branch rule of a state: the FIRST rule whose
/// `when` predicate matches the finished turn wins, and the machine jumps to
/// `goto`. Predicate language: "" | "always" match everything; "ok"/"error"
/// match the turn status; "contains:<text>" / "not_contains:<text>" match the
/// turn's reply text (case-insensitive); "tool_used:<name>" matches a tool
/// invoked during the turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmBranch {
    #[serde(default)]
    pub when: String,
    pub goto: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmState {
    pub name: String,
    #[serde(default)]
    pub directive: String,
    #[serde(default = "default_sm_tools")]
    pub tools: String, // "none" | "readonly" | "full"
    #[serde(default)]
    pub next: Option<String>,
    #[serde(default)]
    pub terminal: bool,
    /// Ordered conditional branches evaluated before `next` after a clean
    /// lane-0 turn; the first match wins.
    #[serde(default)]
    pub branches: Vec<SmBranch>,
    /// Parallel fan-out: state names to run concurrently as subagents right
    /// after this state's clean lane-0 turn (before branch/next evaluation).
    #[serde(default)]
    pub parallel: Vec<String>,
}

fn default_sm_tools() -> String {
    "full".into()
}

/// Declarative workflow definition: an ordered list of states. states[0] is
/// the entry state a session lands in when the workflow is selected. Managed
/// in the UI, persisted in config.json, referenced by sessions via the
/// in-memory workflow gate value `sm:<id>:<state>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowDef {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub states: Vec<SmState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub version: u32,
    pub providers: Vec<Provider>,
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,
    /// Named subagent profiles for delegate_subagent (managed in the UI).
    #[serde(default)]
    pub subagents: Vec<SubagentProfile>,
    /// Declarative workflows (state machines) — selectable per session as
    /// workflow `sm:<id>`, each turn lands in a state with its own directive
    /// and tool surface, auto-advancing on success.
    #[serde(default)]
    pub workflows: Vec<WorkflowDef>,
    pub settings: AppSettings,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: 1,
            providers: vec![Provider {
                id: "p_deepseek".into(),
                name: "DeepSeek".into(),
                kind: ProviderKind::OpenaiCompatible,
                base_url: "https://api.deepseek.com/v1".into(),
                api_key: String::new(),
                models: vec!["deepseek-chat".into(), "deepseek-reasoner".into()],
                enabled: true,
                allow_local: false,
                context_window: None,
                pricing: std::collections::BTreeMap::new(),
                behavior: std::collections::BTreeMap::new(),
            }],
            mcp_servers: Vec::new(),
            subagents: Vec::new(),
            workflows: Vec::new(),
            settings: AppSettings::default(),
        }
    }
}

pub fn config_path(data_dir: &Path) -> PathBuf {
    data_dir.join("config.json")
}

// ---------- API-key at-rest protection ----------

// Sealed forms stored in config.json:
//   Windows:  `dpapi:v1:<hex>`  — DPAPI (CryptProtectData, user scope)
//   Other OS: `enc:v1:<b64>`    — AES-256-GCM keyed by a 32-byte master key
//                                 held in the OS keyring (macOS Keychain /
//                                 Linux Secret Service via the keyring crate)
// If the OS keyring is unavailable (headless Linux, CI), sealing degrades to
// plaintext (fail open — losing the key would be worse than storing it).
// Unsealing a sealed value that cannot be decrypted returns "" (re-enter the
// key). In memory the key is always plaintext: load() unseals, save() seals.

const KEY_MARK: &str = "dpapi:v1:";
const KEY_MARK_ENC: &str = "enc:v1:";
#[cfg(not(windows))]
const KEYRING_SERVICE: &str = "com.ccharness.app";
#[cfg(not(windows))]
const KEYRING_USER: &str = "api-key-master";

#[cfg(windows)]
fn dpapi_protect(plain: &[u8]) -> Result<Vec<u8>, String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    unsafe {
        let mut input = CRYPT_INTEGER_BLOB {
            cbData: plain.len() as u32,
            pbData: plain.as_ptr() as *mut u8,
        };
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        let ok = CryptProtectData(
            &mut input,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        );
        if ok == 0 {
            return Err("CryptProtectData failed".into());
        }
        let slice = std::slice::from_raw_parts(out.pbData, out.cbData as usize);
        let data = slice.to_vec();
        LocalFree(out.pbData as _);
        Ok(data)
    }
}

#[cfg(windows)]
fn dpapi_unprotect(blob: &[u8]) -> Result<Vec<u8>, String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    unsafe {
        let mut input = CRYPT_INTEGER_BLOB {
            cbData: blob.len() as u32,
            pbData: blob.as_ptr() as *mut u8,
        };
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        let ok = CryptUnprotectData(
            &mut input,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        );
        if ok == 0 {
            return Err("CryptUnprotectData failed".into());
        }
        let slice = std::slice::from_raw_parts(out.pbData, out.cbData as usize);
        let data = slice.to_vec();
        LocalFree(out.pbData as _);
        Ok(data)
    }
}

/// 32-byte master key for AES-GCM sealing, stored in the OS keyring
/// (created on first use). Cached in memory to avoid repeated keyring
/// round-trips within a config load/save.
#[cfg(not(windows))]
fn os_master_key() -> Result<[u8; 32], String> {
    use std::sync::Mutex;
    static CACHE: Mutex<Option<[u8; 32]>> = Mutex::new(None);
    if let Some(k) = CACHE.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
        return Ok(*k);
    }
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
        .map_err(|e| format!("keyring unavailable: {e}"))?;
    let key: [u8; 32] = match entry.get_password() {
        Ok(s) => {
            let bytes = hex::decode(s.trim()).map_err(|e| format!("master key corrupt: {e}"))?;
            if bytes.len() != 32 {
                return Err("master key corrupt: bad length".into());
            }
            let mut k = [0u8; 32];
            k.copy_from_slice(&bytes);
            k
        }
        Err(keyring::Error::NoEntry) => {
            use rand::RngCore;
            let mut k = [0u8; 32];
            rand::thread_rng().fill_bytes(&mut k);
            entry
                .set_password(&hex::encode(k))
                .map_err(|e| format!("keyring write failed: {e}"))?;
            k
        }
        Err(e) => return Err(format!("keyring read failed: {e}")),
    };
    if let Ok(mut c) = CACHE.lock() {
        *c = Some(key);
    }
    Ok(key)
}

/// Seal `plain` with AES-256-GCM under the keyring master key.
/// Blob layout: base64(12-byte nonce || ciphertext+tag).
#[cfg(not(windows))]
fn aes_seal(plain: &str, master: &[u8; 32]) -> Result<String, String> {
    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Nonce};
    use base64::Engine as _;
    use rand::RngCore;
    let cipher = Aes256Gcm::new(master.into());
    let mut nonce = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce);
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce), plain.as_bytes())
        .map_err(|e| format!("encrypt failed: {e}"))?;
    let mut blob = Vec::with_capacity(12 + ct.len());
    blob.extend_from_slice(&nonce);
    blob.extend_from_slice(&ct);
    Ok(format!(
        "{KEY_MARK_ENC}{}",
        base64::engine::general_purpose::STANDARD.encode(blob)
    ))
}

#[cfg(not(windows))]
fn aes_unseal(sealed: &str, master: &[u8; 32]) -> Result<String, String> {
    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Nonce};
    use base64::Engine as _;
    let blob = base64::engine::general_purpose::STANDARD
        .decode(sealed)
        .map_err(|e| format!("bad blob: {e}"))?;
    if blob.len() < 13 {
        return Err("blob too short".into());
    }
    let cipher = Aes256Gcm::new(master.into());
    let pt = cipher
        .decrypt(Nonce::from_slice(&blob[..12]), &blob[12..])
        .map_err(|e| format!("decrypt failed: {e}"))?;
    String::from_utf8(pt).map_err(|e| format!("bad utf8: {e}"))
}

/// Seal a plaintext key for disk storage. Already-sealed and empty keys pass
/// through; on seal failure the plaintext is kept (fail open — losing the
/// key would be worse than storing it).
pub fn protect_api_key(plain: &str) -> String {
    if plain.is_empty() || plain.starts_with(KEY_MARK) || plain.starts_with(KEY_MARK_ENC) {
        return plain.to_string();
    }
    #[cfg(windows)]
    let sealed = dpapi_protect(plain.as_bytes())
        .map(|blob| format!("{KEY_MARK}{}", hex::encode(blob)))
        .unwrap_or_else(|e| {
            eprintln!("[config] api-key seal failed ({e}); storing plaintext");
            plain.to_string()
        });
    #[cfg(not(windows))]
    let sealed = match os_master_key().and_then(|m| aes_seal(plain, &m)) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[config] api-key seal unavailable ({e}); storing plaintext");
            plain.to_string()
        }
    };
    sealed
}

/// Unseal a stored key. Unrecognized (plaintext) values pass through, which
/// transparently migrates pre-encryption configs on their next save. Sealed
/// values that cannot be decrypted on this platform unseal to "" (the key
/// must be re-entered).
pub fn unprotect_api_key(stored: &str) -> String {
    match stored.strip_prefix(KEY_MARK) {
        // DPAPI blobs only unseal on Windows.
        #[cfg(windows)]
        Some(rest) => hex::decode(rest)
            .ok()
            .and_then(|blob| dpapi_unprotect(&blob).ok())
            .and_then(|b| String::from_utf8(b).ok())
            .unwrap_or_else(|| {
                eprintln!("[config] api-key unseal failed; key must be re-entered");
                String::new()
            }),
        #[cfg(not(windows))]
        Some(_) => {
            eprintln!("[config] DPAPI key on non-Windows; key must be re-entered");
            String::new()
        }
        None => match stored.strip_prefix(KEY_MARK_ENC) {
            // AES-GCM blobs only unseal where the OS keyring holds the master key.
            #[cfg(not(windows))]
            Some(sealed) => match os_master_key().and_then(|m| aes_unseal(sealed, &m)) {
                Ok(k) => k,
                Err(e) => {
                    eprintln!("[config] api-key unseal failed ({e}); key must be re-entered");
                    String::new()
                }
            },
            #[cfg(windows)]
            Some(_) => {
                eprintln!("[config] non-Windows sealed key on Windows; key must be re-entered");
                String::new()
            }
            None => stored.to_string(),
        },
    }
}

pub fn load(data_dir: &Path) -> AppConfig {
    let path = config_path(data_dir);
    let mut cfg = match fs::read_to_string(&path) {
        Ok(raw) => match serde_json::from_str::<AppConfig>(&raw) {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!("[config] parse failed ({e}); writing .broken backup and using defaults");
                let _ = fs::copy(&path, data_dir.join("config.json.broken"));
                let cfg = AppConfig::default();
                save(data_dir, &cfg);
                cfg
            }
        },
        Err(_) => {
            let cfg = AppConfig::default();
            save(data_dir, &cfg);
            cfg
        }
    };
    // keys are stored sealed on disk; memory always holds plaintext
    for p in &mut cfg.providers {
        p.api_key = unprotect_api_key(&p.api_key);
    }
    cfg
}

pub fn save(data_dir: &Path, cfg: &AppConfig) {
    let _ = fs::create_dir_all(data_dir);
    // seal API keys before they touch the disk
    let mut out = cfg.clone();
    for p in &mut out.providers {
        p.api_key = protect_api_key(&p.api_key);
    }
    // atomic-ish: write temp then rename
    let path = config_path(data_dir);
    let tmp = data_dir.join("config.json.tmp");
    if let Ok(body) = serde_json::to_string_pretty(&out) {
        if fs::write(&tmp, body).is_ok() {
            let _ = fs::rename(&tmp, &path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plaintext_keys_pass_through() {
        assert_eq!(protect_api_key(""), "");
        assert_eq!(unprotect_api_key("sk-plain"), "sk-plain");
        // already-sealed values of either mark are never double-sealed
        assert_eq!(protect_api_key("dpapi:v1:aa"), "dpapi:v1:aa");
        assert_eq!(protect_api_key("enc:v1:aa"), "enc:v1:aa");
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_roundtrip_and_passthrough() {
        let sealed = protect_api_key("sk-test-12345");
        assert!(sealed.starts_with(KEY_MARK), "expected sealed form, got {sealed}");
        assert_eq!(unprotect_api_key(&sealed), "sk-test-12345");
        // already-sealed input must not be double-sealed
        assert_eq!(protect_api_key(&sealed), sealed);
        // garbage blob unseals to empty, not a panic
        assert_eq!(unprotect_api_key("dpapi:v1:deadbeef"), "");
        // blobs sealed on a non-Windows machine cannot unseal here
        assert_eq!(unprotect_api_key("enc:v1:deadbeef"), "");
    }

    #[cfg(not(windows))]
    #[test]
    fn keyring_roundtrip_or_failopen() {
        let sealed = protect_api_key("sk-test-12345");
        if sealed.starts_with(KEY_MARK_ENC) {
            // OS keyring available (desktop): full roundtrip.
            assert_eq!(unprotect_api_key(&sealed), "sk-test-12345");
            // already-sealed input must not be double-sealed
            assert_eq!(protect_api_key(&sealed), sealed);
            // garbage blob unseals to empty, not a panic
            assert_eq!(unprotect_api_key("enc:v1:not-base64!!"), "");
            // DPAPI blobs from a Windows config unseal to empty here
            assert_eq!(unprotect_api_key("dpapi:v1:deadbeef"), "");
        } else {
            // No OS keyring (headless CI): fail-open to plaintext by design.
            assert_eq!(sealed, "sk-test-12345");
        }
    }

    #[test]
    fn behavior_defaults_serde_roundtrip() {
        let b: ModelBehavior = serde_json::from_str("{}").unwrap();
        assert_eq!(b, ModelBehavior::default());
        let b: ModelBehavior =
            serde_json::from_str(r#"{"max_output":4096,"temperature":0.3}"#).unwrap();
        assert_eq!(b.max_output, Some(4096));
        assert_eq!(b.temperature, Some(0.3));
        assert_eq!(b.reasoning, None);
        // providers without the field (old configs) deserialize cleanly
        let raw = r#"{"version":1,"providers":[{"id":"p","name":"n","kind":"openai_compatible","base_url":"https://x","api_key":"","pricing":{}}],"settings":{"theme":"dark"}}"#;
        let cfg: AppConfig = serde_json::from_str(raw).unwrap();
        assert!(cfg.providers[0].behavior.is_empty());
    }
}
