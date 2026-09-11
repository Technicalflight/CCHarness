// Vector long-term memory: a per-workspace JSON store (data_dir/memvector.json)
// + an OpenAI-compatible embeddings client + cosine top-k retrieval. Used by
// the memory_save / memory_search agent tools and the per-turn recall
// injection; post-turn auto-reflection writes into the same store.

use crate::config::AppConfig;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Hard cap per workspace — keeps the JSON store bounded (a 1536-dim item is
/// roughly 10KB of JSON, so 200 items ≈ 2MB worst case).
const MAX_ITEMS_PER_WS: usize = 200;
const MAX_TEXT_CHARS: usize = 2000;

/// One stored memory: text + its embedding + creation timestamp (ms).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryItem {
    pub text: String,
    pub vec: Vec<f32>,
    pub ts: i64,
}

type Store = HashMap<String, Vec<MemoryItem>>; // workspace -> items

/// Single-process write lock: remember() can run concurrently from several
/// lane tasks; read-modify-write must not interleave.
static STORE_LOCK: Mutex<()> = Mutex::new(());

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn store_path(data_dir: &Path) -> PathBuf {
    data_dir.join("memvector.json")
}

fn load_store(data_dir: &Path) -> Store {
    std::fs::read_to_string(store_path(data_dir))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_store(data_dir: &Path, store: &Store) -> Result<(), String> {
    let p = store_path(data_dir);
    let tmp = p.with_extension("json.tmp");
    let body = serde_json::to_string(store).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, body).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &p).map_err(|e| e.to_string())
}

/// Build the embeddings endpoint URL: trim the trailing slash and append
/// "/embeddings" when the user pasted a base URL instead of the endpoint.
fn endpoint_url(raw: &str) -> String {
    let u = raw.trim().trim_end_matches('/');
    if u.ends_with("embeddings") {
        u.to_string()
    } else {
        format!("{u}/embeddings")
    }
}

/// Call an OpenAI-compatible embeddings endpoint for one text.
pub async fn embed(
    client: &reqwest::Client,
    url: &str,
    key: &str,
    model: &str,
    text: &str,
) -> Result<Vec<f32>, String> {
    if url.trim().is_empty() || model.trim().is_empty() {
        return Err("未配置 embeddings 接口：请在设置中填写 Embeddings URL 与模型".into());
    }
    let ep = endpoint_url(url);
    let mut req = client
        .post(&ep)
        .json(&json!({ "model": model.trim(), "input": [text] }))
        .timeout(std::time::Duration::from_secs(30));
    if !key.trim().is_empty() {
        req = req.bearer_auth(key.trim());
    }
    let resp = req.send().await.map_err(|e| format!("embeddings 请求失败: {e}"))?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.map_err(|e| format!("embeddings 响应解析失败: {e}"))?;
    if !status.is_success() {
        let msg = body
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or("未知错误");
        return Err(format!("embeddings 接口错误（HTTP {status}）: {msg}"));
    }
    let v = body
        .pointer("/data/0/embedding")
        .and_then(|e| e.as_array())
        .ok_or_else(|| "embeddings 响应缺少 data[0].embedding".to_string())?;
    let vec: Vec<f32> = v
        .iter()
        .filter_map(|x| x.as_f64().map(|f| f as f32))
        .collect();
    if vec.is_empty() {
        return Err("embeddings 返回了空向量".into());
    }
    Ok(vec)
}

