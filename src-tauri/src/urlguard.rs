// SSRF guard: loopback / private-network endpoints are refused unless the
// provider carries an explicit user consent flag (allow_local). Local model
// runners (Ollama, llama.cpp) are the intended use of that flag. Hostnames
// are additionally RESOLVED and every answer classified — a public-looking
// name that answers with a private IP (nip.io / sslip.io style services,
// rebinding resolvers) is treated as internal.
use std::net::{IpAddr, ToSocketAddrs};
use std::time::Duration;

pub fn is_loopback_or_private(host: &str) -> bool {
    let host = host.trim().trim_start_matches('[').trim_end_matches(']');
    // literal IP
    if let Ok(ip) = host.parse::<IpAddr>() {
        return classify_ip(&ip);
    }
    // localhost variants
    let lower = host.to_ascii_lowercase();
    if lower == "localhost" || lower.ends_with(".localhost") || lower == "local" || lower.ends_with(".local") {
        return true;
    }
    // common home/hostnames that usually resolve into private space
    lower.ends_with(".lan") || lower.ends_with(".internal") || lower.ends_with(".home") || lower == "host.docker.internal"
}

fn classify_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            // loopback, RFC1918, link-local, 0.0.0.0, CGNAT, reserved broadcast
            o[0] == 127 || o[0] == 10 || (o[0] == 172 && (o[1] & 0xf0) == 16) || (o[0] == 192 && o[1] == 168)
                || (o[0] == 169 && o[1] == 254) || o[0] == 0 || (o[0] == 100 && (o[1] & 0xc0) == 64)
                || o[0] == 255
        }
        IpAddr::V6(v6) => {
            // IPv4-mapped IPv6 (::ffff:0:0/96) must be judged by its
            // embedded IPv4 address — ::ffff:127.0.0.1 is loopback and
            // ::ffff:10.0.0.1 is private, not public IPv6
            if let Some(v4) = v6.to_ipv4_mapped() {
                return classify_ip(&IpAddr::V4(v4));
            }
            let seg = v6.segments();
            // loopback, link-local fe80::/10, unique-local fc00::/7
            v6.is_loopback() || (seg[0] & 0xffc0) == 0xfe80 || (seg[0] & 0xfe00) == 0xfc00
        }
    }
}

/// Resolve `host` and report whether ANY resolved address is
/// loopback/private. Bounded: DNS must answer within 3s on a worker thread
/// (std `to_socket_addrs` has no timeout, and a hung resolver would stall
/// the caller) — unresolvable names return None, which callers treat as
/// refuse (the subsequent HTTP request would hang on the same resolver).
fn resolves_into_private(host: &str, port: u16) -> Option<bool> {
    let (tx, rx) = std::sync::mpsc::channel::<Vec<IpAddr>>();
    let target = (host.to_string(), port);
    std::thread::spawn(move || {
        let ips = target
            .to_socket_addrs()
            .map(|it| it.map(|a| a.ip()).collect::<Vec<_>>())
            .unwrap_or_default();
        let _ = tx.send(ips);
    });
    match rx.recv_timeout(Duration::from_secs(3)) {
        Ok(ips) if !ips.is_empty() => Some(ips.iter().any(|ip| classify_ip(ip))),
        // resolution failed or timed out — the caller refuses (fail-closed)
        _ => None,
    }
}

#[derive(Debug)]
pub enum UrlCheck {
    // `base` carries the normalized origin for future callers (catalog
    // normalization); kept even though current call sites only pattern-match.
    Ok {
        #[allow(dead_code)]
        base: String,
    },
    Refused(String),
}

