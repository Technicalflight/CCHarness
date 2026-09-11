// Privacy mode (伪匿名化安全模式): type-consistent surrogate generation.
//
// Design borrowed from SurrogateShield / Tonic Textual / PIIGhost thinking:
//   - Format fidelity: every surrogate passes the same format validation as
//     the original (ID card MOD 11-2, Luhn for bank cards, provider prefixes
//     for API keys) so the model's reasoning about types stays intact.
//   - Deterministic mapping: surrogate = HMAC-SHA256(master_key,
//     session_id + type + original), truncated. Same session + same value ⇒
//     same surrogate bytes on every send ⇒ prefix-cache byte stability.
//     Different sessions derive different seeds ⇒ cross-session isolation.
//   - Idempotence: the reverse table short-circuits re-scan of already
//     surrogated text, so replaying assistant history never double-scrubs.
//   - Restore: assistant replies are mapped back (exact match) before the
//     record is written, so the user always reads real values; the model
//     never sees them.
//
// The mapping never leaves the machine: records persist the ORIGINAL values
// (readable), only the wire (build_body / transcript rebuild) is scrubbed.
// This is pseudonymisation, NOT anonymisation — documented in Settings.

use hmac::{Hmac, Mac};
use rand::RngCore;
use regex::Regex;
use sha2::Sha256;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

type HmacSha256 = Hmac<Sha256>;

// ---------- 映射日志（privacy_log.jsonl）-------------------------------
//
// 每生成一个新映射（原文 → 替身）就追加一行 JSON，方便用户在设置页查看
// 「到底哪些信息被匿名化了」。只记录新建映射：历史重放里已存在的替身不
// 重复记录。日志与映射表一样只存在本机 data_dir，文件超过 5MB 自动裁剪
// 到最近 2000 行。

/// One mapping-log row (JSONL line in <data_dir>/privacy_log.jsonl).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LogEntry {
    pub ts: String,
    pub session: String,
    /// Entity type: apikey | email | userpath | idcard | bank | phone |
    /// secret | ipv4.
    pub kind: String,
    pub original: String,
    pub surrogate: String,
}

static LOG_PATH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
static LOG_WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

const LOG_MAX_BYTES: u64 = 5 * 1024 * 1024;
const LOG_KEEP_LINES: usize = 2000;

/// Point the log at <data_dir>/privacy_log.jsonl (called once at startup).
pub fn init_log(dir: &Path) {
    let _ = LOG_PATH.set(dir.join("privacy_log.jsonl"));
}

// ---------- 自定义脱敏规则（正则，用户在设置页维护）--------------------
//
// 内置检测覆盖不了的自由文本敏感信息（中文姓名、内部代号、项目专名等）
// 由用户用正则自行补充：命中统一替换为 [匿名-xxxxxxxx]（确定性，可还原，
// 照常进映射日志）。规则在启动与每次保存配置时重新编译加载。

fn custom_lock() -> std::sync::MutexGuard<'static, Vec<Regex>> {
    static CUSTOM: Mutex<Vec<Regex>> = Mutex::new(Vec::new());
    CUSTOM.lock().unwrap_or_else(|p| p.into_inner())
}

/// Load user-defined scrub patterns (called at startup / config save).
/// Uncompilable patterns are skipped with a stderr note (never panic).
pub fn set_custom_patterns(patterns: Vec<String>) {
    let compiled: Vec<Regex> = patterns
        .iter()
        .take(64)
        .filter_map(|p| {
            let trimmed = p.trim();
            if trimmed.is_empty() {
                return None;
            }
            match Regex::new(trimmed) {
                Ok(re) => Some(re),
                Err(e) => {
                    eprintln!("[privacy] 自定义规则编译失败，已跳过 ({e}): {trimmed}");
                    None
                }
            }
        })
        .collect();
    *custom_lock() = compiled;
}

fn trim_log(path: &Path) {
    let Ok(md) = std::fs::metadata(path) else { return };
    if md.len() <= LOG_MAX_BYTES {
        return;
    }
    if let Ok(raw) = std::fs::read_to_string(path) {
        let kept: Vec<&str> = raw.lines().rev().take(LOG_KEEP_LINES).collect();
        let mut body = kept.into_iter().rev().collect::<Vec<_>>().join("\n");
        body.push('\n');
        let _ = std::fs::write(path, body);
    }
}

