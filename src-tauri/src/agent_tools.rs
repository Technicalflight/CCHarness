// Read-only agent tools: schema injected into the request (stable bytes,
// part of the cacheable prefix) and executed in-process under a strict
// workspace path guard. Write/execute tools are deliberately absent until a
// permission surface exists.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

pub const MAX_TOOL_RESULT_CHARS: usize = 8_000;
const READ_FILE_CAP: u64 = 256 * 1024;
const WRITE_FILE_CAP: usize = 1024 * 1024;
const LIST_CAP: usize = 200;
const GLOB_CAP: usize = 100;
const GREP_FILE_CAP: usize = 400;
const GREP_MATCH_CAP: usize = 50;
const SKIP_DIRS: &[&str] = &[".git", "node_modules", "target", "dist", "build", ".next", "__pycache__"];

/// True when a directory name should be hidden from listings (skip-list).
pub fn is_skip_dir(name: &str) -> bool {
    SKIP_DIRS.contains(&name)
}

/// Tools that mutate the workspace or execute code — each execution requires
/// an explicit user approval (fail-closed after 120s) unless a session grant
/// exists. take_screenshot is included deliberately: capturing the screen
/// exposes it to the model, so it rides the same privacy gate.
pub const WRITE_TOOLS: &[&str] = &[
    "write_file",
    "edit_file",
    "apply_patch",
    "delete_file",
    "move_path",
    "run_command",
    "take_screenshot",
    "kill_background",
];

pub fn is_write_tool(name: &str) -> bool {
    WRITE_TOOLS.contains(&name)
}

// ---- sandbox mode (沙箱模式：文件 / 命令 / 网络三类访问策略) -----------
//
// Tool-level isolation, NOT a VM/container: the agent keeps running in
// process, but its destructive surface is cut down by policy —
//   文件策略  delete-class tools are refused outright; deny-listed path
//             patterns are blocked for every path-bearing tool; allow-listed
//             trusted paths keep the auto tier approval-free
//   命令策略  per-program deny list (wsl / wmic / sc / reg / schtasks by
//             default), ask list that forces the approval card even in auto
//             mode, allow list that overrides the built-in blocklist, plus
//             the built-in destructive-command blocklist
//   网络策略  domain deny/allow lists, a block-all-external switch and
//             built-in malicious-URL heuristics for web_fetch
// plus: the per-session permission mode "auto" degrades to "approve"
// (enforced at send time). Fail-closed: unknown risk = block.
// A global policy loaded from settings on startup and on every save_config.
// `on` gates everything; the three sub-policies mirror the settings page.
// Backup snapshots live in commands.rs (needs data_dir).

#[derive(Debug, Clone, PartialEq)]
pub struct SandboxPolicy {
    pub on: bool,
    pub files: bool,
    pub commands: bool,
    pub network: bool,
    /// 文件策略：禁止触碰的路径模式（* 通配，不区分大小写，优先级最高）。
    pub file_deny: Vec<String>,
    /// 文件策略：可信路径白名单（自动模式下免审批）。
    pub file_allow: Vec<String>,
    /// 命令策略：禁止运行的程序名（小写，不含 .exe）。
    pub cmd_deny: Vec<String>,
    /// 命令策略：允许运行的程序名（跳过内置高危黑名单）。
    pub cmd_allow: Vec<String>,
    /// 命令策略：需逐次确认的程序名（auto 模式也强制审批）。
    pub cmd_ask: Vec<String>,
    /// 网络策略：禁止访问的域名（含子域名）。
    pub net_deny: Vec<String>,
    /// 网络策略：允许访问的域名（阻止全部外部网络时仍放行）。
    pub net_allow: Vec<String>,
    /// 网络策略：阻止所有外部网络。
    pub net_block_all: bool,
    /// 网络策略：恶意域名拦截（内置启发式规则）。
    pub net_malicious: bool,
}

/// Three-way verdict for the sandbox guard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxVerdict {
    /// Proceed; the regular approval flow applies as configured.
    Allow,
    /// Proceed, but force the approval card even in auto / granted tiers
    /// (命令策略·需逐次确认名单).
    ForceAsk,
    /// Refuse outright; the reason goes back to the model as the tool result
    /// without ever raising an approval card.
    Block(String),
}

static SANDBOX: Mutex<SandboxPolicy> = Mutex::new(SandboxPolicy {
    on: false,
    files: false,
    commands: false,
    network: false,
    file_deny: Vec::new(),
    file_allow: Vec::new(),
    cmd_deny: Vec::new(),
    cmd_allow: Vec::new(),
    cmd_ask: Vec::new(),
    net_deny: Vec::new(),
    net_allow: Vec::new(),
    net_block_all: false,
    net_malicious: false,
});

/// Called on startup and whenever the config is saved.
pub fn set_sandbox_policy(p: SandboxPolicy) {
    *SANDBOX.lock().unwrap_or_else(|p| p.into_inner()) = p;
}

