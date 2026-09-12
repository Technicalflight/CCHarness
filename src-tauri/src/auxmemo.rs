// AuxMemo — response-side EXACT cache for whitelisted idempotent calls
// (see docs/design/auxmemo-and-enhance.md). Two whitelist kinds exist today:
// title generation and prompt enhancement. The agent main loop is NEVER
// cached — a replayed tool_use round would act on a stale world.
//
// Layering: L1 (bounded in-process LRU) → L2 (per-workspace encrypted disk
// cache) → real call. Keys are exact: SHA-256 over (schemaVer, kind, model,
// endpoint, input, workspaceNS). Normalization lives ONLY in the key — the
// outbound payload is never touched by this module.
//
// Ledger: every lookup outcome is appended to aux-ledger.jsonl with an
// origin ("l1" | "l2" | "miss"), billed and saved amounts. Hits are billed
// 0; misses carry the real call cost. This ledger is disjoint from the main
// loop's RequestStat telemetry — two books, never mixed.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

/// Bump when the entry format or key derivation changes (invalidates L2).
pub const SCHEMA_VER: &str = "2";

const L1_MAX_ENTRIES: usize = 128;
const L1_MAX_TEXT_BYTES: usize = 2 * 1024 * 1024;
const L2_DIR_MAX_BYTES: u64 = 8 * 1024 * 1024;
const MAGIC: &[u8; 4] = b"AUX1";

// ---------- entries / keys ----------

/// Cached value + the stats recorded at insert time, so a later hit can
/// report what the same call would have cost ("saved").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryMeta {
    pub text: String,
    pub model: String,
    #[serde(default)]
    pub in_tok: Option<u64>,
    #[serde(default)]
    pub out_tok: Option<u64>,
    #[serde(default)]
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    L1,
    L2,
    Miss,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::L1 => "l1",
            Origin::L2 => "l2",
            Origin::Miss => "miss",
        }
    }
}

/// Workspace namespace: hash of the bound workspace path (16 hex chars), or
/// "global" when the session has no workspace. Enters the key AND names the
/// L2 directory — physical isolation between workspaces.
pub fn namespace(workspace: Option<&str>) -> String {
    match workspace.map(str::trim).filter(|w| !w.is_empty()) {
        None => "global".to_string(),
        Some(ws) => hex::encode(&Sha256::digest(ws.as_bytes())[..8]),
    }
}

/// Exact cache key. Endpoint is normalized (trailing slash stripped) — the
/// same gateway written with or without "/" must be one identity.
pub fn compute_key(kind: &str, model: &str, endpoint: &str, input: &str, ns: &str) -> String {
    let mut h = Sha256::new();
    h.update(format!("v{SCHEMA_VER}|{kind}|{model}|{}|{input}|{ns}", endpoint.trim_end_matches('/')));
    hex::encode(h.finalize())
}

// ---------- L1: bounded LRU ----------

struct L1State {
    order: VecDeque<String>,
    map: HashMap<String, EntryMeta>,
    text_bytes: usize,
}

static L1: Mutex<Option<L1State>> = Mutex::new(None);

fn l1_get(key: &str) -> Option<EntryMeta> {
    let mut guard = L1.lock().unwrap();
    let st = guard.as_mut()?;
    if let Some(e) = st.map.get(key).cloned() {
        // touch: move to the back of the eviction order
        if let Some(pos) = st.order.iter().position(|k| k == key) {
            st.order.remove(pos);
            st.order.push_back(key.to_string());
        }
        return Some(e);
    }
    None
}

fn l1_put(key: &str, e: EntryMeta) {
    let mut guard = L1.lock().unwrap();
    let st = guard.get_or_insert_with(|| L1State { order: VecDeque::new(), map: HashMap::new(), text_bytes: 0 });
    if let Some(prev) = st.map.insert(key.to_string(), e.clone()) {
        st.text_bytes = st.text_bytes.saturating_sub(prev.text.len());
        if let Some(pos) = st.order.iter().position(|k| k == key) {
            st.order.remove(pos);
        }
    }
    st.order.push_back(key.to_string());
    st.text_bytes += e.text.len();
    // evict oldest until both bounds hold
    while st.order.len() > L1_MAX_ENTRIES || st.text_bytes > L1_MAX_TEXT_BYTES {
        match st.order.pop_front() {
            Some(old) => {
                if let Some(v) = st.map.remove(&old) {
                    st.text_bytes = st.text_bytes.saturating_sub(v.text.len());
                }
            }
            None => break,
        }
    }
}

/// Test-only: drop all L1 entries (simulates a process restart).
#[cfg(test)]
fn l1_clear() {
    *L1.lock().unwrap() = None;
}