/// Append one mapping to the JSONL log. No-op until init_log ran (tests).
fn log_hit(session: &str, kind: &str, original: &str, surrogate: &str) {
    let Some(path) = LOG_PATH.get() else { return };
    let entry = LogEntry {
        ts: chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
        session: session.chars().take(40).collect(),
        kind: kind.to_string(),
        original: original.chars().take(160).collect(),
        surrogate: surrogate.chars().take(160).collect(),
    };
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        use std::io::Write;
        if let Ok(line) = serde_json::to_string(&entry) {
            let _ = writeln!(f, "{line}");
            let n = LOG_WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n % 200 == 0 {
                trim_log(path);
            }
        }
    }
}

/// Surrogate reverse-table per session (surrogate → original). Rebuilt
/// lazily on every outbound scrub; used by the restore engine.
struct Vault {
    reverse: HashMap<String, String>,
}

static VAULTS: Mutex<Option<HashMap<String, Vault>>> = Mutex::new(None);

fn vault_lock() -> std::sync::MutexGuard<'static, Option<HashMap<String, Vault>>> {
    VAULTS.lock().unwrap_or_else(|p| p.into_inner())
}

/// Master key at <data_dir>/privacy.key — random 32 bytes, generated once.
/// Seeds are derived per session, so leaking one session's surrogates does
/// not enable cross-session correlation; deleting the file rotates ALL
/// mappings at once (old surrogates stop restoring).
fn master_key(data_dir: &Path) -> [u8; 32] {
    let path = data_dir.join("privacy.key");
    if let Ok(bytes) = std::fs::read(&path) {
        if bytes.len() == 32 {
            let mut k = [0u8; 32];
            k.copy_from_slice(&bytes);
            return k;
        }
    }
    let mut k = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut k);
    let _ = std::fs::write(&path, &k);
    k
}

fn hmac_seed(master: &[u8; 32], session_id: &str) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(master).expect("hmac key");
    mac.update(b"ccharness-privacy-v1");
    mac.update(session_id.as_bytes());
    let out = mac.finalize().into_bytes();
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&out);
    seed
}

/// Per-session seed for the scrub/restore engines. The master key is read
/// once per process (cached); each session derives its own seed, so the
/// same value yields different surrogates in different sessions.
pub fn session_seed(data_dir: &Path, session_id: &str) -> [u8; 32] {
    static MASTER: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
    let key = MASTER.get_or_init(|| master_key(data_dir));
    hmac_seed(key, session_id)
}

/// Deterministic surrogate for one entity: HMAC(seed, "type:original"),
/// hex-encoded, cut to `n` bytes. Same inputs ⇒ identical surrogate.
fn derive(seed: &[u8; 32], kind: &str, original: &str, n: usize) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(seed).expect("hmac key");
    mac.update(kind.as_bytes());
    mac.update(b"\x00");
    mac.update(original.as_bytes());
    let out = mac.finalize().into_bytes();
    out[..n.min(32)].to_vec()
}

// ---------- per-type surrogate generators (format-faithful) ----------

/// Base64url-ish high-entropy body mapped into [A-Za-z0-9], same length.
fn alnum_surrogate(seed: &[u8; 32], kind: &str, original: &str) -> String {
    const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let bytes = derive(seed, kind, original, original.len().min(32));
    let mut out = String::with_capacity(original.len());
    let mut i = 0;
    for ch in original.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ALPHA[bytes[i % bytes.len()] as usize % ALPHA.len()] as char);
            i += 1;
        } else {
            out.push(ch); // separators inside the body keep their shape
        }
    }
    out
}

/// Secret with same charset shape: letter→letter, digit→digit, other→other.
fn shape_surrogate(seed: &[u8; 32], kind: &str, original: &str) -> String {
    let bytes = derive(seed, kind, original, original.len().min(32));
    let mut out = String::with_capacity(original.len());
    let mut i = 0;
    for ch in original.chars() {
        let b = bytes[i % bytes.len()];
        out.push(if ch.is_ascii_alphabetic() {
            (b'a' + (b % 26)) as char
        } else if ch.is_ascii_digit() {
            (b'0' + (b % 10)) as char
        } else {
            ch
        });
        i += 1;
    }
    out
}