pub fn sandbox_policy() -> SandboxPolicy {
    SANDBOX.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

/// Destructive / network shell patterns (lowercase, substring match).
const SANDBOX_CMD_BLOCKLIST: &[&str] = &[
    // file destruction
    "rm -rf", "rm -fr", "rm -r ", "rd /s", "rmdir /s", "del /s", "del /f", "del /q",
    "remove-item", "format ", "diskpart", "mkfs", "dd if=", "cipher /w", "vssadmin delete",
    "attrib -s -h", "schtasks /create",
    // registry / system
    "reg delete", "reg add", "reg import", "regedit /s", "shutdown", "logoff",
    "stop-computer", "restart-computer",
    "taskkill /f", "taskkill /im", "stop-process", "stop-service",
    // git destructive
    "git push --force", "git push -f", "git reset --hard", "git clean -f",
    "git clean -fd", "git checkout -- .", "git branch -d", "git branch -D",
    // network fetching (命令策略内一并拦截，降低外联风险)
    "curl ", "wget ", "invoke-webrequest", "iwr ", "certutil -urlcache", "bitsadmin",
    "nc ", "ncat ", "telnet ", "ftp ",
    // fork bomb / eval tricks
    ":(){", "| sh", "| bash", "|sh", "|bash", "invoke-expression", "iex ",
];

/// Case-insensitive `*`-wildcard match (no path-boundary awareness — keep
/// the semantics predictable for users writing patterns in the settings UI).
fn wildcard_match(pattern: &str, subject: &str) -> bool {
    fn go(p: &[u8], t: &[u8]) -> bool {
        let (mut pi, mut ti) = (0usize, 0usize);
        let (mut star, mut mark) = (usize::MAX, 0usize);
        while ti < t.len() {
            if pi < p.len() && p[pi] == b'*' {
                star = pi;
                mark = ti;
                pi += 1;
            } else if pi < p.len() && p[pi].eq_ignore_ascii_case(&t[ti]) {
                pi += 1;
                ti += 1;
            } else if star < p.len() {
                pi = star + 1;
                mark += 1;
                ti = mark;
            } else {
                return false;
            }
        }
        while pi < p.len() && p[pi] == b'*' {
            pi += 1;
        }
        pi == p.len()
    }
    let pat = pattern.trim();
    !pat.is_empty() && go(pat.as_bytes(), subject.as_bytes())
}

/// Host of a URL: lowercased, without scheme / userinfo / port / path.
fn url_host(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let rest = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let host = rest.rsplit_once('@').map(|(_, h)| h).unwrap_or(rest);
    let host = host.split(':').next().unwrap_or(host);
    host.trim_matches(|c: char| c == '[' || c == ']').to_lowercase()
}

/// Domain list match: exact host, any sub-domain suffix, or `*` wildcard.
fn domain_match(pattern: &str, host: &str) -> bool {
    let pat = pattern.trim().to_lowercase();
    if pat.is_empty() || host.is_empty() {
        return false;
    }
    wildcard_match(&pat, host) || host == pat || host.ends_with(&format!(".{pat}"))
}

/// Program name of a shell command line: first token (a quoted path counts
/// as one token), basename of any path, `.exe` suffix stripped, lowercased.
/// Unquoted paths containing spaces are inherently ambiguous and resolve to
/// Normalize a command line for policy matching only: cmd.exe treats `^` as
/// an escape and stitches mid-token quotes, so `r^m -rf` and `"r"m -rf`
/// both execute as `rm`; TAB-separated tokens dodge space-anchored
/// blocklist patterns like "rm -rf". Collapse whitespace runs to single
/// spaces, strip `^` and quote characters, lowercase. The raw string is
/// what actually runs — this function exists purely so the deny/blocklist
/// sees what the shell will see.
fn cmd_norm(cmd: &str) -> String {
    let stripped: String = cmd.chars().filter(|c| !matches!(c, '^' | '"' | '\'')).collect();
    stripped.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// Every program a command line may end up executing: the outer program
/// plus the inner programs of shell wrappers (cmd /c, pwsh -command,
/// bash -c, wsl ...), recursively. Policy lists (deny/ask/allow) must see
/// them all — matching only the first token let `cmd /c wsl ...` hide
/// behind an allow entry for `cmd` and bypass the deny list and the
/// built-in blocklist entirely.
fn command_programs(cmd: &str) -> Vec<String> {
    const WRAPPERS: &[&str] = &["cmd", "powershell", "pwsh", "bash", "sh", "zsh", "wsl"];
    let mut out = vec![program_name(cmd)];
    let norm = cmd_norm(cmd);
    let toks: Vec<&str> = norm.split_whitespace().collect();
    let is_wrapper = |t: &str| WRAPPERS.contains(&t.strip_suffix(".exe").unwrap_or(t));
    let mut in_wrapper = if toks.first().is_some_and(|t| is_wrapper(t)) { 1 } else { 0 };
    for raw in toks.iter().skip(1) {
        let t = raw.strip_suffix(".exe").unwrap_or(raw);
        let flag = t.starts_with('/') || t.starts_with('-');
        let t = if flag { t } else { t.rsplit(['/', '\\']).next().unwrap_or(t) };
        if in_wrapper > 0 {
            if flag {
                continue; // /c, -command, -d ... — the wrapped program comes next
            }
            out.push(t.to_string());
            in_wrapper -= 1;
            if is_wrapper(t) && in_wrapper < 4 {
                in_wrapper += 1; // nested wrapper: cmd /c pwsh -c ...
            }
        } else if is_wrapper(t) && in_wrapper < 4 {
            in_wrapper += 1; // wrapper appearing mid-command
        }
    }
    out.sort();
    out.dedup();
    out
}

/// the first segment — document this in the settings dialog.
fn program_name(cmd: &str) -> String {
    let s = cmd.trim();
    let first = if s.starts_with('"') || s.starts_with('\'') {
        let q = s.chars().next().unwrap();
        match s[1..].find(q) {
            Some(end) => &s[1..1 + end],
            None => s[1..].trim_end_matches(q),
        }
    } else {
        s.split_whitespace().next().unwrap_or("")
    };
    let base = first.rsplit(['/', '\\']).next().unwrap_or(first);
    // same escapes/quote-stitching dodge the deny list: `w^sl` and `w"s"l`
    // both spawn wsl once the shell parses them
    let base: String = base.chars().filter(|c| !matches!(c, '^' | '"' | '\'')).collect();
    // lowercase BEFORE stripping the suffix: strip_suffix is case-sensitive
    let lower = base.to_lowercase();
    match lower.strip_suffix(".exe") {
        Some(stem) => stem.to_string(),
        None => lower,
    }
}

/// Built-in malicious-URL heuristics (网络策略·恶意域名拦截): non-http(s)
/// scheme, credentials in the URL, punycode homograph host.
fn malicious_url(url: &str, host: &str) -> bool {
    let lower = url.to_lowercase();
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        return true;
    }
    if let Some((_, rest)) = lower.split_once("://") {
        if rest.contains('@') {
            return true; // userinfo tricks (user:pass@host)
        }
    }
    host.starts_with("xn--") || host.contains(".xn--")
}

/// Path-like string arguments of a tool (deny/allow lists evaluate these).
fn path_args(args: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for key in ["path", "from", "to"] {
        if let Some(v) = args.get(key).and_then(|v| v.as_str()) {
            if !v.is_empty() {
                out.push(v.to_string());
            }
        }
    }
    out
}

/// 文件策略·白名单：every path the tool touches sits inside the trusted
/// allow-list (used by the send path to keep the auto tier approval-free
/// for explicitly trusted paths).
pub fn file_paths_trusted(name: &str, args: &Value) -> bool {
    let p = sandbox_policy();
    if !p.on || !p.files || p.file_allow.is_empty() {
        return false;
    }
    if !matches!(name, "write_file" | "edit_file" | "apply_patch") {
        return false;
    }
    let paths = path_args(args);
    !paths.is_empty()
        && paths
            .iter()
            .all(|path| p.file_allow.iter().any(|pat| wildcard_match(pat, path)))
}

/// Pre-execution guard for mutating / network tools. The write path treats
/// `ForceAsk` as "show the approval card even in auto / grant mode"; both
/// paths turn `Block` into an ERROR tool result without ever raising a card.
pub fn sandbox_check(name: &str, args: &Value) -> SandboxVerdict {
    let p = sandbox_policy();
    if !p.on {
        return SandboxVerdict::Allow;
    }
    // ---- 文件策略 ----
    if p.files {
        if name == "delete_file" {
            return SandboxVerdict::Block(
                "沙箱模式已拦截：删除类操作被禁止（文件策略）。如需删除，请关闭沙箱模式或手动执行。".into(),
            );
        }
        if !p.file_deny.is_empty() {
            for path in path_args(args) {
                if p.file_deny.iter().any(|pat| wildcard_match(pat, &path)) {
                    return SandboxVerdict::Block(format!(
                        "沙箱模式已拦截：路径「{path}」命中文件禁止名单（文件策略）。"
                    ));
                }
            }
        }
    }
    // ---- 网络策略 ----
    if p.network && name == "web_fetch" {
        let url = str_arg(args, "url");
        let host = url_host(&url);
        if p.net_malicious && malicious_url(&url, &host) {
            return SandboxVerdict::Block(format!(
                "沙箱模式已拦截：「{host}」命中恶意域名拦截规则（网络策略）。"
            ));
        }
        if p.net_deny.iter().any(|d| domain_match(d, &host)) {
            return SandboxVerdict::Block(format!(
                "沙箱模式已拦截：「{host}」命中网络禁止名单（网络策略）。"
            ));
        }
        if p.net_block_all && !p.net_allow.iter().any(|d| domain_match(d, &host)) {
            return SandboxVerdict::Block(
                "沙箱模式已拦截：已开启「阻止所有外部网络」，目标域名不在允许名单（网络策略）。".into(),
            );
        }
    }
    // ---- 命令策略 ----
    if p.commands && name == "run_command" {
        let cmd = str_arg(args, "command");
        let programs = command_programs(&cmd);
        let listed = |list: &[String], pr: &str| list.iter().any(|d| d.trim().to_lowercase() == pr);
        if let Some(pr) = programs.iter().find(|pr| listed(&p.cmd_deny, pr)) {
            return SandboxVerdict::Block(format!(
                "沙箱模式已拦截：程序「{pr}」在命令禁止名单中（命令策略）。"
            ));
        }
        if programs.iter().any(|pr| listed(&p.cmd_ask, pr)) {
            return SandboxVerdict::ForceAsk;
        }
        if !programs.is_empty() && programs.iter().all(|pr| listed(&p.cmd_allow, pr)) {
            // 显式允许名单：用户自担风险的放行，跳过内置高危黑名单。
            // 只有当全部将执行的程序（含 cmd /c 等包装的内层）都在允许
            // 名单里才生效 —— 单放行外层不再为内层程序开绿灯。
            return SandboxVerdict::Allow;
        }
        if let Some(hit) = SANDBOX_CMD_BLOCKLIST
            .iter()
            .find(|pat| cmd_norm(&cmd).contains(&pat.trim().to_lowercase()))
        {
            return SandboxVerdict::Block(format!(
                "沙箱模式已拦截：命令命中高危策略「{}」（命令策略）。可关闭沙箱模式后重试，或自行在终端执行。",
                hit.trim()
            ));
        }
    }
    SandboxVerdict::Allow
}

/// The OpenAI `tools` array. Constant bytes per build — it sits in the
/// request head ahead of `messages`, inside the cacheable prefix.
pub fn schema() -> Value {
    json!([
        {
            "type": "function",
            "function": {
                "name": "list_dir",
                "description": "列出工作区内某目录的内容（目录在先，含大小标注）。path 为相对工作区的路径，根目录用 \"\"",
                "parameters": {
                    "type": "object",
                    "properties": { "path": { "type": "string", "description": "相对工作区的目录路径，\"\" 表示根" } },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "读取工作区内一个文本文件（超过 256KB 截断，二进制文件会被拒绝）",
                "parameters": {
                    "type": "object",
                    "properties": { "path": { "type": "string", "description": "相对工作区的文件路径" } },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "glob_files",
                "description": "按 glob 模式匹配工作区内的文件路径，如 \"src/**/*.rs\"",
                "parameters": {
                    "type": "object",
                    "properties": { "pattern": { "type": "string", "description": "相对工作区的 glob 模式" } },
                    "required": ["pattern"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "grep_files",
                "description": "在工作区文本文件中做大小写不敏感的子串搜索，返回 路径:行号:内容；可用 glob 过滤文件范围",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "搜索的子串" },
                        "glob": { "type": "string", "description": "可选的文件名过滤 glob，如 *.rs" }
                    },
                    "required": ["pattern"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "load_skill",
                "description": "按名字加载一个技能的完整指令正文（纯 prompt 注入，不读写文件）。当任务匹配系统提示「可用技能」名录中的某项、或用户要求使用某技能时调用；同一技能每次会话只需加载一次，重复加载返回相同内容",
                "parameters": {
                    "type": "object",
                    "properties": { "name": { "type": "string", "description": "技能名，见系统提示「可用技能」名录（可带或不带前导 /）" } },
                    "required": ["name"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "创建或完整覆写工作区内一个文本文件（需要用户批准）。适合新文件或整体重写；小改动优先用 edit_file",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "相对工作区的文件路径" },
                        "content": { "type": "string", "description": "完整文件内容" }
                    },
                    "required": ["path", "content"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "edit_file",
                "description": "对工作区内文件做一次精确文本替换（需要用户批准）。old_text 必须在文件中恰好出现一次；不唯一时提供更长的上下文重试",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "相对工作区的文件路径" },
                        "old_text": { "type": "string", "description": "要替换的原文（须唯一）" },
                        "new_text": { "type": "string", "description": "替换后的文本" }
                    },
                    "required": ["path", "old_text", "new_text"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "run_command",
                "description": "在工作区根目录执行一条 shell 命令并返回合并输出（需要用户批准）。适合构建、测试、git 等短任务；超时上限 120 秒，超时进程会被终止；输出超长会被截断。run_in_background=true 时立即返回 shell_id，之后用 read_background_output 读取增量输出、kill_background 终止 —— 适合 dev server / 长构建 / 监听类命令",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "要执行的命令行（Windows 走 cmd /C，其他走 sh -c）" },
                        "timeout_secs": { "type": "integer", "description": "可选超时秒数，1–120，默认 60（后台模式忽略）" },
                        "run_in_background": { "type": "boolean", "description": "true 时后台运行：立即返回 shell_id，不等待结束（默认 false）" }
                    },
                    "required": ["command"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "read_background_output",
                "description": "读取一个后台 shell 自上次读取以来的新增输出（stdout+stderr 合并）。返回运行状态（running / exited(code)）与新增文本；重复调用返回增量。shell_id 来自 run_command 的 run_in_background 返回值",
                "parameters": {
                    "type": "object",
                    "properties": { "shell_id": { "type": "integer", "description": "后台 shell 编号" } },
                    "required": ["shell_id"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "kill_background",
                "description": "终止一个仍在运行的后台 shell 进程（需要用户批准）。shell_id 来自 run_command 的 run_in_background 返回值",
                "parameters": {
                    "type": "object",
                    "properties": { "shell_id": { "type": "integer", "description": "后台 shell 编号" } },
                    "required": ["shell_id"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "take_screenshot",
                "description": "截取当前整个屏幕的画面（需要用户批准），截图会作为图片附带在工具结果之后返回给你，可以据此观察 GUI 状态后再决定下一步动作（如用 run_command 操作）。适合验证命令执行后的界面效果、查看弹窗或报错信息",
                "parameters": { "type": "object", "properties": {}, "required": [] }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "web_fetch",
                "description": "抓取一个公网 URL 的正文并转为纯文本（只读；SSRF 防护会拒绝本机/内网地址）。HTML 页面会去除标签与 script/style；适合查文档、读 API 响应",
                "parameters": {
                    "type": "object",
                    "properties": { "url": { "type": "string", "description": "http/https 地址" } },
                    "required": ["url"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "apply_patch",
                "description": "对工作区内一个文件按顺序应用多处精确替换（需要用户批准）。每处 old_text 都必须在当前文件中恰好出现一次；比多次 edit_file 更省轮次",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "相对工作区的文件路径" },
                        "hunks": {
                            "type": "array",
                            "description": "按顺序应用的替换列表",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "old_text": { "type": "string", "description": "要替换的原文（须唯一）" },
                                    "new_text": { "type": "string", "description": "替换后的文本" }
                                },
                                "required": ["old_text", "new_text"]
                            }
                        }
                    },
                    "required": ["path", "hunks"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "delete_file",
                "description": "删除工作区内一个文件（需要用户批准；只能删文件，不能删目录，不可恢复）",
                "parameters": {
                    "type": "object",
                    "properties": { "path": { "type": "string", "description": "相对工作区的文件路径" } },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "move_path",
                "description": "在工作区内移动/重命名一个文件或目录（需要用户批准；目标已存在时拒绝）",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "from": { "type": "string", "description": "源路径（相对工作区）" },
                        "to": { "type": "string", "description": "目标路径（相对工作区）" }
                    },
                    "required": ["from", "to"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "todo_write",
                "description": "维护本会话的任务清单（显示在任务面板）。长任务开始时拆解为 3–7 个待办；每完成/开始一项就整体更新一次清单。status: pending(待办) / in_progress(进行中) / done(已完成)。每次调用以全量列表替换现有清单",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "todos": {
                            "type": "array",
                            "description": "完整的任务列表（覆盖式）",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "text": { "type": "string", "description": "任务描述（一句话，≤200 字符）" },
                                    "status": { "type": "string", "description": "pending | in_progress | done", "enum": ["pending", "in_progress", "done"] }
                                },
                                "required": ["text", "status"]
                            }
                        }
                    },
                    "required": ["todos"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "delegate_subagent",
                "description": "把一个相对独立的调研/分析子任务委派给后台子智能体（独立上下文、只读工具），完成后以结论回报。适合：代码库探索、多文件调研、测试分析。task 需自包含（子智能体看不到当前对话）。若下文列出了具名子智能体，agent 参数填其名字即可使用该角色的专属模型与提示词",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "task": { "type": "string", "description": "子任务的完整描述：要查什么、输出什么结论" },
                        "agent": { "type": "string", "description": "可选：具名子智能体（见工具说明中列出的名字）。缺省用主会话的模型配置" }
                    },
                    "required": ["task"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "memory_save",
                "description": "把一条值得长期记住的事实/结论/偏好写入跨会话的向量长期记忆（按工作区隔离，之后每轮对话会按语义相关自动召回）。只存精炼结论，不要存大段原文或临时信息",
                "parameters": {
                    "type": "object",
                    "properties": { "text": { "type": "string", "description": "要记住的内容（一句话精炼结论，≤2000 字符）" } },
                    "required": ["text"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "memory_search",
                "description": "在当前工作区的向量长期记忆中做语义搜索，返回最相关的若干条已存记忆",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "语义搜索的查询文本" },
                        "k": { "type": "integer", "description": "可选：返回条数（默认 5，上限 20）" }
                    },
                    "required": ["query"]
                }
            }
        }
    ])
}

/// Tools excluded from the read-only surface. Write tools need approval;
/// delegation is reserved for the parent agent (sub-agents cannot spawn
/// sub-agents — no recursion).
const EXCLUDED_FROM_READONLY: &[&str] = &[
    "write_file",
    "edit_file",
    "apply_patch",
    "delete_file",
    "move_path",
    "run_command",
    "take_screenshot",
    "delegate_subagent",
    "kill_background",
];

/// The read-only subset — used when the session's permission mode is
/// "readonly" or the workflow gate is in plan mode, and for sub-agents.
pub fn schema_readonly() -> Value {
    let all = schema();
    let filtered: Vec<Value> = all
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter(|t| {
                    let name = t.pointer("/function/name").and_then(|n| n.as_str()).unwrap_or("");
                    !EXCLUDED_FROM_READONLY.contains(&name)
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    Value::Array(filtered)
}

/// Whitelist filter for named-subagent tool surfaces (ZCode-style profile
/// `tools` field): keep only the named tools from the FULL schema. Write
/// tools are included only when explicitly listed — the caller (delegate
/// surface) is opt-in per profile. Unknown names are silently dropped.
pub fn schema_filtered(allow: &[String]) -> Value {
    let all = schema();
    let filtered: Vec<Value> = all
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter(|t| {
                    let name = t.pointer("/function/name").and_then(|n| n.as_str()).unwrap_or("");
                    allow.iter().any(|a| a == name)
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    Value::Array(filtered)
}

/// Resolve a workspace-relative path, refusing escapes, absolute paths and
/// symlink tricks as far as lexical + canonical checks allow.
pub fn resolve_in_workspace(workspace: &str, rel: &str) -> Result<PathBuf, String> {
    let rel_trim = rel.trim();
    // Windows tolerates stray leading slashes on workspace-relative input
    // ("/sub/file.txt" meaning "sub/file.txt"); on POSIX a leading slash IS
    // the absolute marker and must reach the absolute branch untouched —
    // trimming it there would silently re-base /etc/passwd into the
    // workspace instead of refusing it (CI Linux catch, v0.2.0).
    #[cfg(windows)]
    let rel_trim = rel_trim.trim_start_matches(['/', '\\']);
    let rel_trim = rel_trim.trim_end_matches(['/', '\\']);
    if rel_trim.is_empty() {
        return Ok(PathBuf::from(workspace));
    }
    let candidate = Path::new(rel_trim);
    let has_parent = candidate
        .components()
        .any(|c| matches!(c, Component::ParentDir));
    if candidate.is_absolute() {
        // `..` is refused before any prefix comparison: starts_with is a
        // component-level lexical check, so "C:/ws/../../x" would pass a
        // naive prefix test while the OS still resolves the ParentDirs
        // outside the workspace when the path is actually opened.
        if has_parent {
            return Err("拒绝包含 .. 的路径".into());
        }
        // an empty workspace has no boundary to compare against — any
        // absolute path would trivially "start with" the empty prefix
        if workspace.trim().is_empty() {
            return Err(format!("拒绝绝对路径 {rel_trim}：未设置工作区"));
        }
        // allow a path that is literally inside the workspace
        let ws_norm = normalize_plain(Path::new(workspace));
        let c_norm = normalize_plain(candidate);
        if c_norm.starts_with(&ws_norm) {
            // canonical re-check closes the symlink escape: when both sides
            // resolve on disk, the real location must remain inside.
            canonical_escape_check(&c_norm, &ws_norm, rel_trim)?;
            return Ok(c_norm);
        }
        return Err(format!("拒绝绝对路径 {rel_trim}：请使用相对工作区的路径"));
    }
    if has_parent {
        return Err("拒绝包含 .. 的路径".into());
    }
    let joined = normalize_plain(&Path::new(workspace).join(candidate));
    let base = normalize_plain(Path::new(workspace));
    if !joined.starts_with(&base) {
        return Err(format!("路径 {rel_trim} 越出工作区边界"));
    }
    // the relative branch needs the same canonical re-check as the absolute
    // one: a symlink inside the workspace (repo-supplied, or planted via one
    // approved command) carries the resolved location outside while the
    // lexical prefix still holds.
    canonical_escape_check(&joined, &base, rel_trim)?;
    Ok(joined)
}

/// Symlink-escape re-check shared by both path branches: resolve the deepest
/// existing ancestor of `p` and require it to stay inside the canonical
/// workspace root. Walking up matters because `canonicalize` needs every
/// component to exist — a not-yet-created leaf is covered by its deepest
/// existing ancestor, which is exactly the chain `create_dir_all`/`fs::write`
/// would follow.
fn canonical_escape_check(p: &Path, base: &Path, rel_trim: &str) -> Result<(), String> {
    let Ok(ws_canon) = fs::canonicalize(base) else {
        return Ok(()); // workspace root not on disk — nothing to compare against
    };
    let mut probe = p.to_path_buf();
    let resolved = loop {
        match fs::canonicalize(&probe) {
            Ok(real) => break real,
            Err(_) => match probe.parent() {
                Some(parent) => probe = parent.to_path_buf(),
                None => break probe,
            },
        }
    };
    if !resolved.starts_with(&ws_canon) {
        return Err(format!("路径 {rel_trim} 经符号链接越出工作区边界"));
    }
    Ok(())
}

/// Write-side complement to `canonical_escape_check`: refuse a symlink as
/// the final target component. A *dangling* link cannot be resolved by
/// `canonicalize` at all, yet `fs::write` through it would create the file
/// at the link's destination — outside the workspace.
fn refuse_symlink_target(path: &Path, rel: &str) -> Result<(), String> {
    if fs::symlink_metadata(path)
        .map(|m| m.is_symlink())
        .unwrap_or(false)
    {
        return Err(format!("拒绝操作符号链接 {rel}（真实落点可能在工作区之外）"));
    }
    Ok(())
}

fn normalize_plain(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn truncate_result(mut s: String) -> String {
    if s.len() > MAX_TOOL_RESULT_CHARS {
        let mut cut = MAX_TOOL_RESULT_CHARS;
        while cut > 0 && !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.push_str("\n…[结果已截断]");
    }
    s
}

/// Shared truncation for tool-shaped text (MCP results use the same cap).
pub fn truncate_public(s: &str) -> String {
    truncate_result(s.to_string())
}

pub fn execute(workspace: &str, name: &str, args: &Value) -> String {
    let result: Result<String, String> = match name {
        "list_dir" => list_dir(workspace, &str_arg(args, "path")),
        "read_file" => read_file(workspace, &str_arg(args, "path")),
        "glob_files" => glob_files(workspace, &str_arg(args, "pattern")),
        "grep_files" => grep_files(workspace, &str_arg(args, "pattern"), &str_arg(args, "glob")),
        "web_fetch" => web_fetch(&str_arg(args, "url")),
        "read_background_output" => read_background_output(
            args.get("shell_id").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
        ),
        "load_skill" => crate::skills::load_body(
            if workspace.is_empty() { None } else { Some(workspace) },
            &str_arg(args, "name"),
        )
        .map(|body| {
            // market-installed bodies are third-party text — the tool
            // result must carry the same untrusted fence as Zone S
            let name = str_arg(args, "name");
            crate::skills::guarded_body(&name, &body)
        }),
        _ => Err(format!("未知工具 {name}")),
    };
    match result {
        Ok(text) => truncate_result(text),
        Err(e) => truncate_result(format!("ERROR: {e}")),
    }
}

/// Human-facing preview for the approval card: a unified-ish diff for
/// edit_file, or the head of the new content for write_file.
pub fn approval_preview(workspace: &str, name: &str, args: &Value) -> String {
    let path = str_arg(args, "path");
    match name {
        "write_file" => {
            let content = str_arg(args, "content");
            let lines = content.lines().count();
            let head: String = content.lines().take(40).collect::<Vec<_>>().join("\n");
            let pres = if path.ends_with(".rs") {
                "rust"
            } else if path.ends_with(".ts") || path.ends_with(".tsx") {
                "ts"
            } else if path.ends_with(".py") {
                "python"
            } else {
                ""
            };
            let more = if lines > 40 { "\n…[仅预览前 40 行]" } else { "" };
            format!("写入 {path}（{lines} 行，{} B）\n\n```{pres}\n{head}\n```{more}", content.len())
        }
        "edit_file" => {
            let old = str_arg(args, "old_text");
            let new = str_arg(args, "new_text");
            let exists =
                fs::read_to_string(resolve_in_workspace(workspace, &path).unwrap_or_default()).ok();
            match exists {
                Some(cur) => {
                    let n = cur.matches(&old).count();
                    format!(
                        "编辑 {path}（原文出现 {n} 次，须为 1 才会成功）\n\n- {}\n+ {}",
                        truncate_result(old),
                        truncate_result(new),
                    )
                }
                None => format!("编辑 {path}（文件当前不存在或不可读）\n\n- {old}\n+ {new}"),
            }
        }
        "run_command" => {
            let cmd = str_arg(args, "command");
            let bg = args.get("run_in_background").and_then(|v| v.as_bool()).unwrap_or(false);
            if bg {
                format!("在工作区后台执行命令（立即返回，不等待完成）\n\n```sh\n{cmd}\n```")
            } else {
                let timeout = args.get("timeout_secs").and_then(|t| t.as_u64()).unwrap_or(60);
                format!(
                    "在工作区执行命令（超时 {timeout} 秒）\n\n```sh\n{cmd}\n```"
                )
            }
        }
        "kill_background" => {
            let id = args.get("shell_id").and_then(|v| v.as_u64()).unwrap_or(0);
            format!("终止后台 shell #{id}（仍在运行则杀掉其进程）")
        }
        "take_screenshot" => {
            "截取当前整个屏幕画面（PNG）并发送给模型查看 —— 截图保存在本会话的附件目录".to_string()
        }
        "apply_patch" => {
            let n = args.get("hunks").and_then(|h| h.as_array()).map(|a| a.len()).unwrap_or(0);
            let first_old = args
                .pointer("/hunks/0/old_text")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let first_new = args
                .pointer("/hunks/0/new_text")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            format!(
                "编辑 {path}（{n} 处替换）\n\n- {}\n+ {}{more}",
                truncate_result(first_old.to_string()),
                truncate_result(first_new.to_string()),
                more = if n > 1 { format!("\n\n…[共 {n} 处，仅预览第 1 处]") } else { String::new() }
            )
        }
        "delete_file" => {
            let abs = resolve_in_workspace(workspace, &path).ok();
            let size = abs.as_ref().and_then(|p| fs::metadata(p).ok()).map(|m| m.len()).unwrap_or(0);
            format!("删除文件 {path}（{size} B，不可恢复；目录不可删除）")
        }
        "move_path" => {
            let from = str_arg(args, "from");
            let to = str_arg(args, "to");
            format!("移动 {from} → {to}（目标已存在时拒绝）")
        }
        _ => format!("{name} {path}"),
    }
}

/// Mutating execution — only called after the approval gate cleared.
pub fn execute_write(workspace: &str, name: &str, args: &Value) -> String {
    let result: Result<String, String> = match name {
        "write_file" => write_file(workspace, &str_arg(args, "path"), &str_arg(args, "content")),
        "edit_file" => edit_file(
            workspace,
            &str_arg(args, "path"),
            &str_arg(args, "old_text"),
            &str_arg(args, "new_text"),
        ),
        "run_command" => {
            if args.get("run_in_background").and_then(|v| v.as_bool()).unwrap_or(false) {
                match spawn_background(workspace, &str_arg(args, "command")) {
                    Ok(id) => Ok(format!(
                        "已在后台启动（shell_id={id}）—— 用 read_background_output(shell_id={id}) 读取增量输出；不再需要时用 kill_background 终止。输出过多时仅保留尾部。"
                    )),
                    Err(e) => Err(e),
                }
            } else {
                run_command(
                    workspace,
                    &str_arg(args, "command"),
                    args.get("timeout_secs").and_then(|t| t.as_u64()).unwrap_or(60),
                )
            }
        }
        "kill_background" => kill_background(
            args.get("shell_id").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
        ),
        "apply_patch" => {
            let hunks = args.get("hunks").and_then(|h| h.as_array()).cloned().unwrap_or_default();
            apply_patch(workspace, &str_arg(args, "path"), &hunks)
        }
        "delete_file" => delete_file(workspace, &str_arg(args, "path")),
        "move_path" => move_path(workspace, &str_arg(args, "from"), &str_arg(args, "to")),
        _ => Err(format!("{name} 不是写工具")),
    };
    match result {
        Ok(text) => truncate_result(text),
        Err(e) => truncate_result(format!("ERROR: {e}")),
    }
}

/// One captured screenshot, ready to attach to a tool record: the relative
/// filename inside the session attachment dir plus the wire-ready payload.
pub struct Shot {
    pub filename: String,
    pub mime: String,
    pub b64: String,
}

/// Computer Use "see" primitive: capture the whole virtual screen to a PNG
/// under the session's attachment dir. The "act" half is run_command. The
/// caller owns the approval gate (privacy-sensitive: this exposes the
/// screen to the model); here we only capture + save + encode.
/// Returns the tool-result text and the shots to attach.
pub fn take_screenshot(data_dir: &std::path::Path, session_id: &str) -> Result<(String, Vec<Shot>), String> {
    const SHOT_MAX_BYTES: usize = 8 * 1024 * 1024;
    let dir = crate::sessions::attachments_dir(data_dir, session_id);
    fs::create_dir_all(&dir).map_err(|e| format!("创建附件目录失败: {e}"))?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let filename = format!("shot_{ts}.png");
    let path = dir.join(&filename);
    capture_to(&path)?;
    let bytes = fs::read(&path).map_err(|e| format!("读取截图失败: {e}"))?;
    if bytes.len() > SHOT_MAX_BYTES {
        let _ = fs::remove_file(&path);
        return Err("截图超过 8MB 上限".into());
    }
    use base64::Engine as _;
    let kb = (bytes.len() + 1023) / 1024;
    Ok((
        format!("OK: 已截取当前屏幕（PNG，{kb} KB），截图已作为图片附带在本条结果之后。"),
        vec![Shot {
            filename,
            mime: "image/png".into(),
            b64: base64::engine::general_purpose::STANDARD.encode(&bytes),
        }],
    ))
}

/// Platform capture into an absolute path. Windows uses PowerShell
/// System.Drawing (no extra dependencies); macOS `screencapture`; Linux
/// scrot or ImageMagick import. Runs with CREATE_NO_WINDOW on Windows so
/// no console flashes over the very screen being captured.
fn capture_to(path: &Path) -> Result<(), String> {
    // Windows passes the path as base64 decoded inside the script: a path
    // that reaches PowerShell through string interpolation is one broken
    // quote away from executing attacker-chosen script (the path derives
    // from the session id, which is not fully under our control).
    let p = path.to_string_lossy().to_string();
    #[cfg(windows)]
    {
        use base64::Engine as _;
        let p_b64 = base64::engine::general_purpose::STANDARD.encode(p.as_bytes());
        let script = format!(
            "Add-Type -AssemblyName System.Windows.Forms,System.Drawing; \
             $p=[System.Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{p_b64}')); \
             $b=[System.Windows.Forms.SystemInformation]::VirtualScreen; \
             $bmp=New-Object System.Drawing.Bitmap $b.Width,$b.Height; \
             $g=[System.Drawing.Graphics]::FromImage($bmp); \
             $g.CopyFromScreen($b.X,$b.Y,0,0,$bmp.Size); \
             $g.Dispose(); $bmp.Save($p); $bmp.Dispose()",
            p_b64 = p_b64
        );
        let mut c = std::process::Command::new("powershell");
        c.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let out = c.output().map_err(|e| format!("启动 PowerShell 失败: {e}"))?;
        if !out.status.success() {
            let err: String = String::from_utf8_lossy(&out.stderr).chars().take(200).collect();
            return Err(format!("截屏失败: {err}"));
        }
        if !path.is_file() {
            return Err("截屏命令执行完成但未生成图片文件".into());
        }
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("screencapture")
            .args(["-x", &p])
            .output()
            .map_err(|e| format!("启动 screencapture 失败: {e}"))?;
        if !out.status.success() {
            return Err("截屏失败（screencapture）".into());
        }
        return Ok(());
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let out = std::process::Command::new("scrot")
            .args(["-z", &p])
            .output()
            .or_else(|_| {
                std::process::Command::new("import").args(["-window", "root", &p]).output()
            })
            .map_err(|_| "截屏失败：未找到 scrot 或 ImageMagick import".to_string())?;
        if !out.status.success() {
            return Err("截屏失败".into());
        }
        Ok(())
    }
}

/// Execute one shell command with cwd = workspace root. Blocking by design
/// (same as the fs tools); pipes are drained on threads so chatty children
/// can't deadlock, and the child is killed at the timeout.
/// 终止整个进程树而非仅 shell：`cmd /C long_task` 的孙进程在只杀
/// cmd.exe 时会存活并继续运行（S3）。Windows 用 taskkill /T /F；Unix
/// 因 shell 以进程组首身份启动（见 process_group(0)），killpg 整组
/// SIGKILL。
#[cfg(windows)]
fn kill_tree(child: &mut std::process::Child) {
    use std::os::windows::process::CommandExt;
    let pid = child.id();
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(unix)]
fn kill_tree(child: &mut std::process::Child) {
    // SAFETY: 只向 spawn 时新建的进程组发送信号
    unsafe {
        libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn run_command(workspace: &str, command: &str, timeout_secs: u64) -> Result<String, String> {
    const CMD_CAP: u64 = 120;
    let cmd = command.trim();
    if cmd.is_empty() {
        return Err("命令为空".into());
    }
    if cmd.len() > 8 * 1024 {
        return Err("命令过长（>8KB）".into());
    }
    let timeout = timeout_secs.clamp(1, CMD_CAP);
    let root = Path::new(workspace);
    if !root.is_dir() {
        return Err("工作区不存在".into());
    }

    let mut c = std::process::Command::new(if cfg!(windows) { "cmd" } else { "sh" });
    if cfg!(windows) {
        // chcp 65001 first so child output is UTF-8 (CJK Windows defaults to GBK)
        c.args(["/C", &format!("chcp 65001>nul & {cmd}")]);
    } else {
        c.args(["-c", cmd]);
    }
    c.current_dir(root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW — no console flash
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        c.process_group(0); // 自成进程组：kill_tree 可整组 SIGKILL
    }
    let mut child = c.spawn().map_err(|e| format!("启动失败: {e}"))?;

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    // join() 可能永久挂住：树被终止后，分离的孙进程仍持有管道句柄时
    // read_to_end 不会返回 —— 改用 channel + 宽限超时，宁可丢尾部输出
    let (tx_out, rx_out) = std::sync::mpsc::channel::<Vec<u8>>();
    let (tx_err, rx_err) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut std::io::BufReader::new(stdout), &mut buf);
        let _ = tx_out.send(buf);
    });
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut std::io::BufReader::new(stderr), &mut buf);
        let _ = tx_err.send(buf);
    });

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout);
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {}
            Err(e) => {
                kill_tree(&mut child);
                return Err(format!("等待进程失败: {e}"));
            }
        }
        if std::time::Instant::now() >= deadline {
            kill_tree(&mut child);
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };

    let decode = |bytes: Vec<u8>| -> String {
        let s = String::from_utf8_lossy(&bytes);
        s.chars().take(MAX_TOOL_RESULT_CHARS * 2).collect()
    };
    const DRAIN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);
    let out = match rx_out.recv_timeout(DRAIN_GRACE) {
        Ok(b) => decode(b),
        Err(_) => String::new(),
    };
    let err = match rx_err.recv_timeout(DRAIN_GRACE) {
        Ok(b) => decode(b),
        Err(_) => String::new(),
    };

    let head = match status {
        Some(st) if st.success() => format!("OK: 退出码 0（{timeout} 秒内完成）"),
        Some(st) => format!("命令失败: 退出码 {}", st.code().unwrap_or(-1)),
        None => format!("命令超时: 超过 {timeout} 秒，进程已终止"),
    };
    let mut body = String::new();
    if !out.trim().is_empty() {
        body.push_str(&format!("\n--- stdout ---\n{}", out.trim_end()));
    }
    if !err.trim().is_empty() {
        body.push_str(&format!("\n--- stderr ---\n{}", err.trim_end()));
    }
    if body.is_empty() {
        body.push_str("\n（无输出）");
    }
    Ok(format!("{head}{body}"))
}

// ---- background shells (run_in_background, ZCode parity) ----------------

/// One live background shell: output buffer + read cursor under one lock,
/// the child behind its own mutex (kill/wait serialize on it), exit state
/// flipped by a 200ms poll thread.
struct BgShell {
    child: Mutex<std::process::Child>,
    out: Mutex<OutBuf>,
    done: std::sync::atomic::AtomicBool,
    exit_code: Mutex<Option<i32>>,
    command: String,
}

struct OutBuf {
    bytes: Vec<u8>,
    read_pos: usize,
}

static BG_SHELLS: Mutex<Option<HashMap<u32, BgShell>>> = Mutex::new(None);
static BG_NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
/// Buffer ceiling per shell — past it the head is dropped (tail kept).
const BG_BUF_CAP: usize = 512 * 1024;
/// Registry ceiling: shells beyond this drop the oldest finished ones.
const BG_SHELL_CAP: usize = 32;

fn bg_lock() -> std::sync::MutexGuard<'static, Option<HashMap<u32, BgShell>>> {
    BG_SHELLS.lock().unwrap_or_else(|p| p.into_inner())
}

/// Spawn a long-running command detached from the tool call: returns the
/// shell_id immediately; output accumulates for read_background_output.
pub fn spawn_background(workspace: &str, command: &str) -> Result<u32, String> {
    let cmd = command.trim();
    if cmd.is_empty() {
        return Err("命令为空".into());
    }
    if cmd.len() > 8 * 1024 {
        return Err("命令过长（>8KB）".into());
    }
    let root = Path::new(workspace);
    if !root.is_dir() {
        return Err("工作区不存在".into());
    }
    let mut c = std::process::Command::new(if cfg!(windows) { "cmd" } else { "sh" });
    if cfg!(windows) {
        c.args(["/C", &format!("chcp 65001>nul & {cmd}")]);
    } else {
        c.args(["-c", cmd]);
    }
    c.current_dir(root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = c.spawn().map_err(|e| format!("启动失败: {e}"))?;
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    let id = BG_NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let shell = BgShell {
        child: Mutex::new(child),
        out: Mutex::new(OutBuf { bytes: Vec::new(), read_pos: 0 }),
        done: std::sync::atomic::AtomicBool::new(false),
        exit_code: Mutex::new(None),
        command: cmd.chars().take(200).collect(),
    };

    // pump stdout/stderr into the shell's buffer until the process ends
    {
        let mut guard = bg_lock();
        let map = guard.get_or_insert_with(HashMap::new);
        // capacity: drop oldest finished shells first
        while map.len() >= BG_SHELL_CAP {
            if let Some(oldest_done) = map
                .iter()
                .filter(|(_, s)| s.done.load(std::sync::atomic::Ordering::Relaxed))
                .map(|(k, _)| *k)
                .min()
            {
                map.remove(&oldest_done);
            } else {
                break;
            }
        }
        map.insert(id, shell);
    }

    // poll thread: flip done + record the exit code
    // (child was moved into the registry above; fetch it back for polling)
    std::thread::spawn(move || loop {
        let finished = {
            let guard = bg_lock();
            let map = guard.as_ref().unwrap();
            match map.get(&id) {
                None => true, // evicted: stop polling
                Some(s) => {
                    let mut c = s.child.lock().unwrap_or_else(|p| p.into_inner());
                    match c.try_wait() {
                        Ok(Some(st)) => {
                            *s.exit_code.lock().unwrap_or_else(|p| p.into_inner()) = st.code();
                            true
                        }
                        _ => false,
                    }
                }
            }
        };
        if finished {
            let guard = bg_lock();
            if let Some(s) = guard.as_ref().and_then(|m| m.get(&id)) {
                s.done.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    });

    // wire the pipes to the registry entry's buffer
    pipe_into_shell(id, stdout);
    pipe_into_shell(id, stderr);
    Ok(id)
}

/// Continuously append one pipe's bytes into a shell's output buffer until
/// the stream ends (process exit or kill).
fn pipe_into_shell<R: std::io::Read + Send + 'static>(id: u32, r: R) {
    std::thread::spawn(move || {
        let mut r = r;
        let mut chunk = [0u8; 4096];
        loop {
            match r.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let guard = bg_lock();
                    let Some(s) = guard.as_ref().and_then(|m| m.get(&id)) else { break };
                    let mut b = s.out.lock().unwrap_or_else(|p| p.into_inner());
                    if b.bytes.len() + n > BG_BUF_CAP {
                        let keep = BG_BUF_CAP * 3 / 4;
                        let cut = b.bytes.len().saturating_sub(keep);
                        b.bytes.drain(..cut);
                        b.read_pos = b.read_pos.saturating_sub(cut);
                    }
                    b.bytes.extend_from_slice(&chunk[..n]);
                }
            }
        }
    });
}

/// read_background_output tool: incremental new bytes since the last call
/// plus the shell's liveness state. Unknown id ⇒ an explanatory error.
pub fn read_background_output(shell_id: u32) -> Result<String, String> {
    let guard = bg_lock();
    let map = guard.as_ref().ok_or("shell_id 不存在 —— 没有任何后台 shell")?;
    let s = map.get(&shell_id).ok_or_else(|| format!("shell_id {shell_id} 不存在（可能已被清理）"))?;
    let mut b = s.out.lock().unwrap_or_else(|p| p.into_inner());
    let new_bytes: Vec<u8> = b.bytes[b.read_pos.min(b.bytes.len())..].to_vec();
    b.read_pos = b.bytes.len();
    drop(b);
    let state = if s.done.load(std::sync::atomic::Ordering::Relaxed) {
        let code = *s.exit_code.lock().unwrap_or_else(|p| p.into_inner());
        match code {
            Some(0) => "exited(0) — 已成功结束".to_string(),
            Some(c) => format!("exited({c}) — 已结束"),
            None => "exited(被终止或信号退出)".to_string(),
        }
    } else {
        "running — 仍在运行".to_string()
    };
    let text = String::from_utf8_lossy(&new_bytes);
    let body = if text.trim().is_empty() {
        "（无新增输出）".to_string()
    } else {
        text.chars().take(MAX_TOOL_RESULT_CHARS).collect()
    };
    Ok(format!("[{state}]\n{body}"))
}

/// kill_background tool: terminate a still-running shell. Returns a status
/// line either way; the entry stays readable until it is evicted.
pub fn kill_background(shell_id: u32) -> Result<String, String> {
    let guard = bg_lock();
    let map = guard.as_ref().ok_or("shell_id 不存在 —— 没有任何后台 shell")?;
    let s = map.get(&shell_id).ok_or_else(|| format!("shell_id {shell_id} 不存在（可能已被清理）"))?;
    if s.done.load(std::sync::atomic::Ordering::Relaxed) {
        return Ok(format!("shell {shell_id} 已退出，无需终止"));
    }
    let mut c = s.child.lock().unwrap_or_else(|p| p.into_inner());
    match c.kill() {
        Ok(()) => Ok(format!("shell {shell_id}（{}）已终止", s.command)),
        Err(e) => Err(format!("终止失败: {e}")),
    }
}

/// Post-write verification hook (better-harness style feedback loop): run the
/// user-configured command once at the workspace root right after a
/// successful file-mutating tool, and return a bounded report block to append
/// to the tool result so the model sees the verification result immediately.
/// Returns None when the hook is disabled or there is no workspace.
pub fn post_write_verify(workspace: &str, command: &str) -> Option<String> {
    let cmd = command.trim();
    if cmd.is_empty() || workspace.is_empty() {
        return None;
    }
    // 90s ceiling: verification loops (build/test/lint) can be slow, but this
    // runs inline inside the tool loop and must not stall the turn.
    match run_command(workspace, cmd, 90) {
        Ok(body) => Some(format!("\n\n## 写后自动验证（post_write_command）\n{body}")),
        Err(e) => Some(format!(
            "\n\n## 写后自动验证（post_write_command）\nERROR: {e}"
        )),
    }
}

/// Fetch a public URL and return readable plain text. Runs on a dedicated
/// thread (reqwest blocking + join) because tool execution is sync inside an
/// async command. SSRF: the same urlguard policy as provider endpoints, plus
/// a per-redirect check so a public URL can't bounce into private space.
fn web_fetch(url: &str) -> Result<String, String> {
    const FETCH_TIMEOUT_SECS: u64 = 20;
    const MAX_BODY_BYTES: usize = 512 * 1024;
    let url = url.trim();
    if url.is_empty() {
        return Err("URL 为空".into());
    }
    if let crate::urlguard::UrlCheck::Refused(e) = crate::urlguard::check_base_url(url, false) {
        return Err(format!("URL 被拒绝: {e}"));
    }

    // dedicated thread: blocking client would otherwise run on a tokio worker
    let url_owned = url.to_string();
    let handle = std::thread::spawn(move || -> Result<(String, String), String> {
        // per-hop resolve-then-pin: for the initial URL AND every redirect
        // hop, resolve the host, vet EVERY answer against the private-net
        // policy, and pin the connection to the first vetted address — so
        // neither DNS rebinding nor a redirect bounce can land on an
        // address we never vetted (string-only hop checks lose to a name
        // that resolves loopback, incl. ::ffff:-mapped answers).
        use std::net::ToSocketAddrs;
        const MAX_HOPS: usize = 5;
        let mut current = reqwest::Url::parse(&url_owned).map_err(|e| format!("URL 无效: {e}"))?;
        for hop in 0..=MAX_HOPS {
            let host = current
                .host_str()
                .ok_or("URL 缺少主机名")?
                .trim_matches(['[', ']'])
                .to_string();
            if crate::urlguard::is_loopback_or_private(&host) {
                return Err(format!("目标 {host} 位于本机/内网，请求已拒绝"));
            }
            let port = current.port_or_known_default().unwrap_or(80);
            let mut pinned: Option<std::net::SocketAddr> = None;
            for sa in (host.as_str(), port)
                .to_socket_addrs()
                .map_err(|e| format!("DNS 解析失败: {e}"))?
            {
                if crate::urlguard::is_loopback_or_private(&sa.ip().to_string()) {
                    return Err(format!("解析结果 {} 位于本机/内网，请求已拒绝", sa.ip()));
                }
                if pinned.is_none() {
                    pinned = Some(sa);
                }
            }
            let pinned = pinned.ok_or_else(|| "DNS 解析未返回地址".to_string())?;
            let client = reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(FETCH_TIMEOUT_SECS))
                .resolve(&host, pinned)
                .redirect(reqwest::redirect::Policy::none())
                .user_agent("CCHarness/0.1 (agent web_fetch)")
                .build()
                .map_err(|e| format!("HTTP 客户端构建失败: {e}"))?;
            let resp = client
                .get(current.clone())
                .timeout(std::time::Duration::from_secs(FETCH_TIMEOUT_SECS))
                .send()
                .map_err(|e| format!("请求失败: {e}"))?;
            if resp.status().is_redirection() {
                let loc = resp
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or("重定向缺少 Location 头")?
                    .to_string();
                if hop == MAX_HOPS {
                    return Err("重定向过多".to_string());
                }
                let next = current.join(&loc).map_err(|e| format!("非法重定向目标: {e}"))?;
                if next.scheme() != "http" && next.scheme() != "https" {
                    return Err(format!("重定向协议不被支持: {}", next.scheme()));
                }
                current = next;
                continue;
            }
            let status = resp.status();
            let ctype = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let bytes = resp.bytes().map_err(|e| format!("读取响应失败: {e}"))?;
            if bytes.len() > MAX_BODY_BYTES {
                return Err(format!("响应超过 {MAX_BODY_BYTES} 字节上限"));
            }
            return Ok((format!("{status} {ctype}"), String::from_utf8_lossy(&bytes).to_string()));
        }
        unreachable!("redirect hops are bounded")
    });
    let (meta, body) = handle.join().map_err(|_| "抓取线程崩溃".to_string())??;

    let text = if meta.contains("text/html") {
        html_to_text(&body)
    } else {
        body
    };
    if text.trim().is_empty() {
        return Err("响应正文为空".into());
    }
    Ok(format!("URL: {url}\n[{meta}]\n\n{}", text.trim()))
}

