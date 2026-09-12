// importer.rs — bring local agent-CLI session transcripts (JSONL) into
// CCHarness as ordinary chat sessions. Source files are only ever read.
//
// Two adapters:
//   claude-code : lines like {"type":"user","message":{"role","content"},
//                 "timestamp"} under ~/.claude/projects/**/*.jsonl
//   generic     : best-effort {"message":{role,content}} or {role,content}
//                 per line — covers other CLIs and hand-picked files
//
// Tool-call blocks are intentionally flattened out: imports are about the
// conversation, not a byte-exact replay surface.
use serde::Serialize;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

/// Per-message content cap (chars) — transcripts can carry huge tool blobs.
const CONTENT_CAP: usize = 200_000;
/// Scan result cap per source.
const SCAN_CAP: usize = 200;
/// Source files larger than this are skipped (corrupt dumps guard).
const FILE_SIZE_CAP: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct ImportCandidate {
    pub source: String,
    pub path: String,
    /// First user-message head — the suggested title.
    pub title: String,
    pub messages: usize,
    pub size_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct ImportedMessage {
    pub role: String,
    pub content: String,
}

fn home_dir() -> Option<PathBuf> {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
        .map(PathBuf::from)
}

/// Default transcript locations per source id.
pub(crate) fn source_dir(source: &str) -> Option<PathBuf> {
    let home = home_dir()?;
    match source {
        "claude-code" => Some(home.join(".claude").join("projects")),
        "codex" => Some(home.join(".codex").join("sessions")),
        "opencode" => Some(home.join(".local").join("share").join("opencode")),
        _ => None,
    }
}

/// Scan a source's default location for importable .jsonl transcripts.
pub fn scan(source: &str) -> Vec<ImportCandidate> {
    let Some(dir) = source_dir(source) else { return Vec::new() };
    let mut files: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    collect_jsonl(&dir, &mut files, 0);
    files.sort_by(|a, b| b.1.cmp(&a.1));
    files
        .into_iter()
        .take(SCAN_CAP)
        .filter_map(|(path, _)| candidate(source, &path))
        .collect()
}

fn collect_jsonl(dir: &Path, out: &mut Vec<(PathBuf, std::time::SystemTime)>, depth: usize) {
    if depth > 6 || out.len() >= SCAN_CAP * 2 {
        return;
    }
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_jsonl(&p, out, depth + 1);
        } else if p.extension().and_then(|x| x.to_str()) == Some("jsonl") {
            let mtime = e.metadata().ok().and_then(|m| m.modified().ok()).unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            out.push((p, mtime));
        }
    }
}

fn candidate(source: &str, path: &Path) -> Option<ImportCandidate> {
    let meta = fs::metadata(path).ok()?;
    if meta.len() > FILE_SIZE_CAP {
        return None;
    }
    let msgs = parse_file(source, &path.to_str()?).ok()?;
    if msgs.is_empty() {
        return None;
    }
    let title = msgs
        .iter()
        .find(|m| m.role == "user")
        .map(|m| m.content.chars().take(40).collect::<String>())
        .unwrap_or_else(|| {
            path.file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default()
        });
    Some(ImportCandidate {
        source: source.to_string(),
        path: path.to_string_lossy().to_string(),
        title,
        messages: msgs.len(),
        size_bytes: meta.len(),
    })
}

/// Parse one transcript file into ordered user/assistant messages.
pub fn parse_file(source: &str, path: &str) -> Result<Vec<ImportedMessage>, String> {
    let raw = fs::read_to_string(path).map_err(|e| format!("读取失败: {e}"))?;
    let mut out: Vec<ImportedMessage> = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue; // tolerate stray separators / partial lines
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let parsed = match source {
            "claude-code" => claude_line(&v),
            _ => generic_line(&v),
        };
        if let Some((role, content)) = parsed {
            let content: String = content.chars().take(CONTENT_CAP).collect();
            if content.trim().is_empty() {
                continue;
            }
            out.push(ImportedMessage { role, content });
        }
    }
    Ok(out)
}

/// Flatten JSON content into plain text: string passes through; block arrays
/// contribute their "text" fields (tool_use / tool_result / image are dropped).
fn content_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| {
                let kind = b.get("type").and_then(|t| t.as_str()).unwrap_or("");
                if kind == "text" || kind.is_empty() {
                    b.get("text").and_then(|t| t.as_str()).map(|s| s.to_string())
                } else if kind == "thinking" || kind == "reasoning" {
                    None // reasoning is not part of the visible transcript
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn claude_line(v: &Value) -> Option<(String, String)> {
    let t = v.get("type").and_then(|x| x.as_str())?;
    if t != "user" && t != "assistant" {
        return None; // summary / system / file-history-snapshot lines
    }
    let msg = v.get("message")?;
    let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or(t);
    if role != "user" && role != "assistant" {
        return None;
    }
    Some((role.to_string(), content_text(msg.get("content")?)))
}

fn generic_line(v: &Value) -> Option<(String, String)> {
    // shape A: {"message": {"role": ..., "content": ...}}
    // shape B: {"role": ..., "content": ...}
    let (role, content) = match v.get("message") {
        Some(m) => (m.get("role")?, m.get("content")?),
        None => (v.get("role")?, v.get("content")?),
    };
    let role = role.as_str()?;
    let role = match role {
        "human" => "user",
        "ai" | "model" => "assistant",
        "user" | "assistant" => role,
        _ => return None, // system / tool / unknown roles are not imported
    };
    Some((role.to_string(), content_text(content)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_lines_parse() {
        let v: Value = serde_json::from_str(
            r#"{"type":"user","message":{"role":"user","content":"你好，帮我看看这个项目"},"timestamp":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(claude_line(&v), Some(("user".into(), "你好，帮我看看这个项目".into())));

        let v: Value = serde_json::from_str(
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"第一段"},{"type":"tool_use","id":"t1","name":"read_file"},{"type":"text","text":"第二段"}]}}"#,
        )
        .unwrap();
        let (_, text) = claude_line(&v).unwrap();
        assert_eq!(text, "第一段\n第二段");

        // non-chat lines are skipped
        let v: Value = serde_json::from_str(r#"{"type":"summary","summary":"标题"}"#).unwrap();
        assert_eq!(claude_line(&v), None);
    }

    #[test]
    fn generic_lines_parse_both_shapes() {
        let v: Value = serde_json::from_str(r#"{"message":{"role":"user","content":"hi"}}"#).unwrap();
        assert_eq!(generic_line(&v), Some(("user".into(), "hi".into())));
        let v: Value = serde_json::from_str(r#"{"role":"assistant","content":[{"type":"text","text":"hello"}]}"#).unwrap();
        assert_eq!(generic_line(&v), Some(("assistant".into(), "hello".into())));
        let v: Value = serde_json::from_str(r#"{"role":"system","content":"sys"}"#).unwrap();
        assert_eq!(generic_line(&v), None);
        let v: Value = serde_json::from_str(r#"{"role":"human","content":"x"}"#).unwrap();
        assert_eq!(generic_line(&v).map(|(r, _)| r), Some("user".into()));
    }

    #[test]
    fn parse_file_orders_and_skips_junk() {
        let dir = std::env::temp_dir().join(format!("ccharness-import-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"type\":\"summary\",\"summary\":\"x\"}\n",
                "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"问\"}}\n",
                "not json\n",
                "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":\"答\"}}\n",
            ),
        )
        .unwrap();
        let msgs = parse_file("claude-code", path.to_str().unwrap()).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[1].content, "答");
        let _ = fs::remove_dir_all(&dir);
    }
}