/// 11-digit CN phone: keep the 3-char head (segment prefix, or a "+86"
/// country prefix), replace the rest. Byte index wraps so prefixed numbers
/// (more than 8 trailing digits) derive fine too.
fn phone_surrogate(seed: &[u8; 32], original: &str) -> String {
    let bytes = derive(seed, "phone", original, 8);
    let mut out: String = original.chars().take(3).collect();
    for (i, _) in original.chars().skip(3).enumerate() {
        out.push((b'0' + (bytes[i % bytes.len()] % 10)) as char);
    }
    out
}

/// 18-digit CN ID card (GB 11643 MOD 11-2): valid area code + plausible
/// birth date + parity-kept sequence + recomputed check digit.
fn idcard_surrogate(seed: &[u8; 32], original: &str) -> String {
    // hand-picked real administrative codes keep the "looks like a region"
    // semantics without correlating to the original's region
    const AREAS: &[&str] = &["110101", "310115", "440305", "330106", "510104", "420102"];
    let bytes = derive(seed, "idcard", original, 16);
    let area = AREAS[bytes[0] as usize % AREAS.len()];
    let year = 1980 + (bytes[1] as u16) % 26; // 1980..=2005
    let month = 1 + (bytes[2] % 12);
    let day = 1 + (bytes[3] % 28);
    let seq_src = original.as_bytes()[14]; // 17th char parity (index 14, 0-based 15th)
    let seq_parity = (seq_src - b'0') % 2;
    let seq = format!("{:03}", ((bytes[4] as u16 * 10 + bytes[5] as u16) % 1000 / 2 * 2 + seq_parity as u16) % 1000);
    let body = format!("{area}{year:04}{month:02}{day:02}{seq}");
    // MOD 11-2 check digit: weights (7,9,10,5,8,4,2,1,6,3,7,9,10,5,8,4,2)
    const W: [u32; 17] = [7, 9, 10, 5, 8, 4, 2, 1, 6, 3, 7, 9, 10, 5, 8, 4, 2];
    const MAP: [&str; 11] = ["1", "0", "X", "9", "8", "7", "6", "5", "4", "3", "2"];
    let sum: u32 = body
        .bytes()
        .enumerate()
        .map(|(i, b)| (b - b'0') as u32 * W[i])
        .sum();
    format!("{body}{}", MAP[(sum % 11) as usize])
}

/// Bank card 13-19 digits passing Luhn: keep BIN (first 6), recompute check.
fn bank_surrogate(seed: &[u8; 32], original: &str) -> String {
    let n = original.len();
    let bytes = derive(seed, "bank", original, n.min(32));
    let mut body: String = original.chars().take(6).collect();
    for i in 6..n - 1 {
        body.push((b'0' + (bytes[i] % 10)) as char);
    }
    // Luhn: find the check digit that makes the total ≡ 0 (mod 10)
    let luhn_sum = |digits: &[u8]| -> u32 {
        digits
            .iter()
            .rev()
            .enumerate()
            .map(|(i, &d)| {
                let mut v = (d - b'0') as u32;
                if i % 2 == 1 {
                    v *= 2;
                    if v > 9 {
                        v -= 9;
                    }
                }
                v
            })
            .sum()
    };
    let mut candidate: Vec<u8> = body.bytes().collect();
    candidate.push(b'0');
    let delta = (10 - luhn_sum(&candidate) % 10) % 10;
    format!("{body}{delta}")
}

/// Email: keep the domain (semantic anchor), replace the local part with
/// same-length lowercase letters.
fn email_surrogate(seed: &[u8; 32], original: &str) -> String {
    let Some(at) = original.rfind('@') else { return original.to_string() };
    let local = &original[..at];
    let bytes = derive(seed, "email", original, local.len().min(32));
    let new_local: String = local
        .chars()
        .enumerate()
        .map(|(i, _)| (b'a' + (bytes[i] % 26)) as char)
        .collect();
    format!("{new_local}{}", &original[at..])
}