/// Validates a provider base URL against the SSRF policy.
/// Returns the normalized origin (+ any path prefix) on success.
pub fn check_base_url(raw: &str, allow_local: bool) -> UrlCheck {
    let raw = raw.trim();
    if raw.is_empty() {
        return UrlCheck::Refused("Base URL 为空".into());
    }
    let parsed = match url::Url::parse(raw) {
        Ok(u) => u,
        Err(_) => return UrlCheck::Refused(format!("无法解析 Base URL: {raw}")),
    };
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return UrlCheck::Refused(format!("不支持的协议: {}", parsed.scheme()));
    }
    let host = match parsed.host_str() {
        Some(h) if !h.is_empty() => h.to_string(),
        _ => return UrlCheck::Refused("Base URL 缺少主机名".into()),
    };
    if is_loopback_or_private(&host) && !allow_local {
        return UrlCheck::Refused(format!(
            "端点 {host} 位于本机/内网 —— 出于 SSRF 防护默认拒绝；确属本地模型服务（如 Ollama）时，请在 Provider 设置中开启「允许本地/内网地址」"
        ));
    }
    if !allow_local && host.parse::<IpAddr>().is_err() {
        // the string check passed and the host is a NAME — it can still
        // answer with private addresses (nip.io/sslip.io subdomain
        // services, rebinding resolvers): resolve and classify every IP
        let port = parsed
            .port()
            .unwrap_or(if parsed.scheme() == "https" { 443 } else { 80 });
        match resolves_into_private(&host, port) {
            Some(false) => {}
            Some(true) => {
                return UrlCheck::Refused(format!(
                    "端点 {host} 解析到本机/内网地址 —— 出于 SSRF 防护默认拒绝；确属本地模型服务（如 Ollama）时，请在 Provider 设置中开启「允许本地/内网地址」"
                ));
            }
            None => {
                return UrlCheck::Refused(format!(
                    "无法解析端点主机 {host}（3 秒超时或域名不存在）—— 出于 SSRF 防护拒绝；请检查域名拼写，或直接填 IP"
                ));
            }
        }
    }
    let port = parsed.port().map(|p| format!(":{p}")).unwrap_or_default();
    let mut base = format!("{}://{}{}", parsed.scheme(), host, port);
    let path = parsed.path().trim_end_matches('/');
    if !path.is_empty() && path != "/" {
        base.push_str(path);
    }
    UrlCheck::Ok { base }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_local_without_consent() {
        assert!(matches!(check_base_url("http://localhost:11434/v1", false), UrlCheck::Refused(_)));
        assert!(matches!(check_base_url("http://127.0.0.1:8080", false), UrlCheck::Refused(_)));
        assert!(matches!(check_base_url("http://192.168.1.5/v1", false), UrlCheck::Refused(_)));
    }

    #[test]
    fn allows_local_with_consent() {
        assert!(matches!(check_base_url("http://localhost:11434/v1", true), UrlCheck::Ok { .. }));
    }

    #[test]
    fn allows_public() {
        let r = check_base_url("https://api.deepseek.com/v1", false);
        match r {
            UrlCheck::Ok { base } => assert_eq!(base, "https://api.deepseek.com/v1"),
            _ => panic!("public url refused"),
        }
    }

    #[test]
    fn refuses_bad_scheme() {
        assert!(matches!(check_base_url("file:///etc/passwd", false), UrlCheck::Refused(_)));
    }

    #[test]
    fn resolution_classifies_localhost_as_private() {
        // real getaddrinfo on a name every OS resolves locally (hosts file,
        // no network dependency): the resolved 127.0.0.1 / ::1 classifies
        // private — this is the path nip.io-style names would take
        assert_eq!(resolves_into_private("localhost", 80), Some(true));
    }

    #[test]
    fn literal_ip_hosts_skip_resolution() {
        // a literal IP never re-enters the resolver; classification is
        // direct (and unresolvable garbage names are not looked up either)
        assert!(matches!(check_base_url("http://8.8.8.8/v1", false), UrlCheck::Ok { .. }));
        assert!(matches!(check_base_url("http://10.0.0.3/v1", false), UrlCheck::Refused(_)));
    }

    #[test]
    fn ipv4_mapped_ipv6_classifies_as_v4() {
        assert!(is_loopback_or_private("::ffff:127.0.0.1"));
        assert!(is_loopback_or_private("::ffff:192.168.1.5"));
        assert!(is_loopback_or_private("::ffff:10.0.0.7"));
        assert!(is_loopback_or_private("::ffff:169.254.1.9"));
        assert!(!is_loopback_or_private("::ffff:8.8.8.8"));
        assert!(!is_loopback_or_private("::1") == false); // v6 loopback itself
        // a real public v6 stays public
        assert!(!is_loopback_or_private("2606:4700::1111"));
    }
}