/// Case-insensitive ASCII substring search (tag names are ASCII; avoids
/// `to_lowercase` byte-length drift on the haystack).
fn find_ci(hay: &str, needle: &str) -> Option<usize> {
    let n = needle.len();
    if n == 0 || hay.len() < n {
        return None;
    }
    let hb = hay.as_bytes();
    let nb = needle.as_bytes();
    (0..=hb.len() - n).find(|&i| hb[i..i + n].eq_ignore_ascii_case(nb))
}

/// Crude HTML → text: drop comments / script / style blocks, strip tags,
/// collapse whitespace. Good enough for model consumption.
fn html_to_text(html: &str) -> String {
    let mut s = html.to_string();
    for tag in ["script", "style"] {
        loop {
            let open = match find_ci(&s, &format!("<{tag}")) {
                Some(i) => i,
                None => break,
            };
            let close = match find_ci(&s[open..], &format!("</{tag}>")) {
                Some(e) => open + e + tag.len() + 3,
                None => break,
            };
            s.replace_range(open..close.min(s.len()), " ");
        }
    }
    // strip comments
    while let Some(i) = s.find("<!--") {
        match s[i..].find("-->") {
            Some(e) => s.replace_range(i..i + e + 3, " "),
            None => {
                s.truncate(i);
                break;
            }
        }
    }
    // strip tags
    let mut out = String::with_capacity(s.len() / 2);
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    // collapse whitespace runs
    let mut collapsed = String::with_capacity(out.len());
    let mut prev_space = false;
    for ch in out.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                collapsed.push(' ');
            }
            prev_space = true;
        } else {
            collapsed.push(ch);
            prev_space = false;
        }
    }
    collapsed.trim().to_string()
}