/// Cosine similarity; zero-magnitude inputs yield 0 (never NaN).
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Insert one memory item for a workspace (pure — the caller supplies the
/// embedding). Caps the store by dropping the oldest items.
pub fn insert(data_dir: &Path, workspace: &str, text: &str, vec: Vec<f32>) -> Result<(), String> {
    let text: String = text.trim().chars().take(MAX_TEXT_CHARS).collect();
    if text.is_empty() {
        return Err("记忆内容不能为空".into());
    }
    let _g = STORE_LOCK.lock().unwrap();
    let mut store = load_store(data_dir);
    let items = store.entry(workspace.to_string()).or_default();
    items.push(MemoryItem { text, vec, ts: now_ms() });
    if items.len() > MAX_ITEMS_PER_WS {
        items.drain(..items.len() - MAX_ITEMS_PER_WS);
    }
    save_store(data_dir, &store)
}

/// Cosine top-k retrieval over a workspace's stored items (pure — the caller
/// supplies the query embedding). Returns (score, text) pairs, best first.
pub fn search(data_dir: &Path, workspace: &str, query_vec: &[f32], k: usize) -> Vec<(f32, String)> {
    let store = load_store(data_dir);
    let mut scored: Vec<(f32, String)> = store
        .get(workspace)
        .map(|items| {
            items
                .iter()
                .map(|m| (cosine(query_vec, &m.vec), m.text.clone()))
                .collect()
        })
        .unwrap_or_default();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(k);
    scored
}

/// Save a memory: embed the text, then persist it for the workspace.
pub async fn remember(
    client: &reqwest::Client,
    cfg: &AppConfig,
    data_dir: &Path,
    workspace: &str,
    text: &str,
) -> Result<(), String> {
    let s = &cfg.settings;
    let vec = embed(client, &s.embeddings_url, &s.embeddings_key, &s.embeddings_model, text).await?;
    insert(data_dir, workspace, text, vec)
}

/// Recall top-k memories for a workspace given a query text.
pub async fn recall(
    client: &reqwest::Client,
    cfg: &AppConfig,
    data_dir: &Path,
    workspace: &str,
    query: &str,
    k: usize,
) -> Vec<String> {
    let s = &cfg.settings;
    let Ok(qv) = embed(client, &s.embeddings_url, &s.embeddings_key, &s.embeddings_model, query).await
    else {
        return Vec::new(); // silent: memory is a garnish, never a blocker
    };
    search(data_dir, workspace, &qv, k)
        .into_iter()
        .map(|(_, text)| text)
        .collect()
}

/// Post-turn automatic reflection: ask the model to distill 0-3 durable
/// facts from the finished exchange and store each via the normal remember
/// path. OpenAI-compatible chat endpoint only (Anthropic-style providers are
/// skipped). Every failure is silent — reflection is a garnish, never a
/// blocker for the turn that already finished.
pub async fn reflect_and_remember(
    client: &reqwest::Client,
    cfg: &AppConfig,
    provider: &crate::config::Provider,
    model: &str,
    user_text: &str,
    reply_text: &str,
    data_dir: &Path,
    workspace: &str,
) {
    let trim = |s: &str| -> String { s.trim().chars().take(4000).collect() };
    let sys = "你是记忆提炼器。阅读一轮用户与助手的对话，提取值得跨会话长期记住的信息（项目事实、用户偏好、重要结论、踩过的坑）。\
规则：每条一行、独立可读、精炼（不超过120字）；最多 3 条；没有值得记住的就什么都不输出。不要输出编号、前缀符号、解释或任何其他文字。\
隐私红线（必须遵守）：记忆库是长期持久的，禁止把密钥、令牌、密码、Cookie、私钥、内网/本机绝对路径、个人身份信息原文写入任何一条记忆；\
确需引用时一律用占位符替代（如 <API_KEY>、<路径>），只记「是什么、为什么、怎么用」，不记原始值。";
    let user = format!(
        "【用户消息】\n{}\n\n【助手回复】\n{}",
        trim(user_text),
        trim(reply_text)
    );
    let text = match crate::chat::ask_once(client, provider, model, sys, &user, 400).await {
        Ok(t) => t.trim().to_string(),
        Err(_) => return, // silent: reflection must never disturb the turn
    };
    if text.is_empty() {
        return; // the model decided nothing is worth remembering
    }
    for line in text.lines() {
        let line = line
            .trim()
            .trim_start_matches(['-', '*', '•', '·'])
            .trim_start_matches(|c: char| c.is_ascii_digit() || c == '.' || c == ')' || c == '、')
            .trim();
        if line.chars().count() < 4 {
            continue; // too short to be a real fact
        }
        if remember(client, cfg, data_dir, workspace, line).await.is_err() {
            break; // embeddings broken — stop trying
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "ccharness-memvector-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn cosine_basics() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert!((cosine(&[1.0, 0.0], &[-1.0, 0.0]) + 1.0).abs() < 1e-6);
        assert_eq!(cosine(&[], &[1.0]), 0.0);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
    }

    #[test]
    fn endpoint_url_variants() {
        assert_eq!(endpoint_url("https://x/v1/embeddings"), "https://x/v1/embeddings");
        assert_eq!(endpoint_url("https://x/v1/"), "https://x/v1/embeddings");
        assert_eq!(endpoint_url(" https://x/v1 "), "https://x/v1/embeddings");
    }

    #[test]
    fn insert_search_persist_roundtrip() {
        let d = tmpdir("rt");
        // two items, one clearly closer to the query
        insert(&d, "C:/ws", "项目使用 Tauri 2 框架", vec![1.0, 0.0, 0.0]).unwrap();
        insert(&d, "C:/ws", "用户偏好深色主题", vec![0.0, 1.0, 0.0]).unwrap();
        let hits = search(&d, "C:/ws", &[1.0, 0.1, 0.0], 2);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].1, "项目使用 Tauri 2 框架");
        assert!(hits[0].0 > 0.99);
        // empty store for an unknown workspace
        assert!(search(&d, "C:/elsewhere", &[1.0, 0.0, 0.0], 3).is_empty());
        // persisted across a fresh load (tmp file survives the call)
        let hits2 = search(&d, "C:/ws", &[0.0, 1.0, 0.0], 1);
        assert_eq!(hits2[0].1, "用户偏好深色主题");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn insert_caps_and_rejects_empty() {
        let d = tmpdir("cap");
        for i in 0..(MAX_ITEMS_PER_WS + 10) {
            insert(&d, "ws", &format!("记忆 {i}"), vec![i as f32]).unwrap();
        }
        let store: Store = load_store(&d);
        assert_eq!(store["ws"].len(), MAX_ITEMS_PER_WS);
        // oldest 10 dropped (210 inserted - 200 cap) → 记忆 0..9 are gone
        assert!(!store["ws"].iter().any(|m| m.text == "记忆 9"));
        assert!(store["ws"].iter().any(|m| m.text == "记忆 10"));
        assert!(insert(&d, "ws", "   ", vec![1.0]).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }
}
