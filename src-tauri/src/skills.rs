// Skills: prompt-layer plugins. A skill is a markdown file with optional
// frontmatter (name / description / inject), loaded from two roots:
//
//   global:  ~/.ccharness/skills/*.md
//   project: <workspace>/.ccharness/skills/*.md   (overrides global by name)
//
// inject=auto   → full body folded into Zone S (byte-stable per session;
//                 edits trigger the usual epoch rebuild via system_is)
// inject=command→ callable from the composer as /name; listed by name in
//                 Zone S so the model knows it exists

use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

pub const SKILL_BODY_CAP: usize = 8 * 1024;
const SKILLS_TOTAL_CAP: usize = 32 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    /// true = injected into Zone S; false = callable via /name
    pub auto_inject: bool,
    pub source: String, // "global" | "project"
    pub body: String,
}

fn parse_frontmatter(raw: &str, fallback_name: &str) -> SkillInfo {
    let mut name = fallback_name.to_string();
    let mut description = String::new();
    let mut auto_inject = false;
    let mut body = raw.trim().to_string();

    if let Some(rest) = raw.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            let header = &rest[..end];
            body = rest[end + 4..].trim().to_string();
            for line in header.lines() {
                let Some((k, v)) = line.split_once(':') else { continue };
                let v = v.trim();
                match k.trim() {
                    "name" if !v.is_empty() => name = v.to_string(),
                    "description" => description = v.to_string(),
                    "inject" if v.eq_ignore_ascii_case("auto") => auto_inject = true,
                    _ => {}
                }
            }
        }
    }

    if body.chars().count() > SKILL_BODY_CAP {
        let mut cut = SKILL_BODY_CAP;
        while cut > 0 && !body.is_char_boundary(cut) {
            cut -= 1;
        }
        body.truncate(cut);
        body.push_str("\n…[技能正文超过 8KB，已截断]");
    }

    SkillInfo { name, description, auto_inject, source: String::new(), body }
}

fn scan_dir(dir: &Path, source: &str, out: &mut Vec<SkillInfo>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("md") {
            continue;
        }
        let stem = p
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        if let Ok(raw) = fs::read_to_string(&p) {
            let mut info = parse_frontmatter(&raw, &stem);
            info.source = source.to_string();
            // project skill with the same name replaces the global one
            if let Some(existing) = out.iter_mut().find(|s| s.name == info.name) {
                if source == "project" {
                    *existing = info;
                }
                continue;
            }
            out.push(info);
        }
    }
}

/// Scan both roots; project skills shadow global ones by name.
pub fn scan(workspace: Option<&str>) -> Vec<SkillInfo> {
    let mut out: Vec<SkillInfo> = Vec::new();
    if let Some(home) = dirs_home() {
        scan_dir(&home.join(".ccharness").join("skills"), "global", &mut out);
    }
    if let Some(ws) = workspace {
        if !ws.trim().is_empty() {
            scan_dir(&PathBuf::from(ws).join(".ccharness").join("skills"), "project", &mut out);
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

/// Body of one skill by name (both roots, project shadows global) — the
/// model-facing loader behind the load_skill tool. The body rides back as
/// the tool_result; error messages list what IS installed so the model can
/// self-correct on a near-miss name.
pub fn load_body(workspace: Option<&str>, name: &str) -> Result<String, String> {
    let needle = name.trim().trim_start_matches('/');
    if needle.is_empty() {
        return Err("技能名为空 —— 请填「可用技能」名录中的名字".into());
    }
    let all = scan(workspace);
    match all.iter().find(|s| s.name == needle) {
        Some(s) => Ok(format!("[调用技能 /{}]\n\n{}", s.name, s.body)),
        None => {
            let known: Vec<&str> = all.iter().map(|s| s.name.as_str()).collect();
            if known.is_empty() {
                Err("当前没有已安装的技能".into())
            } else {
                Err(format!(
                    "技能「{needle}」不存在。可用的技能：{}",
                    known.join("、")
                ))
            }
        }
    }
}

/// Zone S block: auto-inject bodies + a name catalog for command skills.
/// Bounded so a fat skills folder can never crowd out the conversation.
pub fn zone_section(workspace: Option<&str>) -> String {
    let skills = scan(workspace);
    if skills.is_empty() {
        return String::new();
    }
    let mut parts: Vec<String> = Vec::new();
    let mut budget = SKILLS_TOTAL_CAP;
    for s in skills.iter().filter(|s| s.auto_inject) {
        let block = format!("### 技能: {}\n{}", s.name, s.body);
        if block.len() > budget {
            break;
        }
        budget -= block.len();
        parts.push(block);
    }
    let commands: Vec<String> = skills
        .iter()
        .filter(|s| !s.auto_inject)
        .map(|s| {
            if s.description.is_empty() {
                format!("- /{}", s.name)
            } else {
                format!("- /{} — {}", s.name, s.description)
            }
        })
        .collect();
    let mut section = String::from(
        "## 可用技能\n技能路由阶梯：任务命中任一技能描述（含 Use when/触发场景）→ 先用已装技能；\
         已装技能不满足 → 看是否有可配置项可调；都没有 → 与其凭空自创流程，不如向用户说明并建议新建技能。\
         优先复用，避免重复造轮子。\n",
    );
    if !parts.is_empty() {
        section.push_str(&parts.join("\n\n"));
        section.push('\n');
    }
    if !commands.is_empty() {
        section.push_str(&format!(
            "以下技能的正文不会自动展示；当任务与其描述匹配时，用 load_skill 工具按名字加载全文后遵循执行（用户也可以 /名 直接触发）：\n{}\n",
            commands.join("\n")
        ));
    }
    section
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter() {
        let raw = "---\nname: review\ndescription: 代码审查\ninject: auto\n---\n正文内容";
        let s = parse_frontmatter(raw, "review");
        assert_eq!(s.name, "review");
        assert_eq!(s.description, "代码审查");
        assert!(s.auto_inject);
        assert_eq!(s.body, "正文内容");
    }

    #[test]
    fn no_frontmatter_uses_stem() {
        let s = parse_frontmatter("# 直接正文", "my-skill");
        assert_eq!(s.name, "my-skill");
        assert!(!s.auto_inject);
        assert_eq!(s.body, "# 直接正文");
    }

    #[test]
    fn body_cap_is_char_safe() {
        let raw = format!("---\nname: big\n---\n{}", "汉".repeat(SKILL_BODY_CAP + 1));
        let s = parse_frontmatter(&raw, "big");
        assert!(s.body.chars().count() <= SKILL_BODY_CAP + 16);
        assert!(s.body.contains("已截断"));
    }
}