/// Sequential unique-replace: each hunk's old_text must appear exactly once
/// in the file as mutated by the hunks before it. All-or-nothing per file:
/// a failing hunk aborts without writing.
fn apply_patch(workspace: &str, rel: &str, hunks: &[Value]) -> Result<String, String> {
    if hunks.is_empty() {
        return Err("hunks 为空".into());
    }
    if hunks.len() > 50 {
        return Err("hunks 超过 50 处上限".into());
    }
    let path = resolve_in_workspace(workspace, rel)?;
    refuse_symlink_target(&path, rel)?;
    if path.is_dir() {
        return Err(format!("{rel} 是目录"));
    }
    let mut content = fs::read_to_string(&path).map_err(|e| format!("读取失败: {e}"))?;
    for (i, h) in hunks.iter().enumerate() {
        let old = h.get("old_text").and_then(|v| v.as_str()).unwrap_or("");
        let new = h.get("new_text").and_then(|v| v.as_str()).unwrap_or("");
        if old.is_empty() {
            return Err(format!("第 {} 处替换的 old_text 为空", i + 1));
        }
        let n = content.matches(old).count();
        if n != 1 {
            return Err(format!(
                "第 {} 处替换失败：old_text 出现 {n} 次（须为 1）；请提供更长的上下文",
                i + 1
            ));
        }
        content = content.replacen(old, new, 1);
    }
    let newline_normalized = content.replace("\r\n", "\n");
    fs::write(&path, &newline_normalized).map_err(|e| format!("写入失败: {e}"))?;
    Ok(format!(
        "OK: {rel} 应用 {} 处替换（现 {} B，{} 行）",
        hunks.len(),
        newline_normalized.len(),
        newline_normalized.lines().count()
    ))
}

