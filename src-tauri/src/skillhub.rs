// SkillHub (skillhub.cn) marketplace integration.
//
// Endpoints (probed from the official web app, no auth needed):
//   GET {BASE}/api/skills?page&pageSize&sortBy&keyword   → {code:0,data:{skills:[],total}}
//   GET {BASE}/api/v1/download?slug&namespace&version    → 302 → zip (SKILL.md + files)
//
// Install = download zip, extract SKILL.md, write it to
// ~/.ccharness/skills/<slug>.md with normalized frontmatter (name = slug so
// /slash works; description = zh summary). Only the prompt layer is
// installed — scripts inside the package are NOT executed.

use crate::config::MarketSkill;
use serde_json::json;
use std::io::Read;
use std::path::PathBuf;

pub const BASE: &str = "https://api.skillhub.cn";
const LIST_TIMEOUT_SECS: u64 = 20;
const DOWNLOAD_TIMEOUT_SECS: u64 = 60;
const MAX_ZIP_BYTES: usize = 10 * 1024 * 1024;

fn web_headers() -> reqwest::header::HeaderMap {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
    let mut h = HeaderMap::new();
    if let Ok(o) = HeaderValue::from_str("https://www.skillhub.cn") {
        h.insert(HeaderName::from_static("origin"), o);
    }
    if let Ok(r) = HeaderValue::from_str("https://www.skillhub.cn/") {
        h.insert(HeaderName::from_static("referer"), r);
    }
    h
}