// ---------- L2: per-workspace encrypted disk cache ----------

fn l2_dir(data_dir: &Path, ns: &str) -> PathBuf {
    data_dir.join("auxcache").join(ns)
}

fn key_file(data_dir: &Path, ns: &str, key: &str) -> PathBuf {
    l2_dir(data_dir, ns).join(format!("{key}.bin"))
}

fn master_key_file(data_dir: &Path) -> PathBuf {
    data_dir.join("auxcache.key")
}

/// Per-install 32-byte key (two UUIDv4s). Generated once; if generation or
/// read fails we refuse to serve L2 (silent degradation, L1 still works).
fn load_master_key(data_dir: &Path) -> Option<[u8; 32]> {
    let path = master_key_file(data_dir);
    if let Ok(bytes) = fs::read(&path) {
        if bytes.len() != 32 {
            return None; // foreign/corrupt key file — refuse L2, keep L1
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(&bytes);
        return Some(key);
    }
    let a: String = uuid::Uuid::new_v4().simple().to_string();
    let b: String = uuid::Uuid::new_v4().simple().to_string();
    let hexs = format!("{a}{b}");
    let mut key = [0u8; 32];
    for (i, slot) in key.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&hexs[i * 2..i * 2 + 2], 16).ok()?;
    }
    let _ = fs::create_dir_all(data_dir);
    fs::write(&path, key).ok()?;
    // the master key unseals every L2 entry — never world-readable on unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }
    Some(key)
}

/// SHA-256 counter-mode keystream XOR over (master key ‖ per-file nonce ‖
/// counter). The per-file nonce is what makes this sound: a fixed keystream
/// reused across files is a many-time-pad — passive possession of the cache
/// directory would XOR any two files to recover their plaintext difference.
/// Still at-rest encryption against casual disk scans / synced folders, not
/// against a process that can read the key file next to it.
fn keystream_xor(key: &[u8; 32], nonce: &[u8; 16], data: &mut [u8]) {
    let mut counter: u64 = 0;
    for chunk in data.chunks_mut(32) {
        let mut h = Sha256::new();
        h.update(key);
        h.update(nonce);
        h.update(counter.to_be_bytes());
        let ks = h.finalize();
        for (b, k) in chunk.iter_mut().zip(ks.iter()) {
            *b ^= *k;
        }
        counter += 1;
    }
}

/// Random per-file nonce (UUIDv4 is getrandom-backed).
fn random_nonce() -> [u8; 16] {
    uuid::Uuid::new_v4().into_bytes()
}

fn l2_put(data_dir: &Path, ns: &str, key: &str, e: &EntryMeta) -> std::io::Result<()> {
    let Some(mk) = load_master_key(data_dir) else {
        return Ok(()); // key unavailable → skip disk layer silently
    };
    let dir = l2_dir(data_dir, ns);
    fs::create_dir_all(&dir)?;
    let json = serde_json::to_vec(e).map_err(|e| std::io::Error::other(e))?;
    // layout: MAGIC ‖ nonce(16, plaintext) ‖ ciphertext
    let nonce = random_nonce();
    let mut payload: Vec<u8> = Vec::with_capacity(MAGIC.len() + 16 + json.len());
    payload.extend_from_slice(MAGIC);
    payload.extend_from_slice(&nonce);
    payload.extend_from_slice(&json);
    keystream_xor(&mk, &nonce, &mut payload[MAGIC.len() + 16..]);
    let path = key_file(data_dir, ns, key);
    let tmp = dir.join(format!(".{key}.tmp"));
    fs::write(&tmp, &payload)?;
    fs::rename(&tmp, &path)?;
    enforce_bound(&dir);
    Ok(())
}

fn l2_get(data_dir: &Path, ns: &str, key: &str) -> Option<EntryMeta> {
    let path = key_file(data_dir, ns, key);
    let mut payload = fs::read(&path).ok()?;
    if payload.len() < MAGIC.len() || &payload[..MAGIC.len()] != MAGIC {
        let _ = fs::remove_file(&path);
        return None;
    }
    let Some(mk) = load_master_key(data_dir) else {
        return None;
    };
    let body = payload.len() - MAGIC.len();
    // plaintext start: where the EntryMeta JSON lives after decryption
    let plaintext_at = if body >= 16 {
        // current format: per-file random nonce → unique keystream per file
        let mut nonce = [0u8; 16];
        nonce.copy_from_slice(&payload[MAGIC.len()..MAGIC.len() + 16]);
        keystream_xor(&mk, &nonce, &mut payload[MAGIC.len() + 16..]);
        MAGIC.len() + 16
    } else {
        // legacy pre-nonce format (zero nonce = the old fixed keystream);
        // real entries always exceed 16 bytes of body, so in practice they
        // fail the nonce parse above and are dropped, then rewritten with a
        // fresh nonce by the next put
        keystream_xor(&mk, &[0u8; 16], &mut payload[MAGIC.len()..]);
        MAGIC.len()
    };
    match serde_json::from_slice::<EntryMeta>(&payload[plaintext_at..]) {
        Ok(e) => Some(e),
        Err(_) => {
            // corrupt or foreign-format entry: delete, treat as miss
            let _ = fs::remove_file(&path);
            None
        }
    }
}