fn delete_file(workspace: &str, rel: &str) -> Result<String, String> {
    if rel.trim().is_empty() || rel.trim() == "." {
        return Err("拒绝删除工作区根".into());
    }
    let path = resolve_in_workspace(workspace, rel)?;
    refuse_symlink_target(&path, rel)?;
    if path.is_dir() {
        return Err(format!("{rel} 是目录 —— delete_file 只接受文件（防误删整棵子树）"));
    }
    let size = fs::metadata(&path).map(|m| m.len()).map_err(|e| format!("文件不存在: {e}"))?;
    fs::remove_file(&path).map_err(|e| format!("删除失败: {e}"))?;
    Ok(format!("OK: 已删除 {rel}（{size} B）"))
}

fn move_path(workspace: &str, from: &str, to: &str) -> Result<String, String> {
    if from.trim().is_empty() || to.trim().is_empty() || from.trim() == "." || to.trim() == "." {
        return Err("from/to 不能为空或工作区根".into());
    }
    let src = resolve_in_workspace(workspace, from)?;
    let dst = resolve_in_workspace(workspace, to)?;
    refuse_symlink_target(&src, from)?;
    refuse_symlink_target(&dst, to)?;
    if !src.exists() {
        return Err(format!("源不存在: {from}"));
    }
    if dst.exists() {
        return Err(format!("目标已存在，拒绝覆盖: {to}"));
    }
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建目标目录失败: {e}"))?;
    }
    fs::rename(&src, &dst).map_err(|e| format!("移动失败（跨盘移动不支持时请改用复制）: {e}"))?;
    Ok(format!("OK: 已移动 {from} → {to}"))
}