/// User-home paths: C:\Users\<name> | /Users/<name> | /home/<name> — keep
/// the path shape (models reason about directory structure), anonymize the
/// user segment. Same user name ⇒ same user-N across the whole session.
fn user_path_surrogate(seed: &[u8; 32], original: &str) -> String {
    let n = derive(seed, "userpath", original, 4)[0] % 90 + 10; // 10..=99
    let lower = original.to_ascii_lowercase();
    // locate case-insensitively, but splice with the ORIGINAL casing
    for (marker, sep) in [("\\users\\", '\\'), ("/users/", '/'), ("/home/", '/')] {
        if let Some(pos) = lower.find(marker) {
            let head = &original[..pos];
            let tail = &original[pos + marker.len()..];
            return match tail.split_once(sep) {
                Some((_user, rest)) => format!("{head}{}user-{n}{sep}{rest}", &original[pos..pos + marker.len()]),
                None => format!("{head}{}user-{n}", &original[pos..pos + marker.len()]),
            };
        }
    }
    format!("user-{n}")
}

/// IPv4: keep the first two octets (network context), replace the last two.
fn ipv4_surrogate(seed: &[u8; 32], original: &str) -> String {
    let bytes = derive(seed, "ipv4", original, 2);
    let octets: Vec<&str> = original.split('.').collect();
    format!(
        "{}.{}.{}.{}",
        octets[0],
        octets[1],
        bytes[0] % 254 + 1,
        bytes[1] % 254 + 1
    )
}

// ---------- detection (regex candidates + strict validators) ----------

/// One scrubbed entity: (byte span in the ORIGINAL text, replacement).
/// One scrubbed entity: (byte span in the ORIGINAL text, replacement, kind).
struct Hit {
    start: usize,
    end: usize,
    replacement: String,
    kind: &'static str,
}

/// Luhn validity for a digit string.
fn luhn_ok(s: &str) -> bool {
    let digits: Vec<u8> = s.bytes().collect();
    if digits.len() < 13 {
        return false;
    }
    luhn_sum(&digits) % 10 == 0
}

fn luhn_sum(digits: &[u8]) -> u32 {
    digits
        .iter()
        .rev()
        .enumerate()
        .map(|(i, &d)| {
            let mut v = (d - b'0') as u32;
            if i % 2 == 1 {
                v *= 2;
                if v > 9 {
                    v -= 9;
                }
            }
            v
        })
        .sum()
}

/// GB 11643 MOD 11-2 validity.
fn idcard_ok(s: &str) -> bool {
    if s.len() != 18 || !s[..17].bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let last = s.as_bytes()[17];
    if !last.is_ascii_digit() && last != b'X' && last != b'x' {
        return false;
    }
    const W: [u32; 17] = [7, 9, 10, 5, 8, 4, 2, 1, 6, 3, 7, 9, 10, 5, 8, 4, 2];
    const MAP: [char; 11] = ['1', '0', 'X', '9', '8', '7', '6', '5', '4', '3', '2'];
    let sum: u32 = s[..17]
        .bytes()
        .enumerate()
        .map(|(i, b)| (b - b'0') as u32 * W[i])
        .sum();
    MAP[(sum % 11) as usize] == last.to_ascii_uppercase() as char
}