/// Keep one namespace directory under L2_DIR_MAX_BYTES; evict by oldest mtime.
fn enforce_bound(dir: &Path) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    let mut files: Vec<(PathBuf, SystemTime, u64)> = Vec::new();
    let mut total: u64 = 0;
    for entry in rd.flatten() {
        let Ok(md) = entry.metadata() else { continue };
        if !md.is_file() {
            continue;
        }
        let size = md.len();
        let mtime = md.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        total += size;
        files.push((entry.path(), mtime, size));
    }
    if total <= L2_DIR_MAX_BYTES {
        return;
    }
    files.sort_by_key(|(_, mtime, _)| *mtime);
    for (path, _, size) in files {
        if total <= L2_DIR_MAX_BYTES {
            break;
        }
        if fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(size);
        }
    }
}

// ---------- public get / put ----------

/// Lookup through L1 then L2. An L2 hit is promoted into L1.
pub fn get(data_dir: &Path, key: &str, ns: &str) -> Option<(EntryMeta, Origin)> {
    if let Some(e) = l1_get(key) {
        return Some((e, Origin::L1));
    }
    match l2_get(data_dir, ns, key) {
        Some(e) => {
            l1_put(key, e.clone());
            Some((e, Origin::L2))
        }
        None => None,
    }
}

/// Store into L1 + L2 (best-effort; failures only cost cache effect).
pub fn put(data_dir: &Path, key: &str, ns: &str, e: &EntryMeta) {
    l1_put(key, e.clone());
    let _ = l2_put(data_dir, ns, key, &e);
}

// ---------- ledger ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerRow {
    pub ts: u64,
    pub kind: String,
    pub model: String,
    /// "l1" | "l2" | "miss"
    pub origin: String,
    #[serde(default)]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: Option<u64>,
    #[serde(default)]
    pub billed_usd: Option<f64>,
    #[serde(default)]
    pub saved_usd: Option<f64>,
}

const LEDGER_MAX_ROWS: usize = 4000;

static LEDGER_LOCK: Mutex<()> = Mutex::new(());
static LEDGER_WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn ledger_path(data_dir: &Path) -> PathBuf {
    data_dir.join("aux-ledger.jsonl")
}

