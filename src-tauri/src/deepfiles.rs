//! Files API image reuse (opt-in per provider): upload an image once, then
//! reference it by `file_id` on every later request instead of re-sending
//! the base64 payload. The index is content-addressed (SHA-256 of the base64
//! payload) and persisted under `<data_dir>/files_index.json`, so the same
//! attachment resolves to the same file id across turns AND restarts — the
//! wire bytes stay deterministic, which keeps the prefix-cache discipline.
//!
//! Wire shape (OpenAI-compatible `/files`, verified against DeepSeek's
//! Files API): multipart upload with `purpose=user_data`; the chat message
//! part becomes `{"type":"file","file_id":…}`. Every failure path falls back
//! to the classic inline data URI — an upload problem degrades to today's
//! behavior, never to a broken request.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// One resolved upload: provider-returned id + when it was created (ms).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub file_id: String,
    pub created_at: u64,
}

/// Content-addressed index: provider id → payload hash → entry.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct FileIndex {
    map: HashMap<String, HashMap<String, FileEntry>>,
}

impl FileIndex {
    fn path_for(data_dir: &Path) -> PathBuf {
        data_dir.join("files_index.json")
    }

    pub fn load(data_dir: &Path) -> Self {
        match std::fs::read(Self::path_for(data_dir)) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, data_dir: &Path) {
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(Self::path_for(data_dir), json);
        }
    }

    pub fn lookup(&self, provider_id: &str, hash: &str) -> Option<&FileEntry> {
        self.map.get(provider_id)?.get(hash)
    }

    pub fn insert(&mut self, provider_id: &str, hash: &str, entry: FileEntry) {
        self.map
            .entry(provider_id.to_string())
            .or_default()
            .insert(hash.to_string(), entry);
    }

    /// 过期驱逐 + 单 provider 容量上限（新者保留）。返回是否有变化，
    /// 调用方据此决定是否回盘。此前索引只增不减，长期使用会无限膨胀。
    pub fn evict(&mut self, now_ms: u64) -> bool {
        let mut changed = false;
        for entries in self.map.values_mut() {
            let before = entries.len();
            entries.retain(|_, e| now_ms.saturating_sub(e.created_at) <= ENTRY_MAX_AGE_MS);
            if entries.len() != before {
                changed = true;
            }
            if entries.len() > MAX_PER_PROVIDER {
                let mut by_age: Vec<(u64, String)> = entries
                    .iter()
                    .map(|(h, e)| (e.created_at, h.clone()))
                    .collect();
                by_age.sort_by_key(|(ts, _)| *ts); // oldest first
                for (_, h) in by_age.into_iter().take(entries.len() - MAX_PER_PROVIDER) {
                    entries.remove(&h);
                    changed = true;
                }
            }
        }
        self.map.retain(|_, e| !e.is_empty());
        changed
    }
}

/// 驱逐策略：条目过期（provider 侧文件随时可能被回收，过期 id 只会让
/// wire 请求退化失败）或单 provider 条目数超限时，丢弃最旧的。
const ENTRY_MAX_AGE_MS: u64 = 30 * 24 * 3600 * 1000;
const MAX_PER_PROVIDER: usize = 512;

/// Content hash of one image payload (16 hex chars — file-name grade).
pub fn image_hash(b64: &str) -> String {
    hex::encode(&Sha256::digest(b64.as_bytes())[..8])
}

/// Resolve every image on the message batch to a `file_ref` where possible.
/// Images already carrying a reference are left untouched; upload failures
/// leave the image inline (data URI) — fail-safe by construction.
pub async fn ensure_file_refs(
    client: &reqwest::Client,
    data_dir: &Path,
    provider_id: &str,
    base_url: &str,
    api_key: &str,
    messages: &mut [crate::prefix::ChatMessage],
) {
    let mut index = FileIndex::load(data_dir);
    let mut dirty = index.evict(crate::sessions::now_ms());
    for msg in messages.iter_mut() {
        for img in msg.images.iter_mut() {
            if img.file_ref.is_some() {
                continue;
            }
            let hash = image_hash(&img.b64);
            if let Some(entry) = index.lookup(provider_id, &hash) {
                img.file_ref = Some(entry.file_id.clone());
                continue;
            }
            match upload_image(client, base_url, api_key, &img.mime, &img.b64).await {
                Ok(file_id) => {
                    index.insert(
                        provider_id,
                        &hash,
                        FileEntry {
                            file_id: file_id.clone(),
                            created_at: crate::sessions::now_ms(),
                        },
                    );
                    img.file_ref = Some(file_id);
                    dirty = true;
                }
                Err(e) => {
                    eprintln!("[files] 图片上传失败，回退内联发送: {e}");
                }
            }
        }
    }
    if dirty {
        index.save(data_dir);
    }
}