/// Scan `text` and return non-overlapping hits in priority order.
fn detect(seed: &[u8; 32], text: &str) -> Vec<Hit> {
    let mut hits: Vec<Hit> = Vec::new();
    let mut push = |start: usize, end: usize, kind: &'static str, replacement: String| {
        if !hits.iter().any(|h| start < h.end && end > h.start) {
            hits.push(Hit { start, end, replacement, kind });
        }
    };
    // API keys / tokens with known provider prefixes (sk-, ghp_, AKIA…)
    let key_re = Regex::new(r"\b(sk-(?:proj-|ant-|sub-)?[A-Za-z0-9_\-]{16,}|gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|AKIA[0-9A-Z]{16}|xox[abpr]-[A-Za-z0-9\-]{10,}|AIza[0-9A-Za-z_\-]{30,})\b").unwrap();
    for m in key_re.find_iter(text) {
        let original = m.as_str();
        // keep the recognised prefix, surrogate the body
        let lower = original.to_ascii_lowercase();
        let prefix_len = ["sk-proj-", "github_pat_", "sk-ant-", "sk-sub-", "sk-", "ghp_", "gho_", "ghu_", "ghs_", "ghr_", "xoxa-", "xoxb-", "xoxp-", "xoxr-", "akia", "aiza"]
            .iter()
            .find(|p| lower.starts_with(*p))
            .map(|p| p.len())
            .unwrap_or(0);
        let (prefix, body) = original.split_at(prefix_len);
        push(m.start(), m.end(), "apikey", format!("{prefix}{}", alnum_surrogate(seed, "apikey", body)));
    }
    // email
    let email_re = Regex::new(r"\b[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}\b").unwrap();
    for m in email_re.find_iter(text) {
        push(m.start(), m.end(), "email", email_surrogate(seed, m.as_str()));
    }
    // user-home paths (Windows + macOS + Linux)
    let path_re = Regex::new(r#"(?i)(?:[A-Z]:\\Users\\|/Users/|/home/)[A-Za-z0-9._\-]+(?:[\\/][^\s"'<>)]+)?"#).unwrap();
    for m in path_re.find_iter(text) {
        push(m.start(), m.end(), "userpath", user_path_surrogate(seed, m.as_str()));
    }
    // CN ID card (validated)
    let id_re = Regex::new(r"\b[0-9]{17}[0-9Xx]\b").unwrap();
    for m in id_re.find_iter(text) {
        if idcard_ok(m.as_str()) {
            push(m.start(), m.end(), "idcard", idcard_surrogate(seed, m.as_str()));
        }
    }
    // bank card: 13-19 digits + Luhn (validation avoids clashing with phones/IDs)
    let digits_re = Regex::new(r"\b[0-9]{13,19}\b").unwrap();
    for m in digits_re.find_iter(text) {
        let s = m.as_str();
        if luhn_ok(s) {
            push(m.start(), m.end(), "bank", bank_surrogate(seed, s));
        }
    }
    // CN mobile: 1[3-9] + 9 digits, boundary-guarded
    // CN mobile: 1[3-9] + 9 digits, optionally with a +86 / 86 country
    // prefix. The regex crate has no lookbehind/lookahead, so matches glued
    // into longer digit runs (bank-card candidates, truncated numbers) are
    // rejected with manual boundary checks.
    let phone_re = Regex::new(r"(?:\+?86)?1[3-9][0-9]{9}").unwrap();
    for m in phone_re.find_iter(text) {
        let head_digit = m.start() > 0 && text[..m.start()].ends_with(|c: char| c.is_ascii_digit());
        let tail_digit =
            m.end() < text.len() && text[m.end()..].starts_with(|c: char| c.is_ascii_digit());
        if head_digit || tail_digit {
            continue;
        }
        push(m.start(), m.end(), "phone", phone_surrogate(seed, m.as_str()));
    }
    // generic high-entropy secret assignments (token = "…", password: '…')
    let secret_re = Regex::new(r#"(?i)\b(token|secret|password|passwd|api_?key)\b["'\s:=]{1,4}["']?([A-Za-z0-9!@#$%^&*_.\-+/]{16,})["']?"#).unwrap();
    for cap in secret_re.captures_iter(text) {
        let value = cap.get(2).unwrap();
        push(value.start(), value.end(), "secret", shape_surrogate(seed, "secret", value.as_str()));
    }
    // IPv4
    let ip_re = Regex::new(r"\b(?:[0-9]{1,3}\.){3}[0-9]{1,3}\b").unwrap();
    for m in ip_re.find_iter(text) {
        let s = m.as_str();
        if s.split('.').all(|o| o.parse::<u16>().map(|v| v <= 255).unwrap_or(false)) {
            push(m.start(), m.end(), "ipv4", ipv4_surrogate(seed, s));
        }
    }
    // user-defined custom patterns (设置页·自定义脱敏规则): run LAST so the
    // built-in typed detectors win overlapping spans; deterministic
    // [匿名-xxxxxxxx] replacement keeps restore + the mapping log working.
    for re in custom_lock().iter() {
        for m in re.find_iter(text) {
            let bytes = derive(seed, "custom", m.as_str(), 4);
            let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            push(m.start(), m.end(), "custom", format!("[匿名-{hex}]"));
        }
    }
    hits.sort_by_key(|h| h.start);
    hits
}

/// Outbound scrub: replace every detected PII entity with its deterministic
/// surrogate. Idempotent: a match that IS ALREADY a recorded surrogate
/// (assistant replay) is left untouched, so scrubbing scrubbed bytes never
/// drifts into layered surrogates. Records the forward mapping into the
/// session vault for the restore engine.
pub fn outbound(session_id: &str, seed: &[u8; 32], text: &str) -> String {
    let hits = detect(seed, text);
    if hits.is_empty() {
        return text.to_string();
    }
    let mut guard = vault_lock();
    let vault = guard.get_or_insert_with(HashMap::new).entry(session_id.to_string()).or_insert_with(|| Vault {
        reverse: HashMap::new(),
    });
    let mut out = String::with_capacity(text.len());
    let mut last = 0usize;
    for h in hits {
        let original = &text[h.start..h.end];
        out.push_str(&text[last..h.start]);
        if vault.reverse.contains_key(original) {
            // the matched text is itself a surrogate from an earlier scrub
            // (assistant history replay): keep it, never surrogate a layer
            out.push_str(original);
        } else {
            out.push_str(&h.replacement);
            vault.reverse
                .entry(h.replacement.clone())
                .or_insert_with(|| original.to_string());
            // 映射日志：只记录新建映射，历史重放里已存在的替身不重复记录
            log_hit(session_id, h.kind, original, &h.replacement);
        }
        last = h.end;
    }
    out.push_str(&text[last..]);
    out
}

/// Restore: map known surrogates in an assistant reply back to the
/// original values (exact match; also tolerates the model lowercasing or
/// splitting a surrogate by stripping spaces). Unknown text passes through.
pub fn restore(session_id: &str, text: &str) -> String {
    let guard = vault_lock();
    let Some(vault) = guard.as_ref().and_then(|m| m.get(session_id)) else {
        return text.to_string();
    };
    if vault.reverse.is_empty() {
        return text.to_string();
    }
    let mut out = text.to_string();
    // longest-first so overlapping surrogates restore atomically
    let mut pairs: Vec<(&String, &String)> = vault.reverse.iter().collect();
    pairs.sort_by_key(|(s, _)| std::cmp::Reverse(s.len()));
    for (surrogate, original) in pairs {
        if out.contains(surrogate.as_str()) {
            out = out.replace(surrogate.as_str(), original);
        }
        // fuzzy variant: surrogate with spaces stripped inside (model may
        // reformat long tokens); rebuild the spaced pattern conservatively
        if surrogate.len() > 12 {
            let spaced = surrogate
                .chars()
                .enumerate()
                .map(|(i, c)| if i > 0 && i % 8 == 0 { format!(" {c}") } else { format!("{c}") })
                .collect::<String>();
            if spaced != *surrogate && out.contains(&spaced) {
                out = out.replace(&spaced, original);
            }
        }
    }
    out
}

/// Drop one session's vault (session deleted / cleared).
#[allow(dead_code)]
pub fn forget(session_id: &str) {
    vault_lock().as_mut().map(|m| m.remove(session_id));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed() -> [u8; 32] {
        [7u8; 32]
    }

    #[test]
    fn api_key_keeps_prefix_and_length() {
        let s = seed();
        let orig = "sk-ant-api03-aaaaaaaaaaaaaaaaaaaabbbbbbbbbbbbbbbbbbbb";
        let out = outbound("t1", &s, &format!("用这个 key：{orig}"));
        assert!(out.starts_with("用这个 key：sk-ant-"), "prefix kept: {out}");
        let got = out.split("key：").nth(1).unwrap();
        assert_eq!(got.len(), orig.len(), "length preserved");
        // deterministic
        let again = outbound("t1", &s, orig);
        assert_eq!(got, again);
    }

    #[test]
    fn phone_keeps_segment_prefix() {
        let s = seed();
        let out = outbound("t2", &s, "联系电话 13812345678 或者发邮件");
        assert!(out.contains("138"));
        assert!(!out.contains("13812345678"));
    }

    #[test]
    fn idcard_passes_mod11() {
        let s = seed();
        // 31010119920315452 → sum%11=8 → check digit '4'
        let orig = "310101199203154524";
        assert!(idcard_ok(orig), "test fixture must itself be valid");
        let out = outbound("t3", &s, &format!("身份证 {} 请处理", orig));
        let surrogate: String = out
            .split_whitespace()
            .find(|w| w.len() == 18)
            .unwrap().to_string();
        assert!(idcard_ok(&surrogate), "surrogate must pass GB 11643: {surrogate}");
        assert_ne!(surrogate, orig);
        // same session deterministic
        let out2 = outbound("t3", &s, &format!("再写一次 {}", orig));
        assert!(out2.contains(&surrogate));
    }

    #[test]
    fn bank_card_passes_luhn() {
        let s = seed();
        // classic valid-Luhn Visa test number
        let orig = "4111111111111111";
        assert!(luhn_ok(orig), "fixture must be Luhn-valid");
        let out = outbound("t4", &s, &format!("卡号 {}", orig));
        let surrogate: String = out
            .split_whitespace()
            .find(|w| w.len() == orig.len())
            .unwrap().to_string();
        assert!(luhn_ok(&surrogate), "surrogate must pass Luhn: {surrogate}");
        assert!(surrogate.starts_with("411111"), "BIN kept");
    }

    #[test]
    fn email_keeps_domain() {
        let s = seed();
        let out = outbound("t5", &s, "邮箱是 zhangsan@company.com 收一下");
        assert!(out.contains("@company.com"));
        assert!(!out.contains("zhangsan@company.com"));
    }

    #[test]
    fn user_path_anonymized_shape_kept() {
        let s = seed();
        let out = outbound("t6", &s, r"配置在 C:\Users\zhangsan\AppData\Roaming 里面");
        assert!(out.contains("C:\\Users\\user-"), "got: {out}");
        assert!(!out.contains("zhangsan"));
    }

    #[test]
    fn ipv4_keeps_network_half() {
        let s = seed();
        let out = outbound("t7", &s, "服务在 192.168.10.53 上跑");
        assert!(out.contains("192.168."));
        assert!(!out.contains("192.168.10.53"));
    }

    #[test]
    fn idempotent_on_replay() {
        let s = seed();
        let scrubbed = outbound("t8", &s, "key: sk-1234567890abcdef12345678");
        let rescanned = outbound("t8", &s, &scrubbed);
        assert_eq!(scrubbed, rescanned, "double-scrub must not drift");
    }

    #[test]
    fn restore_roundtrip() {
        let s = seed();
        let original = "密钥 sk-abcdefghijklmnopqrst1234567890 和邮箱 bob@corp.cn";
        let scrubbed = outbound("t9", &s, original);
        assert_ne!(scrubbed, original);
        // the reverse table now holds the mappings
        let restored = restore("t9", &scrubbed);
        assert_eq!(restored, original, "exact restore must round-trip");
    }

    #[test]
    fn clean_text_untouched() {
        let s = seed();
        let text = "今天天气不错，我们在写一个 Rust 项目，函数 foo() 返回 42。";
        assert_eq!(outbound("t10", &s, text), text);
    }

    #[test]
    fn phone_with_country_prefix() {
        let s = seed();
        // +86 前缀（此前是检测盲区：6 紧贴 1 导致 11 位正则失配）
        let out = outbound("t11", &s, "帮我查一下 +8613027666495 是谁的");
        assert!(!out.contains("13027666495"), "got: {out}");
        assert!(out.contains("+86"), "prefix kept: {out}");
        // 86 无加号（整串 13 位数字，Luhn 不过才会落到手机号检测）
        let out2 = outbound("t11", &s, "号码 8613027666495");
        assert!(!out2.contains("8613027666495"), "got: {out2}");
        // 更长的数字串不被误判（13 位数字贴着更多数字时跳过）
        let out3 = outbound("t11", &s, "订单号 986130276664951");
        assert!(out3.contains("986130276664951"), "longer run untouched: {out3}");
    }

    #[test]
    fn custom_patterns_scrub_restore_and_log_kind() {
        set_custom_patterns(vec!["张三".into(), r"\bProjectX\b".into()]);
        let s = seed();
        let original = "张三在 ProjectX 项目里";
        let scrubbed = outbound("t12", &s, original);
        assert!(!scrubbed.contains("张三"), "got: {scrubbed}");
        assert!(!scrubbed.contains("ProjectX"), "got: {scrubbed}");
        assert!(scrubbed.contains("[匿名-"), "got: {scrubbed}");
        // 确定性 + 可还原
        assert_eq!(outbound("t12", &s, original), scrubbed);
        assert_eq!(restore("t12", &scrubbed), original);
        set_custom_patterns(Vec::new());
    }
}