fn write_file(workspace: &str, rel: &str, content: &str) -> Result<String, String> {
    if content.len() > WRITE_FILE_CAP {
        return Err(format!("内容超过 1MB 上限（{} B）", content.len()));
    }
    if content.contains('\0') {
        return Err("内容包含 NUL，疑似二进制".into());
    }
    let path = resolve_in_workspace(workspace, rel)?;
    refuse_symlink_target(&path, rel)?;
    if path.is_dir() {
        return Err(format!("{rel} 是目录"));
    }
    let existed = path.exists();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {e}"))?;
    }
    // refuse to clobber a binary file
    if existed {
        if let Ok(head) = fs::read(&path).map(|b| b[..b.len().min(8)].to_vec()) {
            if head.contains(&0) {
                return Err("目标疑似二进制文件，拒绝覆写".into());
            }
        }
    }
    let newline_normalized = content.replace("\r\n", "\n");
    fs::write(&path, &newline_normalized).map_err(|e| format!("写入失败: {e}"))?;
    Ok(format!(
        "OK: {} {}（{} B，{} 行）",
        if existed { "覆写" } else { "创建" },
        rel,
        newline_normalized.len(),
        newline_normalized.lines().count()
    ))
}

fn edit_file(workspace: &str, rel: &str, old_text: &str, new_text: &str) -> Result<String, String> {
    if old_text.is_empty() {
        return Err("old_text 不能为空".into());
    }
    let path = resolve_in_workspace(workspace, rel)?;
    refuse_symlink_target(&path, rel)?;
    let current = fs::read_to_string(&path).map_err(|e| format!("读取失败: {e}"))?;
    let n = current.matches(old_text).count();
    if n == 0 {
        return Err("old_text 在文件中不存在（可能已应用过）".into());
    }
    if n > 1 {
        return Err(format!("old_text 出现 {n} 次，不唯一——请带更多上下文重试"));
    }
    let updated = current.replacen(old_text, new_text, 1);
    fs::write(&path, updated).map_err(|e| format!("写入失败: {e}"))?;
    Ok(format!(
        "OK: 编辑 {}（-{} / +{} 字符）",
        rel,
        old_text.len(),
        new_text.len()
    ))
}

fn str_arg(args: &Value, key: &str) -> String {
    args.get(key).and_then(|v| v.as_str()).unwrap_or("").to_string()
}

fn list_dir(workspace: &str, rel: &str) -> Result<String, String> {
    let dir = resolve_in_workspace(workspace, rel)?;
    let mut dirs: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    let entries = fs::read_dir(&dir).map_err(|e| format!("无法读取目录: {e}"))?;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            dirs.push(format!("d  {name}/"));
        } else {
            let size = e.metadata().map(|m| m.len()).unwrap_or(0);
            files.push(format!("f  {name}  ({size} B)"));
        }
    }
    dirs.sort();
    files.sort();
    let mut lines: Vec<String> = dirs.into_iter().chain(files).collect();
    if lines.is_empty() {
        return Ok("(空目录)".into());
    }
    if lines.len() > LIST_CAP {
        lines.truncate(LIST_CAP);
        lines.push(format!("…[共超过 {LIST_CAP} 项，已截断]"));
    }
    Ok(lines.join("\n"))
}

fn read_file(workspace: &str, rel: &str) -> Result<String, String> {
    // spill allowlist: absolute paths under the spill root (written only by
    // the spill subsystem itself, shape-gated) are readable so the model can
    // retrieve a spilled tool output. No other escape from the workspace.
    if let Some(sp) = crate::spill::resolve_spill_file(rel) {
        let meta = fs::metadata(&sp).map_err(|e| format!("无法读取文件: {e}"))?;
        if meta.is_dir() {
            return Err("spill 路径指向目录".into());
        }
        let cap = READ_FILE_CAP.min(meta.len());
        let f = fs::read(&sp).map_err(|e| format!("读取失败: {e}"))?;
        if f.get(..8).is_some_and(|head| head.contains(&0)) {
            return Err("疑似二进制文件，已拒绝读取".into());
        }
        let text = String::from_utf8_lossy(&f[..cap as usize]);
        let mut out = text.to_string();
        if meta.len() > READ_FILE_CAP {
            out.push_str("\n…[文件超过 256KB，已截断]");
        }
        return Ok(out);
    }
    let path = resolve_in_workspace(workspace, rel)?;
    let meta = fs::metadata(&path).map_err(|e| format!("无法读取文件: {e}"))?;
    if meta.is_dir() {
        return Err(format!("{rel} 是目录，read_file 只接受文件（可用 list_dir 列目录）"));
    }
    let cap = READ_FILE_CAP.min(meta.len());
    let f = fs::read(&path).map_err(|e| format!("读取失败: {e}"))?;
    if f.get(..8).is_some_and(|head| head.contains(&0)) {
        return Err("疑似二进制文件，已拒绝读取".into());
    }
    let text = String::from_utf8_lossy(&f[..cap as usize]);
    let mut out = text.to_string();
    if meta.len() > READ_FILE_CAP {
        out.push_str("\n…[文件超过 256KB，已截断]");
    }
    Ok(out)
}

fn glob_files(workspace: &str, pattern: &str) -> Result<String, String> {
    if pattern.trim().is_empty() {
        return Err("pattern 不能为空".into());
    }
    // the pattern is model-supplied and concatenated onto the workspace
    // root: a `..` component or an absolute / drive prefix would walk the
    // glob outside the workspace and list foreign file names
    let raw = pattern.trim();
    if raw.starts_with('/') || raw.starts_with('\\') || raw.contains('\0') {
        return Err("pattern 必须是相对工作区的路径".into());
    }
    let pat = raw.trim_start_matches(['/', '\\']);
    if pat.split(['/', '\\']).any(|c| c == "..") {
        return Err("pattern 不允许包含 .. 路径组件".into());
    }
    if Path::new(pat).is_absolute() {
        return Err("pattern 必须是相对工作区的路径".into());
    }
    #[cfg(windows)]
    if pat.as_bytes().len() >= 2
        && pat.as_bytes()[1] == b':'
        && pat.as_bytes()[0].is_ascii_alphabetic()
    {
        return Err("pattern 必须是相对工作区的路径（不允许盘符前缀）".into());
    }
    let base = workspace.trim_end_matches(['/', '\\']);
    let full = format!("{base}/{pat}");
    // belt-and-braces: keep only entries that really resolve inside the
    // workspace (a symlinked directory inside it could still redirect the
    // glob walk); unresolvable entries are skipped, not surfaced
    let ws_canon = fs::canonicalize(workspace).ok();
    let mut out: Vec<String> = Vec::new();
    for entry in glob::glob(&full).map_err(|e| format!("非法 glob 模式: {e}"))?.flatten() {
        if let Some(ws_canon) = &ws_canon {
            match entry.canonicalize() {
                Ok(real) if real.starts_with(ws_canon) => {}
                _ => continue,
            }
        }
        let shown = entry
            .strip_prefix(workspace)
            .unwrap_or(&entry)
            .to_string_lossy()
            .trim_start_matches(['/', '\\'])
            .to_string();
        out.push(shown);
        if out.len() >= GLOB_CAP {
            out.push(format!("…[超过 {GLOB_CAP} 项，已截断]"));
            break;
        }
    }
    if out.is_empty() {
        return Ok("(无匹配)".into());
    }
    Ok(out.join("\n"))
}

fn grep_files(workspace: &str, pattern: &str, glob_filter: &str) -> Result<String, String> {
    if pattern.is_empty() {
        return Err("pattern 不能为空".into());
    }
    let needle = pattern.to_lowercase();
    let matcher = if glob_filter.trim().is_empty() {
        None
    } else {
        Some(
            glob::Pattern::new(&glob_filter.trim_start_matches(['/', '\\']))
                .map_err(|e| format!("非法 glob: {e}"))?,
        )
    };
    let mut matches: Vec<String> = Vec::new();
    let mut scanned = 0usize;
    walk_text_files(Path::new(workspace), Path::new(workspace), &mut |rel, path| {
        if scanned >= GREP_FILE_CAP || matches.len() >= GREP_MATCH_CAP {
            return;
        }
        if let Some(m) = &matcher {
            if !m.matches(rel) {
                return;
            }
        }
        scanned += 1;
        if let Ok(content) = fs::read_to_string(path) {
            for (i, line) in content.lines().enumerate() {
                if line.to_lowercase().contains(&needle) {
                    let trimmed: String = line.trim().chars().take(200).collect();
                    matches.push(format!("{rel}:{}: {trimmed}", i + 1));
                    if matches.len() >= GREP_MATCH_CAP {
                        return;
                    }
                }
            }
        }
    });
    let mut out = matches.join("\n");
    if scanned >= GREP_FILE_CAP {
        out.push_str(&format!("\n…[扫描文件数达到上限 {GREP_FILE_CAP}]"));
    }
    if out.is_empty() {
        return Ok("(无匹配)".into());
    }
    Ok(out)
}

fn walk_text_files(root: &Path, dir: &Path, visit: &mut impl FnMut(&str, &Path)) {
    // canonical boundary: a symlinked/junctioned DIRECTORY inside the
    // workspace could redirect the whole traversal outside it and feed
    // foreign files into model context (glob_files and read_file already
    // guard this — the walk was the last unguarded traversal). The root
    // resolves once; every directory recursed into and every file visited
    // must canonicalize back inside it. Unresolvable entries are skipped.
    let Ok(root_canon) = fs::canonicalize(root) else { return };
    walk_text_files_inner(&root_canon, dir, visit);
}

fn walk_text_files_inner(root_canon: &Path, dir: &Path, visit: &mut impl FnMut(&str, &Path)) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if p.is_dir() {
            if SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            match fs::canonicalize(&p) {
                Ok(real) if real.starts_with(root_canon) => {}
                _ => continue,
            }
            walk_text_files_inner(root_canon, &p, visit);
        } else {
            // file symlinks too: the content reader follows links, so the
            // resolved target must sit inside the workspace as well — and
            // the visitor reads the RESOLVED path, not the link
            let Ok(real) = fs::canonicalize(&p) else { continue };
            if !real.starts_with(root_canon) {
                continue;
            }
            let rel = real
                .strip_prefix(root_canon)
                .unwrap_or(&real)
                .to_string_lossy()
                .replace('\\', "/");
            visit(&rel, &real);
        }
    }
}

/// Public read wrapper for the composer's @-file references (same path
/// guard and truncation as the read_file tool).
pub fn read_file_public(workspace: &str, rel: &str) -> Result<String, String> {
    read_file(workspace, rel)
}