/// Multipart upload of one image to `{base}/files` (purpose=user_data).
/// Returns the provider file id.
async fn upload_image(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    mime: &str,
    b64: &str,
) -> Result<String, String> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| format!("base64 解码失败: {e}"))?;
    let ext = match mime {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "bin",
    };
    let part = reqwest::multipart::Part::bytes(bytes)
        .file_name(format!("image.{ext}"))
        .mime_str(mime)
        .map_err(|e| format!("mime 非法: {e}"))?;
    let form = reqwest::multipart::Form::new()
        .text("purpose", "user_data")
        .part("file", part);
    let url = format!("{}/files", base_url.trim_end_matches('/'));
    let resp = client
        .post(&url)
        .bearer_auth(api_key)
        .multipart(form)
        .timeout(std::time::Duration::from_secs(60))
        .send()
        .await
        .map_err(|e| format!("上传请求失败: {e}"))?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.map_err(|e| format!("响应解析失败: {e}"))?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {body}"));
    }
    body.get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("响应缺少 id: {body}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ccharness-files-test-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn index_roundtrip_and_isolation() {
        let dir = temp_dir("roundtrip");
        let mut idx = FileIndex::load(&dir);
        idx.insert("p1", "aaa", FileEntry { file_id: "file-1".into(), created_at: 1 });
        idx.save(&dir);
        let reloaded = FileIndex::load(&dir);
        assert_eq!(reloaded.lookup("p1", "aaa").unwrap().file_id, "file-1");
        assert!(reloaded.lookup("p2", "aaa").is_none());
        assert!(reloaded.lookup("p1", "bbb").is_none());
    }

    #[test]
    fn hash_is_deterministic_and_content_addressed() {
        assert_eq!(image_hash("abc"), image_hash("abc"));
        assert_ne!(image_hash("abc"), image_hash("abd"));
        assert_eq!(image_hash("abc").len(), 16);
    }

    #[test]
    fn evict_drops_stale_and_caps_per_provider() {
        let mut idx = FileIndex::default();
        let now = 1_000_000_000_000u64;
        idx.insert(
            "p1",
            "old",
            FileEntry { file_id: "a".into(), created_at: now - ENTRY_MAX_AGE_MS - 1 },
        );
        idx.insert("p1", "fresh", FileEntry { file_id: "b".into(), created_at: now });
        for i in 0..(MAX_PER_PROVIDER + 5) {
            idx.insert(
                "p2",
                &format!("h{i}"),
                FileEntry { file_id: format!("f{i}"), created_at: now - i as u64 },
            );
        }
        assert!(idx.evict(now));
        assert!(idx.lookup("p1", "old").is_none(), "过期条目必须被驱逐");
        assert!(idx.lookup("p1", "fresh").is_some());
        assert_eq!(idx.map["p2"].len(), MAX_PER_PROVIDER);
        assert!(idx.lookup("p2", "h0").is_some(), "最新条目保留");
        assert!(idx.lookup("p2", "h516").is_none(), "最旧条目丢弃");
        assert!(idx.lookup("p2", "h511").is_some());
    }

    #[test]
    fn evict_noop_reports_unchanged() {
        let mut idx = FileIndex::default();
        let now = 1_000_000_000_000u64;
        idx.insert("p", "h", FileEntry { file_id: "f".into(), created_at: now });
        assert!(!idx.evict(now));
    }

    #[test]
    fn corrupt_index_falls_back_to_empty() {
        let dir = temp_dir("corrupt");
        std::fs::write(FileIndex::path_for(&dir), "not json").unwrap();
        let idx = FileIndex::load(&dir);
        assert!(idx.lookup("p", "h").is_none());
    }
}
