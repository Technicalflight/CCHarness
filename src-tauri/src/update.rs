// In-app update check against GitHub Releases. The sidebar version button
// runs a manual check; App.tsx may run a silent startup check (throttled to
// once per 24h in the frontend). Nothing is downloaded or installed here —
// a found update opens the Releases page for a manual download.

use serde::{Deserialize, Serialize};

/// Result of an update check. `error` carries a user-facing reason when the
/// check could not complete (offline, private repo without token, rate limit).
#[derive(Debug, Clone, Serialize)]
pub struct UpdateInfo {
    pub current: String,
    /// Latest release version without the leading "v"; None when unknown.
    pub latest: Option<String>,
    pub update_available: bool,
    pub release_name: Option<String>,
    /// Release notes body (markdown as authored on GitHub).
    pub notes: Option<String>,
    /// Release page URL for the system browser.
    pub url: Option<String>,
    pub error: Option<String>,
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    html_url: Option<String>,
}

/// "v0.1.10" / "0.1.9" -> (0, 1, 10). Returns None on non-numeric tags.
fn parse_ver(v: &str) -> Option<(u64, u64, u64)> {
    let t = v.trim();
    let t = t.strip_prefix('v').unwrap_or(t);
    let mut it = t.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    let patch = it.next().unwrap_or("0").trim().parse().ok()?;
    Some((major, minor, patch))
}

fn newer(latest: &str, current: &str) -> bool {
    match (parse_ver(latest), parse_ver(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

fn unknown(current: &str, error: String) -> UpdateInfo {
    UpdateInfo {
        current: current.to_string(),
        latest: None,
        update_available: false,
        release_name: None,
        notes: None,
        url: None,
        error: Some(error),
    }
}

/// Query the latest published GitHub release and compare it with the running
/// version. The repository is public, so the check is anonymous (GitHub's
/// unauthenticated rate limit is ample for a manual / daily check).
#[tauri::command]
pub async fn check_update() -> Result<UpdateInfo, String> {
    let current = env!("CARGO_PKG_VERSION").to_string();
    let url = "https://api.github.com/repos/Technicalflight/CCHarness/releases/latest";
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;
    let req = client
        .get(url)
        .header("User-Agent", format!("CCHarness/{current}"))
        .header("Accept", "application/vnd.github+json");
    let info = match req.send().await {
        Ok(resp) if resp.status().is_success() => match resp.json::<GhRelease>().await {
            Ok(rel) => UpdateInfo {
                update_available: newer(&rel.tag_name, &current),
                latest: Some(rel.tag_name.trim().trim_start_matches('v').to_string()),
                release_name: rel.name,
                notes: rel.body,
                url: rel.html_url,
                current,
                error: None,
            },
            Err(e) => unknown(&current, format!("解析响应失败: {e}")),
        },
        Ok(resp) => {
            let msg = if resp.status() == reqwest::StatusCode::NOT_FOUND {
                "未找到 Releases（仓库不存在或尚无已发布版本）".into()
            } else if resp.status() == reqwest::StatusCode::FORBIDDEN {
                "GitHub API 限流，请稍后再试".into()
            } else if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
                "GitHub API 认证异常".into()
            } else {
                format!("GitHub API 返回 {}", resp.status())
            };
            unknown(&current, msg)
        }
        Err(e) => unknown(&current, format!("网络请求失败: {e}")),
    };
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare() {
        assert!(newer("v0.1.1", "0.1.0"));
        assert!(newer("0.2.0", "0.1.9"));
        assert!(newer("1.0.0", "0.9.99"));
        assert!(newer("0.1.10", "0.1.9"));
        assert!(!newer("v0.1.0", "0.1.0"));
        assert!(!newer("0.1.0", "0.1.1"));
        assert!(!newer("garbage", "0.1.0"));
        assert!(!newer("0.1", "0.1.0"));
    }
}