/// Workspace-relative file paths for the @-reference completion menu.
/// Query matches case-insensitively against the path; name-prefix matches
/// rank first. Capped at 30 entries.
pub fn search_files(workspace: &str, query: &str) -> Vec<String> {
    let mut all: Vec<String> = Vec::new();
    walk_text_files(Path::new(workspace), Path::new(workspace), &mut |rel, _| {
        all.push(rel.to_string());
    });
    let q = query.to_lowercase();
    let mut scored: Vec<(usize, usize, String)> = all
        .into_iter()
        .filter(|rel| q.is_empty() || rel.to_lowercase().contains(&q))
        .map(|rel| {
            let name = rel.rsplit(['/', '\\']).next().unwrap_or(&rel).to_lowercase();
            let score = if name.starts_with(&q) {
                0
            } else if name.contains(&q) {
                1
            } else {
                2
            };
            // same score: the shorter (more exact) name ranks first
            (score, name.len(), rel)
        })
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)).then_with(|| a.2.cmp(&b.2)));
    scored.truncate(30);
    scored.into_iter().map(|(_, _, rel)| rel).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_shell_lifecycle() {
        // spawn an instant echo in a temp workspace, poll reads until the
        // shell reports exit, and confirm the marker text was observed
        let ws = std::env::temp_dir().join(format!("cch_bg_test_{}", std::process::id()));
        fs::create_dir_all(&ws).unwrap();
        let id = spawn_background(ws.to_str().unwrap(), "echo bg-ok-marker").unwrap();
        let mut saw_marker = false;
        let mut finished = false;
        for _ in 0..50 {
            let out = read_background_output(id).unwrap();
            if out.contains("bg-ok-marker") {
                saw_marker = true;
            }
            if out.contains("已结束") || out.contains("已成功结束") {
                finished = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        assert!(saw_marker, "echo 输出未被捕获");
        assert!(finished, "shell 未在时限内退出");
        // killing an already-exited shell is a graceful no-op
        assert!(kill_background(id).unwrap().contains("已退出"));
    }

    #[test]
    fn rejects_escape_paths() {
        // platform-neutral: these guards must hold on every filesystem
        let (ws, outside) = if cfg!(windows) {
            ("C:/tmp/ws", "C:/Windows/win.ini")
        } else {
            ("/tmp/ws", "/etc/passwd")
        };
        assert!(resolve_in_workspace(ws, "../outside.txt").is_err());
        assert!(resolve_in_workspace(ws, "a/../../b").is_err());
        assert!(resolve_in_workspace(ws, outside).is_err());
    }

    #[test]
    fn rejects_absolute_parent_escape() {
        // lexical prefix matches the workspace, but the OS resolves the
        // ParentDirs outside — a component-level starts_with alone must
        // never wave this through (P0: absolute `..` traversal)
        let (ws, escape, outside) = if cfg!(windows) {
            (
                "C:/tmp/ws",
                "C:/tmp/ws/../../Users/x/.ssh/id_rsa",
                "C:/Windows/win.ini",
            )
        } else {
            ("/tmp/ws", "/tmp/ws/../../etc/shadow", "/etc/passwd")
        };
        assert!(resolve_in_workspace(ws, escape).is_err());
        // empty workspace: absolute candidates have no boundary → refuse
        assert!(resolve_in_workspace("", outside).is_err());
    }

    #[test]
    fn walk_skips_directory_links_outside_workspace() {
        let ws = std::env::temp_dir().join(format!("cch_walk_ws_{}", std::process::id()));
        let out = std::env::temp_dir().join(format!("cch_walk_out_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&out);
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("secret.txt"), "outside\n").unwrap();
        std::fs::write(ws.join("inside.txt"), "inside\n").unwrap();
        // junction/symlink the out dir into the workspace
        #[cfg(windows)]
        let linked = {
            use std::os::windows::process::CommandExt;
            let st = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(ws.join("link"))
                .arg(&out)
                .creation_flags(0x0800_0000)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            st && ws.join("link").is_dir()
        };
        #[cfg(not(windows))]
        let linked = std::os::unix::fs::symlink(&out, ws.join("link")).is_ok();
        if !linked {
            // sandbox refused the link — the guard is untestable here
            let _ = std::fs::remove_dir_all(&ws);
            let _ = std::fs::remove_dir_all(&out);
            return;
        }
        let mut seen: Vec<String> = Vec::new();
        walk_text_files(Path::new(&ws), Path::new(&ws), &mut |rel, _| {
            seen.push(rel.to_string());
        });
        assert!(seen.iter().any(|r| r.ends_with("inside.txt")), "{seen:?}");
        assert!(
            !seen.iter().any(|r| r.contains("secret")),
            "walk escaped through the directory link: {seen:?}"
        );
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn symlink_escape_is_refused() {
        // P0: a symlink inside the workspace must never carry a tool's
        // write outside. Cases where the OS refuses symlink creation
        // (Windows without privilege / developer mode) degrade to the
        // plain-write assertion only.
        let ws = std::env::temp_dir().join(format!("cch_sym_ws_{}", std::process::id()));
        let out = std::env::temp_dir().join(format!("cch_sym_out_{}", std::process::id()));
        let _ = fs::remove_dir_all(&ws);
        let _ = fs::remove_dir_all(&out);
        fs::create_dir_all(&ws).unwrap();
        fs::create_dir_all(&out).unwrap();

        // Case A: dangling link as the final component — canonicalize
        // cannot resolve it, so the write-side symlink rejection must fire
        // (fs::write through it would create the file at the destination).
        let target = out.join("created_outside.txt");
        let made_a = make_symlink(&target, &ws.join("dangling.txt"));
        if made_a {
            let r = execute_write(ws.to_str().unwrap(), "write_file", &serde_json::json!({
                "path": "dangling.txt", "content": "escape"
            }));
            assert!(r.contains("符号链接"), "{r}");
            assert!(!target.exists(), "悬空链接不应被写入");
        }

        // Case B: symlinked directory inside the workspace pointing at a
        // real outside directory — the canonical ancestor check must refuse
        // even though the lexical path (in/new.txt) looks inside.
        fs::write(out.join("secret.txt"), "outside").unwrap();
        let made_b = make_symlink(&out, &ws.join("in"));
        if made_b {
            let r = execute_write(ws.to_str().unwrap(), "write_file", &serde_json::json!({
                "path": "in/new.txt", "content": "escape"
            }));
            assert!(r.contains("符号链接"), "{r}");
            assert!(!out.join("new.txt").exists(), "目录链接不应被写入");
        }

        // normal relative writes remain unaffected
        let ok = execute_write(ws.to_str().unwrap(), "write_file", &serde_json::json!({
            "path": "plain.txt", "content": "inside"
        }));
        assert!(ok.starts_with("OK"), "{ok}");

        let _ = fs::remove_dir_all(&ws);
        let _ = fs::remove_dir_all(&out);
    }

    /// create a symlink (dir- or file-shaped on Windows); false when the
    /// platform or privilege level refuses. The result is *verified*:
    /// sandboxed environments (filter drivers, virtualized FS) have been
    /// observed to report success without materializing the link, which
    /// would otherwise silently downgrade the escape test to a no-op.
    fn make_symlink(target: &std::path::Path, link: &std::path::Path) -> bool {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
                && fs::symlink_metadata(link)
                    .map(|m| m.is_symlink())
                    .unwrap_or(false)
        }
        #[cfg(windows)]
        {
            let made = if target.is_dir() {
                std::os::windows::fs::symlink_dir(target, link).is_ok()
            } else {
                std::os::windows::fs::symlink_file(target, link).is_ok()
            };
            made
                && fs::symlink_metadata(link)
                    .map(|m| m.is_symlink())
                    .unwrap_or(false)
        }
    }

    #[test]
    fn apply_patch_requires_unique_hunks() {
        let ws = std::env::temp_dir().join(format!("cch_test_{}", std::process::id()));
        fs::create_dir_all(&ws).unwrap();
        let _ = execute_write(ws.to_str().unwrap(), "write_file", &serde_json::json!({
            "path": "patch.txt", "content": "alpha\nbeta\ngamma\n"
        }));
        // ambiguous hunk → refuse without writing
        let bad = execute_write(ws.to_str().unwrap(), "apply_patch", &serde_json::json!({
            "path": "patch.txt",
            "hunks": [{ "old_text": "a", "new_text": "X" }]
        }));
        assert!(bad.starts_with("ERROR"));
        // two clean hunks → applied in order
        let ok = execute_write(ws.to_str().unwrap(), "apply_patch", &serde_json::json!({
            "path": "patch.txt",
            "hunks": [
                { "old_text": "alpha", "new_text": "ALPHA" },
                { "old_text": "gamma", "new_text": "GAMMA" }
            ]
        }));
        assert!(ok.starts_with("OK"), "{ok}");
        let cur = fs::read_to_string(ws.join("patch.txt")).unwrap();
        assert_eq!(cur, "ALPHA\nbeta\nGAMMA\n");
        let _ = fs::remove_dir_all(&ws);
    }

    #[test]
    fn delete_and_move_roundtrip() {
        let ws = std::env::temp_dir().join(format!("cch_test_mv_{}", std::process::id()));
        fs::create_dir_all(&ws).unwrap();
        let w = ws.to_str().unwrap();
        let _ = execute_write(w, "write_file", &serde_json::json!({ "path": "a/f.txt", "content": "hi" }));
        assert!(execute_write(w, "move_path", &serde_json::json!({ "from": "a/f.txt", "to": "b/g.txt" })).starts_with("OK"));
        assert!(!ws.join("a/f.txt").exists() && ws.join("b/g.txt").exists());
        // target-exists refusal
        let _ = execute_write(w, "write_file", &serde_json::json!({ "path": "b/h.txt", "content": "x" }));
        assert!(execute_write(w, "move_path", &serde_json::json!({ "from": "b/g.txt", "to": "b/h.txt" })).starts_with("ERROR"));
        assert!(execute_write(w, "delete_file", &serde_json::json!({ "path": "b/g.txt" })).starts_with("OK"));
        assert!(!ws.join("b/g.txt").exists());
        // dir deletion refused
        assert!(execute_write(w, "delete_file", &serde_json::json!({ "path": "b" })).starts_with("ERROR"));
        let _ = fs::remove_dir_all(&ws);
    }

    #[test]
    fn web_fetch_guard_refuses_private() {
        let out = execute("C:/tmp/nowhere", "web_fetch", &serde_json::json!({ "url": "http://127.0.0.1:9/x" }));
        assert!(out.contains("ERROR"), "{out}");
    }

    #[test]
    fn accepts_inside_paths() {
        assert!(resolve_in_workspace("C:/tmp/ws", "src/main.rs").is_ok());
        assert!(resolve_in_workspace("C:/tmp/ws", "").is_ok());
        assert!(resolve_in_workspace("C:/tmp/ws", "C:/tmp/ws/src/main.rs").is_ok());
    }

    #[test]
    fn schema_is_stable() {
        let a = serde_json::to_string(&schema()).unwrap();
        let b = serde_json::to_string(&schema()).unwrap();
        assert_eq!(a, b);
        assert!(a.contains("read_file"));
        assert!(a.contains("write_file"));
        assert!(a.contains("edit_file"));
    }

    #[test]
    fn write_and_edit_roundtrip() {
        let ws = std::env::temp_dir().join("ccharness-tool-test");
        let _ = fs::remove_dir_all(&ws);
        fs::create_dir_all(&ws).unwrap();
        let wstr = ws.to_str().unwrap().to_string();

        // write into a nested path
        let args = serde_json::json!({"path": "notes/deep/hello.txt", "content": "line1\nline2\n"});
        let out = execute_write(&wstr, "write_file", &args);
        assert!(out.starts_with("OK: 创建"), "{out}");
        assert_eq!(fs::read_to_string(ws.join("notes/deep/hello.txt")).unwrap(), "line1\nline2\n");

        // unique edit applies once
        let edit = serde_json::json!({"path": "notes/deep/hello.txt", "old_text": "line2", "new_text": "LINE-TWO"});
        let out = execute_write(&wstr, "edit_file", &edit);
        assert!(out.starts_with("OK: 编辑"), "{out}");
        assert_eq!(
            fs::read_to_string(ws.join("notes/deep/hello.txt")).unwrap(),
            "line1\nLINE-TWO\n"
        );

        // non-unique edit refuses
        fs::write(ws.join("notes/deep/hello.txt"), "aa aa aa").unwrap();
        let edit = serde_json::json!({"path": "notes/deep/hello.txt", "old_text": "aa", "new_text": "bb"});
        let out = execute_write(&wstr, "edit_file", &edit);
        assert!(out.contains("不唯一"), "{out}");

        // missing old_text refuses
        let edit = serde_json::json!({"path": "notes/deep/hello.txt", "old_text": "zzz", "new_text": "bb"});
        let out = execute_write(&wstr, "edit_file", &edit);
        assert!(out.contains("不存在"), "{out}");

        // write cannot escape the workspace
        let args = serde_json::json!({"path": "../escape.txt", "content": "x"});
        let out = execute_write(&wstr, "write_file", &args);
        assert!(out.contains("ERROR"), "{out}");
        assert!(!ws.parent().unwrap().join("escape.txt").exists());

        let _ = fs::remove_dir_all(&ws);
    }

    #[test]
    fn write_tools_flag() {
        assert!(is_write_tool("write_file"));
        assert!(is_write_tool("edit_file"));
        assert!(!is_write_tool("read_file"));
    }

    #[test]
    fn readonly_schema_excludes_delegation() {
        let ro = serde_json::to_string(&schema_readonly()).unwrap();
        assert!(!ro.contains("write_file"));
        assert!(!ro.contains("delegate_subagent"));
        assert!(ro.contains("read_file"));
        let full = serde_json::to_string(&schema()).unwrap();
        assert!(full.contains("delegate_subagent"));
    }

    #[test]
    fn search_files_ranks_and_caps() {
        let ws = std::env::temp_dir().join("ccharness-search-test");
        let _ = fs::remove_dir_all(&ws);
        fs::create_dir_all(ws.join("src/deep")).unwrap();
        fs::write(ws.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(ws.join("src/deep/main_helper.rs"), "x").unwrap();
        fs::write(ws.join("readme.md"), "y").unwrap();
        let wstr = ws.to_str().unwrap().to_string();

        let hits = search_files(&wstr, "main");
        assert_eq!(hits.len(), 2);
        assert!(hits[0].ends_with("main.rs"), "name-prefix match ranks first: {hits:?}");
        let all = search_files(&wstr, "");
        assert_eq!(all.len(), 3);
        assert!(search_files(&wstr, "不存在的关键词").is_empty());
        let _ = fs::remove_dir_all(&ws);
    }
}

// ---- sandbox 三态裁决（Allow / ForceAsk / Block）测试 --------------------
#[cfg(test)]
mod sandbox_tests {
    use super::*;
    use serde_json::json;

    /// sandbox_check / set_sandbox_policy touch the process-global SANDBOX:
    /// serialize the whole group so parallel test threads never interfere.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn policy(lists: serde_json::Value) -> SandboxPolicy {
        let mut p = SandboxPolicy {
            on: true,
            files: true,
            commands: true,
            network: true,
            file_deny: Vec::new(),
            file_allow: Vec::new(),
            cmd_deny: Vec::new(),
            cmd_allow: Vec::new(),
            cmd_ask: Vec::new(),
            net_deny: Vec::new(),
            net_allow: Vec::new(),
            net_block_all: false,
            net_malicious: false,
        };
        if let Some(v) = lists.get("file_deny").and_then(|v| v.as_array()) {
            p.file_deny = v.iter().filter_map(|x| x.as_str().map(String::from)).collect();
        }
        if let Some(v) = lists.get("file_allow").and_then(|v| v.as_array()) {
            p.file_allow = v.iter().filter_map(|x| x.as_str().map(String::from)).collect();
        }
        if let Some(v) = lists.get("cmd_deny").and_then(|v| v.as_array()) {
            p.cmd_deny = v.iter().filter_map(|x| x.as_str().map(String::from)).collect();
        }
        if let Some(v) = lists.get("cmd_allow").and_then(|v| v.as_array()) {
            p.cmd_allow = v.iter().filter_map(|x| x.as_str().map(String::from)).collect();
        }
        if let Some(v) = lists.get("cmd_ask").and_then(|v| v.as_array()) {
            p.cmd_ask = v.iter().filter_map(|x| x.as_str().map(String::from)).collect();
        }
        if let Some(v) = lists.get("net_deny").and_then(|v| v.as_array()) {
            p.net_deny = v.iter().filter_map(|x| x.as_str().map(String::from)).collect();
        }
        if let Some(v) = lists.get("net_allow").and_then(|v| v.as_array()) {
            p.net_allow = v.iter().filter_map(|x| x.as_str().map(String::from)).collect();
        }
        if let Some(v) = lists.get("net_block_all").and_then(|v| v.as_bool()) {
            p.net_block_all = v;
        }
        if let Some(v) = lists.get("net_malicious").and_then(|v| v.as_bool()) {
            p.net_malicious = v;
        }
        p
    }

    #[test]
    fn cmd_deny_blocks_by_program_name() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        set_sandbox_policy(policy(json!({ "cmd_deny": ["wsl", "schtasks"] })));
        assert!(matches!(
            sandbox_check("run_command", &json!({ "command": "wsl -d Ubuntu rm x" })),
            SandboxVerdict::Block(_)
        ));
        // 带路径 + .exe 也能按程序名匹配（大小写不敏感）
        assert!(matches!(
            sandbox_check("run_command", &json!({ "command": r#""C:\Windows\System32\SCHTASKS.EXE" /create ..."# })),
            SandboxVerdict::Block(_)
        ));
        // cmd.exe 转义与引号拼接不能洗白程序名：w^sl / w"s"l 都是 wsl
        assert!(matches!(
            sandbox_check("run_command", &json!({ "command": "w^sl -d Ubuntu rm x" })),
            SandboxVerdict::Block(_)
        ));
        assert!(matches!(
            sandbox_check("run_command", &json!({ "command": "w\"s\"l -d Ubuntu rm x" })),
            SandboxVerdict::Block(_)
        ));
        // 非名单程序不受影响
        assert_eq!(
            sandbox_check("run_command", &json!({ "command": "git status" })),
            SandboxVerdict::Allow
        );
    }

    #[test]
    fn blocklist_cannot_be_dodged_by_tab_or_escape() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        set_sandbox_policy(policy(json!({})));
        // TAB 代替空格，绕过空格锚定的黑名单片段
        assert!(matches!(
            sandbox_check("run_command", &json!({ "command": "rm\t-rf /" })),
            SandboxVerdict::Block(_)
        ));
        // ^ 转义拆散 "rm -rf"
        assert!(matches!(
            sandbox_check("run_command", &json!({ "command": "r^m -rf /" })),
            SandboxVerdict::Block(_)
        ));
        // 多空白变体
        assert!(matches!(
            sandbox_check("run_command", &json!({ "command": "git push --force  origin" })),
            SandboxVerdict::Block(_)
        ));
    }

    #[test]
    fn cmd_ask_forces_and_allow_overrides_blocklist() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        set_sandbox_policy(policy(json!({ "cmd_ask": ["docker"] })));
        assert_eq!(
            sandbox_check("run_command", &json!({ "command": "docker ps" })),
            SandboxVerdict::ForceAsk
        );
        // 内置高危黑名单默认拦截 curl
        set_sandbox_policy(policy(json!({})));
        assert!(matches!(
            sandbox_check("run_command", &json!({ "command": "curl https://x/y" })),
            SandboxVerdict::Block(_)
        ));
        // 显式允许名单跳过内置黑名单（用户自担风险的放行）
        set_sandbox_policy(policy(json!({ "cmd_allow": ["curl"] })));
        assert_eq!(
            sandbox_check("run_command", &json!({ "command": "curl https://x/y" })),
            SandboxVerdict::Allow
        );
        // 但 deny 名单优先级高于 allow
        set_sandbox_policy(policy(json!({ "cmd_allow": ["curl"], "cmd_deny": ["curl"] })));
        assert!(matches!(
            sandbox_check("run_command", &json!({ "command": "curl https://x/y" })),
            SandboxVerdict::Block(_)
        ));
    }

    #[test]
    fn file_deny_wildcard_and_delete() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        set_sandbox_policy(policy(json!({ "file_deny": ["secrets/*", "*.env"] })));
        assert!(matches!(
            sandbox_check("read_file", &json!({ "path": "secrets/api.txt" })),
            SandboxVerdict::Block(_)
        ));
        assert!(matches!(
            sandbox_check("write_file", &json!({ "path": "SRC/Prod.env" })),
            SandboxVerdict::Block(_)
        ));
        // move_path 的 from 与 to 都要检查
        assert!(matches!(
            sandbox_check("move_path", &json!({ "from": "a.txt", "to": "x/.env" })),
            SandboxVerdict::Block(_)
        ));
        // 未命中名单的正常路径放行
        assert_eq!(
            sandbox_check("write_file", &json!({ "path": "src/main.rs" })),
            SandboxVerdict::Allow
        );
        // 删除类工具一律拒绝（文件策略开启时）
        assert!(matches!(
            sandbox_check("delete_file", &json!({ "path": "src/main.rs" })),
            SandboxVerdict::Block(_)
        ));
    }

    #[test]
    fn file_allow_trusted_paths() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        set_sandbox_policy(policy(json!({ "file_allow": ["src/**"] })));
        assert!(file_paths_trusted("write_file", &json!({ "path": "src/a/b.rs" })));
        assert!(!file_paths_trusted("write_file", &json!({ "path": "docs/x.md" })));
        assert!(!file_paths_trusted("read_file", &json!({ "path": "src/a/b.rs" })));
    }

    #[test]
    fn network_lists_and_block_all() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        set_sandbox_policy(policy(json!({ "net_deny": ["evil.example"] })));
        assert!(matches!(
            sandbox_check("web_fetch", &json!({ "url": "https://sub.evil.example/x" })),
            SandboxVerdict::Block(_)
        ));
        assert_eq!(
            sandbox_check("web_fetch", &json!({ "url": "https://ok.example/x" })),
            SandboxVerdict::Allow
        );
        // 阻止所有外部网络：允许名单外的域名全部拒绝
        set_sandbox_policy(policy(json!({ "net_block_all": true, "net_allow": ["docs.rs"] })));
        assert!(matches!(
            sandbox_check("web_fetch", &json!({ "url": "https://crates.io/x" })),
            SandboxVerdict::Block(_)
        ));
        assert_eq!(
            sandbox_check("web_fetch", &json!({ "url": "https://docs.rs/serde" })),
            SandboxVerdict::Allow
        );
    }

    #[test]
    fn malicious_heuristics() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        set_sandbox_policy(policy(json!({ "net_malicious": true })));
        // punycode 仿冒域名
        assert!(matches!(
            sandbox_check("web_fetch", &json!({ "url": "https://xn--pple-43d.com/x" })),
            SandboxVerdict::Block(_)
        ));
        // 带凭据的 URL
        assert!(matches!(
            sandbox_check("web_fetch", &json!({ "url": "https://user:pass@real.example/x" })),
            SandboxVerdict::Block(_)
        ));
        // 非标准协议
        assert!(matches!(
            sandbox_check("web_fetch", &json!({ "url": "file:///C:/Windows/win.ini" })),
            SandboxVerdict::Block(_)
        ));
        // 关闭后放行
        set_sandbox_policy(policy(json!({ "net_malicious": false })));
        assert_eq!(
            sandbox_check("web_fetch", &json!({ "url": "file:///C:/x" })),
            SandboxVerdict::Allow
        );
        set_sandbox_policy(SandboxPolicy { on: false, ..policy(json!({})) });
        assert_eq!(
            sandbox_check("web_fetch", &json!({ "url": "https://xn--pple-43d.com/x" })),
            SandboxVerdict::Allow
        );
    }

    #[test]
    fn command_programs_sees_wrappers() {
        assert_eq!(command_programs("git status"), vec!["git"]);
        assert_eq!(command_programs("cmd /c schtasks /run /tn x"), vec!["cmd", "schtasks"]);
        assert_eq!(
            command_programs("cmd /c powershell -c schtasks"),
            vec!["cmd", "powershell", "schtasks"]
        );
        assert_eq!(command_programs("bash -c \"rm -rf /\""), vec!["bash", "rm"]);
        // 输出经 sort+dedup，断言须用字典序
        let mut pwsh = vec!["powershell", "get-process"];
        pwsh.sort();
        assert_eq!(command_programs("powershell.exe -command get-process"), pwsh);
        let mut wsl = vec!["rm", "wsl"];
        wsl.sort();
        assert_eq!(command_programs("wsl rm -rf /"), wsl);
        // quoted outer path still resolves via program_name
        assert_eq!(
            command_programs(r#""C:\Program Files\Docker\docker.exe" ps"#),
            vec!["docker"]
        );
    }

    #[test]
    fn glob_pattern_cannot_escape() {
        let ws = std::env::temp_dir().join(format!("cch_glob_{}", std::process::id()));
        let _ = fs::remove_dir_all(&ws);
        fs::create_dir_all(&ws).unwrap();
        fs::write(ws.join("a.txt"), "x").unwrap();
        assert!(glob_files(ws.to_str().unwrap(), "../../*").is_err());
        assert!(glob_files(ws.to_str().unwrap(), "a/../../b*").is_err());
        assert!(glob_files(ws.to_str().unwrap(), "/etc/*").is_err());
        let ok = glob_files(ws.to_str().unwrap(), "*.txt").unwrap();
        assert!(ok.contains("a.txt"), "{ok}");
        let _ = fs::remove_dir_all(&ws);
    }

    #[test]
    fn program_name_and_domain_helpers() {
        assert_eq!(program_name(r#""C:\Program Files\Docker\docker.exe" ps"#), "docker");
        assert_eq!(program_name(r#""C:\Program Files\Docker\docker.exe""#), "docker");
        assert_eq!(program_name("/usr/bin/WSL.EXE -l"), "wsl");
        assert_eq!(program_name("git status"), "git");
        assert_eq!(url_host("https://User@Example.COM:8443/a/b?q=1"), "example.com");
        assert_eq!(url_host("http://192.168.1.4:3000"), "192.168.1.4");
        assert!(domain_match("example.com", "api.example.com"));
        assert!(domain_match("*.example.com", "api.example.com"));
        assert!(!domain_match("example.com", "notexample.com"));
        assert!(wildcard_match("src/**", "src/a/b.rs"));
        // * 无路径边界语义：可跨分隔符（设计如此，保持可预期）
        assert!(wildcard_match("src/*", "src/a/b.rs"));
        assert!(!wildcard_match("src/*", "docs/a.md"));
    }
}
