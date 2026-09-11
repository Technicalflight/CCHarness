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
    let rel_trim = rel.trim().trim_matches(['/', '\\']);
    if rel_trim.is_empty() {
        return Ok(PathBuf::from(workspace));
    }
    let candidate = Path::new(rel_trim);
    if candidate.is_absolute() {
        // allow a path that is literally inside the workspace
        let ws_norm = normalize_plain(Path::new(workspace));
        let c_norm = normalize_plain(candidate);
        if c_norm.starts_with(&ws_norm) {
            return Ok(c_norm);
        }
        return Err(format!("拒绝绝对路径 {rel_trim}：请使用相对工作区的路径"));
    }
    if rel_trim.split(['/', '\\']).any(|seg| seg == "..") {
        return Err("拒绝包含 .. 的路径".into());
    }
    let joined = normalize_plain(&Path::new(workspace).join(candidate));
    let base = normalize_plain(Path::new(workspace));
    if !joined.starts_with(&base) {
        return Err(format!("路径 {rel_trim} 越出工作区边界"));
    }
    Ok(joined)
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
        ),
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
    let p = path.to_string_lossy().replace('\'', "");
    #[cfg(windows)]
    {
        let script = format!(
            "Add-Type -AssemblyName System.Windows.Forms,System.Drawing; \
             $b=[System.Windows.Forms.SystemInformation]::VirtualScreen; \
             $bmp=New-Object System.Drawing.Bitmap $b.Width,$b.Height; \
             $g=[System.Drawing.Graphics]::FromImage($bmp); \
             $g.CopyFromScreen($b.X,$b.Y,0,0,$bmp.Size); \
             $g.Dispose(); $bmp.Save('{p}'); $bmp.Dispose()",
            p = p
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
    let mut child = c.spawn().map_err(|e| format!("启动失败: {e}"))?;

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let t_out = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut std::io::BufReader::new(stdout), &mut buf);
        buf
    });
    let t_err = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut std::io::BufReader::new(stderr), &mut buf);
        buf
    });

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout);
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                return Err(format!("等待进程失败: {e}"));
            }
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };

    let decode = |bytes: Vec<u8>| -> String {
        let s = String::from_utf8_lossy(&bytes);
        s.chars().take(MAX_TOOL_RESULT_CHARS * 2).collect()
    };
    let out = decode(t_out.join().unwrap_or_default());
    let err = decode(t_err.join().unwrap_or_default());

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
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(FETCH_TIMEOUT_SECS))
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() > 5 {
                    return attempt.error("重定向过多");
                }
                let host = attempt.url().host_str().unwrap_or("");
                if crate::urlguard::is_loopback_or_private(host) {
                    return attempt.error("重定向目标位于本机/内网，已拒绝");
                }
                attempt.follow()
            }))
            .user_agent("CCHarness/0.1 (agent web_fetch)")
            .build()
            .map_err(|e| format!("HTTP 客户端构建失败: {e}"))?;
        let resp = client
            .get(&url_owned)
            .timeout(std::time::Duration::from_secs(FETCH_TIMEOUT_SECS))
            .send()
            .map_err(|e| format!("请求失败: {e}"))?;
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
        Ok((format!("{status} {ctype}"), String::from_utf8_lossy(&bytes).to_string()))
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
    let base = workspace.trim_end_matches(['/', '\\']);
    let full = format!("{base}/{pattern}");
    let mut out: Vec<String> = Vec::new();
    for entry in glob::glob(&full).map_err(|e| format!("非法 glob 模式: {e}"))?.flatten() {
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
    let Ok(entries) = fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if p.is_dir() {
            if !SKIP_DIRS.contains(&name.as_str()) {
                walk_text_files(root, &p, visit);
            }
        } else {
            let rel = p
                .strip_prefix(root)
                .unwrap_or(&p)
                .to_string_lossy()
                .replace('\\', "/");
            visit(&rel, &p);
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
        assert!(resolve_in_workspace("C:/tmp/ws", "../outside.txt").is_err());
        assert!(resolve_in_workspace("C:/tmp/ws", "a/../../b").is_err());
        assert!(resolve_in_workspace("C:/tmp/ws", "C:/Windows/win.ini").is_err());
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