pub async fn fetch_market(
    client: &reqwest::Client,
    page: u32,
    page_size: u32,
    sort_by: &str,
    keyword: &str,
) -> Result<(Vec<MarketSkill>, u64), String> {
    let mut url = format!("{BASE}/api/skills?page={page}&pageSize={page_size}&sortBy={sort_by}");
    let kw = keyword.trim();
    if !kw.is_empty() {
        url.push_str(&format!("&keyword={}", urlencode(kw)));
    }
    let resp = client
        .get(&url)
        .headers(web_headers())
        .timeout(std::time::Duration::from_secs(LIST_TIMEOUT_SECS))
        .send()
        .await
        .map_err(|e| format!("网络错误: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("SkillHub 返回 {}", resp.status()));
    }
    let v: serde_json::Value = resp.json().await.map_err(|e| format!("响应不是 JSON: {e}"))?;
    if v.get("code").and_then(|c| c.as_i64()) != Some(0) {
        return Err(format!("SkillHub 接口错误: {}", v.get("message").and_then(|m| m.as_str()).unwrap_or("未知")));
    }
    let skills: Vec<MarketSkill> = v
        .pointer("/data/skills")
        .and_then(|s| s.as_array())
        .map(|arr| serde_json::from_value(json!(arr)).unwrap_or_default())
        .unwrap_or_default();
    let total = v.pointer("/data/total").and_then(|t| t.as_u64()).unwrap_or(0);
    Ok((skills, total))
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Download the package and install its SKILL.md as ~/.ccharness/skills/<slug>.md.
/// Returns the installed path and the number of files the package contained.
pub async fn install_skill(
    client: &reqwest::Client,
    slug: &str,
    namespace: &str,
    version: Option<&str>,
    description: &str,
) -> Result<(String, usize), String> {
    let slug_t = slug.trim();
    if slug_t.is_empty() || !slug_t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(format!("非法 slug: {slug}"));
    }
    let mut url = format!("{BASE}/api/v1/download?slug={}", urlencode(slug_t));
    if !namespace.trim().is_empty() {
        url.push_str(&format!("&namespace={}", urlencode(namespace.trim())));
    }
    if let Some(v) = version {
        if !v.trim().is_empty() {
            url.push_str(&format!("&version={}", urlencode(v.trim())));
        }
    }

    // 302 → COS zip; reqwest follows redirects by default
    let resp = client
        .get(&url)
        .headers(web_headers())
        .timeout(std::time::Duration::from_secs(DOWNLOAD_TIMEOUT_SECS))
        .send()
        .await
        .map_err(|e| format!("下载失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("下载返回 {}", resp.status()));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("读取包失败: {e}"))?
        .to_vec();
    if bytes.len() > MAX_ZIP_BYTES {
        return Err(format!("包超过 {MAX_ZIP_BYTES} 字节上限"));
    }

    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| format!("解包失败: {e}"))?;
    let file_count = archive.len();

    // locate SKILL.md (root or any depth — packages usually nest it)
    let names: Vec<String> = archive.file_names().map(|s| s.to_string()).collect();
    let target = names
        .iter()
        .find(|n| n.ends_with("SKILL.md") && !n.contains(".."))
        .ok_or("包内未找到 SKILL.md")?
        .clone();
    let mut f = archive
        .by_name(&target)
        .map_err(|e| format!("读取 {target} 失败: {e}"))?;
    if f.size() > 2 * 1024 * 1024 {
        return Err("SKILL.md 超过 2MB".into());
    }
    let mut raw = String::new();
    f.read_to_string(&mut raw).map_err(|e| format!("SKILL.md 不是 UTF-8 文本: {e}"))?;
    let skill_md: String = raw;

    // normalize: our scanner reads name/description/inject from frontmatter;
    // force name = slug (safe /slash identifier) and keep the original body
    let body = strip_frontmatter(&skill_md);
    let md = format!(
        "---\nname: {slug_t}\ndescription: {}\ninject: command\n---\n{}",
        escape_frontmatter(description),
        body.trim_start()
    );

    let dir = install_dir()?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建技能目录失败: {e}"))?;
    let path = dir.join(format!("{slug_t}.md"));
    std::fs::write(&path, md).map_err(|e| format!("写入失败: {e}"))?;
    Ok((path.display().to_string(), file_count))
}

fn strip_frontmatter(raw: &str) -> &str {
    let rest = match raw.strip_prefix("---") {
        Some(r) => r,
        None => return raw,
    };
    match rest.find("\n---") {
        Some(i) => rest[i + 4..].trim_start(),
        None => raw,
    }
}

fn escape_frontmatter(s: &str) -> String {
    s.replace('\n', " ").replace('\r', "").chars().take(200).collect()
}

pub fn install_dir() -> Result<PathBuf, String> {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .ok_or("无法确定用户主目录")?;
    Ok(home.join(".ccharness").join("skills"))
}

// ---------- plugins (skillhub.cn/plugins → api/v1/plugins) ----------
//
// The plugins listing lives at GET {BASE}/api/v1/plugins?page&pageSize&category
// → {items:[{fullName,name,owner,description,avatarUrl,categoryKey,stars,forks,
//   license,installability,repositoryUrl,defaultBranch,topics}], total}.
// There is no package-download API (POST variants return 405), so install
// pulls the GitHub archive (codeload zip of the default branch) and extracts
// every SKILL.md as a global skill — prompt layer only, scripts never run.

pub const PLUGIN_ZIP_CAP: usize = 30 * 1024 * 1024;
const PLUGIN_SKILL_CAP: usize = 20;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MarketPlugin {
    #[serde(rename = "fullName", default)]
    pub full_name: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub owner: String,
    #[serde(default)]
    pub description: String,
    #[serde(rename = "avatarUrl", default)]
    pub avatar_url: Option<String>,
    #[serde(rename = "categoryKey", default)]
    pub category_key: String,
    #[serde(default)]
    pub stars: Option<u64>,
    #[serde(default)]
    pub forks: Option<u64>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(rename = "installability", default)]
    pub installability: String,
    #[serde(rename = "repositoryUrl", default)]
    pub repository_url: String,
    #[serde(rename = "defaultBranch", default)]
    pub default_branch: String,
    #[serde(default)]
    pub topics: Vec<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct PluginPage {
    pub plugins: Vec<MarketPlugin>,
    pub total: u64,
}

pub async fn fetch_plugins(
    client: &reqwest::Client,
    page: u32,
    page_size: u32,
    category: &str,
) -> Result<PluginPage, String> {
    let mut url = format!("{BASE}/api/v1/plugins?page={page}&pageSize={page_size}");
    let cat = category.trim();
    if !cat.is_empty() {
        url.push_str(&format!("&category={}", urlencode(cat)));
    }
    let resp = client
        .get(&url)
        .headers(web_headers())
        .timeout(std::time::Duration::from_secs(LIST_TIMEOUT_SECS))
        .send()
        .await
        .map_err(|e| format!("网络错误: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("SkillHub 返回 {}", resp.status()));
    }
    let v: serde_json::Value = resp.json().await.map_err(|e| format!("响应不是 JSON: {e}"))?;
    let plugins: Vec<MarketPlugin> = v
        .get("items")
        .cloned()
        .map(|items| serde_json::from_value(items).unwrap_or_default())
        .unwrap_or_default();
    let total = v.get("total").and_then(|t| t.as_u64()).unwrap_or(0);
    Ok(PluginPage { plugins, total })
}

fn valid_repo_part(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn skill_slug(plugin: &str, folder: &str) -> String {
    let clean = |s: &str| -> String {
        let mapped: String = s
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
            .collect();
        let mut out = mapped.to_lowercase();
        while out.contains("--") {
            out = out.replace("--", "-");
        }
        out.trim_matches('-').to_string()
    };
    let f = clean(folder);
    if f.is_empty() {
        clean(plugin)
    } else {
        format!("{}-{}", clean(plugin), f)
    }
}

/// Install a plugin from the SkillHub registry: download the GitHub archive
/// of its default branch, extract every SKILL.md as a global skill
/// (~/.ccharness/skills/<plugin>-<folder>.md). Returns the installed skill
/// slugs and the archive file count.
pub async fn install_plugin(
    client: &reqwest::Client,
    owner: &str,
    name: &str,
    default_branch: &str,
    description: &str,
) -> Result<(Vec<String>, usize), String> {
    if !valid_repo_part(owner) || !valid_repo_part(name) {
        return Err("非法插件标识".into());
    }
    let branch = if default_branch.trim().is_empty() { "main" } else { default_branch.trim() };
    if !valid_repo_part(branch) {
        return Err("非法分支名".into());
    }
    let url = format!(
        "https://codeload.github.com/{owner}/{name}/zip/refs/heads/{}",
        urlencode(branch)
    );
    let resp = client
        .get(&url)
        .timeout(std::time::Duration::from_secs(DOWNLOAD_TIMEOUT_SECS * 2))
        .send()
        .await
        .map_err(|e| format!("下载失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("下载返回 {}", resp.status()));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("读取包失败: {e}"))?
        .to_vec();
    if bytes.len() > PLUGIN_ZIP_CAP {
        return Err(format!("包超过 {PLUGIN_ZIP_CAP} 字节上限"));
    }
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| format!("解包失败: {e}"))?;
    let file_count = archive.len();

    // locate every SKILL.md (skip path-traversal entries), cap the count
    let mut targets: Vec<(String, String)> = Vec::new();
    for n in archive.file_names().map(|s| s.to_string()) {
        if targets.len() >= PLUGIN_SKILL_CAP {
            break;
        }
        if n.ends_with("SKILL.md") && !n.contains("..") {
            let segs: Vec<&str> = n.split('/').collect();
            // segs = [archive-root, …, folder?, "SKILL.md"]
            let folder = if segs.len() >= 3 { segs[segs.len() - 2] } else { "" };
            targets.push((folder.to_string(), n));
        }
    }
    if targets.is_empty() {
        return Err(
            "包内未找到 SKILL.md —— 该插件多为客户端运行时插件，CCHarness 仅安装提示层技能，故无可安装内容".into(),
        );
    }

    let dir = install_dir()?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建技能目录失败: {e}"))?;
    let mut installed: Vec<String> = Vec::new();
    for (folder, entry) in targets {
        let mut f = match archive.by_name(&entry) {
            Ok(f) => f,
            Err(_) => continue,
        };
        if f.size() > 2 * 1024 * 1024 {
            continue;
        }
        let mut raw = String::new();
        if f.read_to_string(&mut raw).is_err() {
            continue; // non-UTF-8 SKILL.md — skip rather than fail the whole install
        }
        let body = strip_frontmatter(&raw);
        let slug = skill_slug(name, &folder);
        let desc = if folder.is_empty() {
            description.to_string()
        } else {
            format!("{description}（{folder}）")
        };
        let md = format!(
            "---\nname: {slug}\ndescription: {}\ninject: command\n---\n{}",
            escape_frontmatter(&desc),
            body.trim_start()
        );
        let path = dir.join(format!("{slug}.md"));
        std::fs::write(&path, md).map_err(|e| format!("写入失败: {e}"))?;
        installed.push(slug);
    }
    if installed.is_empty() {
        return Err("未安装任何技能（SKILL.md 均不可读）".into());
    }
    Ok((installed, file_count))
}
