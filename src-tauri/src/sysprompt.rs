// Zone S assembly — Codex-style layered system prompt.
//
// Layers (in byte order, all frozen at session start):
//   1. identity      constant agent constitution (build-stable bytes)
//   2. user global   settings.system_prompt, if the user wrote one
//   3. AGENTS.md     first match: <workspace>/AGENTS.md → ~/.ccharness/AGENTS.md
//   4. environment   workspace root / OS / date; date changes daily, not
//                    per-turn, so it stays cache-friendly
//
// Everything above rides inside the frozen Zone S: same session ⇒ identical
// bytes ⇒ prefix-cacheable. A workspace or settings change takes effect for
// new sessions, mirroring the cache discipline documented in Settings.

use std::fs;
use std::path::Path;

pub const AGENTS_MD_CAP: usize = 16 * 1024;

fn identity() -> String {
    // constant per build — never interpolate time/locale/env here
    String::from(
        "你是 CCHarness 内置的编程助手。\n\
         ## 工作方式\n\
         - 你可以使用只读工具（list_dir / read_file / glob_files / grep_files / web_fetch）检查用户的工作区与公开网页；先查证，再回答。\n\
         - 所有工具路径都相对当前工作区根目录；越界路径会被拒绝；web_fetch 仅允许公网地址。\n\
         - write_file / edit_file / apply_patch / delete_file / move_path / run_command 属敏感操作，每次执行都会先请求用户批准；被拒绝时不要原样重试。\n\
         - 处理多步骤长任务时，用 todo_write 把计划同步到任务面板（3–7 项），每完成或开始一项就更新清单状态。\n\
         - 回答使用用户的语言；代码、标识符与命令保持英文。\n\
         - 不确定时明确说明，不要编造文件内容。\n\
         ## 质量纪律\n\
         - 每次修改后，确定并运行能覆盖该改动的最小检查（编译 / 测试 / lint），并在最后一次修改之后重跑；没有验证结果就不要宣称完成。\n\
         - 修复失败时遵循链条：复现 → 定位原因 → 最小修复 → 重跑同一检查；没有定位到原因的盲目重试是被禁止的。\n\
         - 修改范围不得超出任务边界；发现必须改动范围外的文件时，先说明原因征得同意，不要直接改。\n\
         - 引用文件内容前必须先用工具读取该文件；只引用读过的内容。\n\
         - 陈述结果时给出可核验证据（文件路径 / 命令输出 / 测试名）；计划了不等于完成了。\n",
    )
}

fn environment(workspace: Option<&str>) -> String {
    let ws = workspace.unwrap_or("（未绑定工作区，工具不可用）");
    let os = std::env::consts::OS;
    let today = chrono::Local::now().format("%Y-%m-%d");
    format!(
        "## 运行环境\n- 工作区根目录: {ws}\n- 操作系统: {os}\n- 今天日期: {today}\n"
    )
}

/// Locate AGENTS.md: workspace-level wins over the global one.
fn load_agents_md(workspace: Option<&str>) -> Option<String> {
    let candidates: Vec<std::path::PathBuf> = match workspace {
        Some(ws) => vec![
            Path::new(ws).join("AGENTS.md"),
            Path::new(ws).join(".ccharness").join("AGENTS.md"),
        ],
        None => vec![],
    };
    for c in candidates {
        if let Ok(body) = fs::read_to_string(&c) {
            let body = body.trim();
            if !body.is_empty() {
                return Some(cap_utf8(body));
            }
        }
    }
    // global fallback: ~/.ccharness/AGENTS.md
    if let Some(home) = dirs_home() {
        let p = home.join(".ccharness").join("AGENTS.md");
        if let Ok(body) = fs::read_to_string(&p) {
            let body = body.trim();
            if !body.is_empty() {
                return Some(cap_utf8(body));
            }
        }
    }
    None
}

fn dirs_home() -> Option<std::path::PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(std::path::PathBuf::from)
}

fn cap_utf8(s: &str) -> String {
    if s.len() <= AGENTS_MD_CAP {
        return s.to_string();
    }
    let mut cut = AGENTS_MD_CAP;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n…[AGENTS.md 超过 16KB，已截断]", &s[..cut])
}

/// Assemble the full Zone S text for a new session.
pub fn assemble(user_global: &str, workspace: Option<&str>) -> String {
    let mut parts: Vec<String> = vec![identity()];
    let user_global = user_global.trim();
    if !user_global.is_empty() {
        parts.push(format!("## 用户全局指令\n{user_global}"));
    }
    if let Some(agents) = load_agents_md(workspace) {
        parts.push(format!("## 项目说明（AGENTS.md）\n{agents}"));
    }
    let skills = crate::skills::zone_section(workspace);
    if !skills.is_empty() {
        parts.push(skills);
    }
    parts.push(environment(workspace));
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_build_constant() {
        assert_eq!(identity(), identity());
    }

    #[test]
    fn assemble_contains_layers() {
        let ws = std::env::temp_dir().join("ccharness-no-such-ws-xyz");
        let s = assemble("要简洁", Some(ws.to_str().unwrap()));
        assert!(s.contains("CCHarness"));
        assert!(s.contains("用户全局指令"));
        assert!(s.contains("要简洁"));
        assert!(s.contains("运行环境"));
        assert!(s.contains("ccharness-no-such-ws-xyz"));
    }

    #[test]
    fn no_user_layer_when_empty() {
        let s = assemble("   ", None);
        assert!(!s.contains("用户全局指令"));
    }

    #[test]
    fn cap_is_char_safe() {
        let cjk = "中".repeat(AGENTS_MD_CAP); // 3 bytes each
        let capped = cap_utf8(&cjk);
        assert!(capped.len() <= AGENTS_MD_CAP + 64);
        assert!(capped.contains("已截断"));
    }
}