/// 台账无限追加会随时间撑大数据目录，stats() 又要整读文件 —— 超过
/// 行数上限时原子重写、保留最新的 rows（每 200 次写入检查一次，误差
/// 有界）。行下限约 120 字节，64B/行的估算门只做廉价的读前过滤。
fn trim_ledger(path: &Path) {
    let Ok(md) = fs::metadata(path) else { return };
    if md.len() < (LEDGER_MAX_ROWS as u64) * 64 {
        return;
    }
    let Ok(content) = fs::read_to_string(path) else { return };
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() <= LEDGER_MAX_ROWS {
        return;
    }
    let body = lines[lines.len() - LEDGER_MAX_ROWS..].join("
");
    let tmp = path.with_extension("jsonl.tmp");
    if fs::write(&tmp, body + "
").is_ok() {
        let _ = fs::rename(&tmp, path);
    }
}

fn ledger_append(data_dir: &Path, row: &LedgerRow) {
    let _guard = LEDGER_LOCK.lock().unwrap();
    let path = ledger_path(data_dir);
    let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    if let Ok(line) = serde_json::to_string(row) {
        use std::io::Write;
        let _ = writeln!(f, "{line}");
    }
    let n = LEDGER_WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if n % 200 == 0 {
        trim_ledger(&path);
    }
}

/// A cache hit served the call: billed 0, saved = recorded insert cost.
pub fn record_hit(data_dir: &Path, kind: &str, e: &EntryMeta, origin: Origin) {
    ledger_append(
        data_dir,
        &LedgerRow {
            ts: crate::sessions::now_ms(),
            kind: kind.to_string(),
            model: e.model.clone(),
            origin: origin.as_str().to_string(),
            input_tokens: e.in_tok,
            output_tokens: e.out_tok,
            billed_usd: Some(0.0),
            saved_usd: e.cost_usd,
        },
    );
}

/// The real call happened (miss): billed = actual cost, saved = 0.
pub fn record_miss(data_dir: &Path, kind: &str, model: &str, in_tok: Option<u64>, out_tok: Option<u64>, billed_usd: Option<f64>) {
    ledger_append(
        data_dir,
        &LedgerRow {
            ts: crate::sessions::now_ms(),
            kind: kind.to_string(),
            model: model.to_string(),
            origin: "miss".to_string(),
            input_tokens: in_tok,
            output_tokens: out_tok,
            billed_usd,
            saved_usd: Some(0.0),
        },
    );
}

// ---------- stats ----------

#[derive(Debug, Clone, Serialize)]
pub struct KindStat {
    pub kind: String,
    pub calls: u64,
    pub l1_hits: u64,
    pub l2_hits: u64,
    pub saved_usd: f64,
    pub billed_usd: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuxStats {
    pub kinds: Vec<KindStat>,
    /// newest first, capped
    pub recent: Vec<LedgerRow>,
}

/// Aggregate the ledger: per-kind counters + the last 50 rows (newest first).
pub fn stats(data_dir: &Path) -> AuxStats {
    let rows = read_ledger(data_dir, 5000);
    let mut by_kind: HashMap<String, KindStat> = HashMap::new();
    for r in &rows {
        let s = by_kind.entry(r.kind.clone()).or_insert(KindStat {
            kind: r.kind.clone(),
            calls: 0,
            l1_hits: 0,
            l2_hits: 0,
            saved_usd: 0.0,
            billed_usd: 0.0,
        });
        s.calls += 1;
        match r.origin.as_str() {
            "l1" => s.l1_hits += 1,
            "l2" => s.l2_hits += 1,
            _ => {}
        }
        s.saved_usd += r.saved_usd.unwrap_or(0.0);
        s.billed_usd += r.billed_usd.unwrap_or(0.0);
    }
    let mut kinds: Vec<KindStat> = by_kind.into_values().collect();
    kinds.sort_by(|a, b| a.kind.cmp(&b.kind));
    let recent: Vec<LedgerRow> = rows.iter().rev().take(50).cloned().collect();
    AuxStats { kinds, recent }
}

fn read_ledger(data_dir: &Path, cap: usize) -> Vec<LedgerRow> {
    let Ok(content) = fs::read_to_string(ledger_path(data_dir)) else {
        return Vec::new();
    };
    let mut rows: Vec<LedgerRow> = Vec::new();
    for line in content.lines().rev() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<LedgerRow>(line) {
            Ok(r) => rows.push(r),
            Err(_) => continue, // tolerate manual edits / partial writes
        }
        if rows.len() >= cap {
            break;
        }
    }
    rows.reverse(); // back to chronological order
    rows
}

// ---------- tests ----------

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ccharness-aux-{tag}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entry(text: &str) -> EntryMeta {
        EntryMeta { text: text.into(), model: "m".into(), in_tok: Some(10), out_tok: Some(5), cost_usd: Some(0.001) }
    }

    #[test]
    fn key_is_deterministic_and_sensitive() {
        let k = |model: &str, ep: &str, input: &str, ns: &str| compute_key("title", model, ep, input, ns);
        let a = k("m", "https://x.com/v1", "hello", "global");
        assert_eq!(a, k("m", "https://x.com/v1/", "hello", "global"), "trailing slash must normalize");
        assert_ne!(a, k("m2", "https://x.com/v1", "hello", "global"));
        assert_ne!(a, k("m", "https://y.com/v1", "hello", "global"));
        assert_ne!(a, k("m", "https://x.com/v1", "hello ", "global"));
        assert_ne!(a, k("m", "https://x.com/v1", "hello", "abc123"));
        assert_ne!(compute_key("title", "m", "e", "i", "g"), compute_key("enhance", "m", "e", "i", "g"));
    }

    #[test]
    fn namespace_isolates_workspaces() {
        assert_eq!(namespace(None), "global");
        assert_eq!(namespace(Some("  ")), "global");
        let a = namespace(Some("C:\\work\\a"));
        let b = namespace(Some("C:\\work\\b"));
        assert_ne!(a, b);
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn l1_lru_evicts_oldest() {
        l1_clear();
        let d = tmpdir("lru");
        for i in 0..(L1_MAX_ENTRIES + 8) {
            l1_put(&format!("k{i}"), entry(&format!("v{i}")));
        }
        assert!(l1_get("k0").is_none(), "oldest must be evicted");
        assert!(l1_get(&format!("k{}", L1_MAX_ENTRIES + 7)).is_some(), "newest must survive");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn l2_roundtrip_and_l1_restart_promotion() {
        l1_clear();
        let d = tmpdir("l2");
        let key = compute_key("enhance", "m", "https://x.com", "draft text", "global");
        put(&d, &key, "global", &entry("增强结果"));
        l1_clear(); // simulate restart
        let (e, origin) = get(&d, &key, "global").expect("L2 must serve after restart");
        assert_eq!(origin, Origin::L2);
        assert_eq!(e.text, "增强结果");
        // promoted: a second get hits L1
        let (_, o2) = get(&d, &key, "global").unwrap();
        assert_eq!(o2, Origin::L1);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn l2_files_are_not_plaintext() {
        l1_clear();
        let d = tmpdir("enc");
        let key = compute_key("title", "m", "https://x.com", "很长的用户消息", "global");
        put(&d, &key, "global", &entry("这是一个绝不应出现在磁盘明文里的标题"));
        let raw = fs::read(key_file(&d, "global", &key)).unwrap();
        let needle = "绝不应出现在磁盘明文里".as_bytes();
        assert!(!raw.windows(needle.len()).any(|w| w == needle), "disk bytes must not contain plaintext");
        assert!(raw.starts_with(MAGIC));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn ns_isolation_blocks_cross_workspace_hits() {
        l1_clear();
        let d = tmpdir("ns");
        let ns_a = namespace(Some("C:\\ws-a"));
        let ns_b = namespace(Some("C:\\ws-b"));
        let key_a = compute_key("enhance", "m", "e", "same input", &ns_a);
        let key_b = compute_key("enhance", "m", "e", "same input", &ns_b);
        put(&d, &key_a, &ns_a, &entry("来自 A"));
        assert!(get(&d, &key_b, &ns_b).is_none(), "B namespace must not see A's entry");
        assert!(get(&d, &key_a, &ns_a).is_some());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn ledger_layers_hit_and_miss() {
        l1_clear();
        let d = tmpdir("ledger");
        // a miss with a real billed cost
        record_miss(&d, "enhance", "m", Some(120), Some(80), Some(0.002));
        // then a hit
        let e = entry("结果");
        record_hit(&d, "enhance", &e, Origin::L2);
        let s = stats(&d);
        assert_eq!(s.kinds.len(), 1);
        let k = &s.kinds[0];
        assert_eq!(k.calls, 2);
        assert_eq!(k.l2_hits, 1);
        assert!((k.billed_usd - 0.002).abs() < 1e-9);
        assert!((k.saved_usd - 0.001).abs() < 1e-9);
        // hit row: billed 0, saved = insert cost
        let hit = s.recent.iter().find(|r| r.origin == "l2").unwrap();
        assert_eq!(hit.billed_usd, Some(0.0));
        assert!(hit.saved_usd.unwrap_or(0.0) > 0.0);
        // miss row: billed > 0
        let miss = s.recent.iter().find(|r| r.origin == "miss").unwrap();
        assert!(miss.billed_usd.unwrap_or(0.0) > 0.0);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn corrupt_l2_entry_is_deleted_and_misses() {
        l1_clear();
        let d = tmpdir("corrupt");
        let key = compute_key("title", "m", "e", "i", "global");
        put(&d, &key, "global", &entry("好标题"));
        // flip bytes after the magic
        let path = key_file(&d, "global", &key);
        let mut raw = fs::read(&path).unwrap();
        for b in raw.iter_mut().skip(MAGIC.len()) {
            *b = b.wrapping_add(1);
        }
        fs::write(&path, &raw).unwrap();
        l1_clear();
        assert!(get(&d, &key, "global").is_none());
        assert!(!path.exists(), "corrupt entry must be removed");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn ledger_is_trimmed_to_row_cap() {
        l1_clear();
        let d = tmpdir("trim");
        for _ in 0..(LEDGER_MAX_ROWS + 300) {
            record_miss(&d, "title", "m", Some(1), Some(1), None);
        }
        let s = stats(&d);
        assert_eq!(s.kinds.len(), 1);
        // 每 200 次写入触发一次裁剪：最终行数落在 [MAX_ROWS, MAX_ROWS+200)
        let calls = s.kinds[0].calls;
        assert!(
            calls >= LEDGER_MAX_ROWS as u64 && calls < (LEDGER_MAX_ROWS + 250) as u64,
            "ledger must stay bounded, got {calls}"
        );
        assert_eq!(s.recent.len(), 50, "recent view stays capped");
        let _ = fs::remove_dir_all(&d);
    }
}
