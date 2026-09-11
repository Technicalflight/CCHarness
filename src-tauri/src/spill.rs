//! Context-volume control: oversized tool results spill to disk.
//!
//! When a tool result exceeds the configured character budget, the full
//! output is written once to `<data_dir>/spills/<session_id>/<sha256-16>.txt`
//! and the string that enters the model context (the tool record, and from
//! there Zone H and every replay) becomes a bounded head + locator marker +
//! bounded tail. The trim is a pure function of the content, so live wire
//! bytes and restart rebuilds stay byte-identical — the prefix-cache
//! discipline is untouched. Writes are fail-safe: any IO error returns the
//! original result unchanged (oversized but correct beats truncated).

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Characters kept from the head of a spilled result.
pub const SPILL_HEAD_CHARS: usize = 8_000;
/// Characters kept from the tail of a spilled result.
pub const SPILL_TAIL_CHARS: usize = 2_000;
/// Default per-result budget (chars) before spilling kicks in; ~6k tokens
/// of mixed-script content. 0 disables spilling entirely.
pub const DEFAULT_SPILL_MAX_CHARS: usize = 24_000;

/// Marker prefix — also the idempotence guard: content that already carries
/// a spill marker is never re-trimmed (a lowered threshold would otherwise
/// nest markers and corrupt the head/tail shape).
const MARKER_PREFIX: &str = "[工具输出过大：";

/// Read-only allowlist root for agent_tools::read_file — set once at boot.
/// The dispatch layer has no data_dir, so the allowlist is resolved through
/// this global instead (same pattern as the sandbox policy).
static SPILL_ROOT: OnceLock<PathBuf> = OnceLock::new();

pub fn init_root(data_dir: &Path) {
    let _ = SPILL_ROOT.set(data_dir.join("spills"));
}

fn spill_root() -> Option<&'static Path> {
    SPILL_ROOT.get().map(|p| p.as_path())
}

/// Spill directory for one session: `<root>/<session_id>`.
fn session_dir(root: &Path, session_id: &str) -> PathBuf {
    let safe: String = session_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    let safe = if safe.is_empty() { "unknown".to_string() } else { safe };
    root.join(safe)
}

/// Resolve an absolute path to a spill file for the read_file allowlist.
/// Only `<root>/<session>/<16-hex>.txt` shapes qualify — anything else
/// (relative paths, other extensions, traversal attempts) is rejected so the
/// allowlist can never widen into the rest of the data dir.
pub fn resolve_spill_file(rel: &str) -> Option<PathBuf> {
    let root = spill_root()?;
    let p = Path::new(rel.trim());
    if !p.is_absolute() {
        return None;
    }
    let name_ok = p
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.len() == 20 && n.ends_with(".txt") && n[..16].bytes().all(|b| b.is_ascii_hexdigit()));
    let under_root = p.starts_with(root) && p.parent().and_then(|d| d.parent()) == Some(root);
    (name_ok && under_root).then(|| p.to_path_buf())
}

/// Convenience wrapper using the boot-initialized root. Sites that already
/// hold `data_dir` should prefer [`maybe_spill_in`] to stay testable.
pub fn maybe_spill(session_id: &str, tool_name: &str, result: &str, max_chars: usize) -> String {
    match spill_root() {
        Some(root) => maybe_spill_in(root, session_id, tool_name, result, max_chars),
        None => result.to_string(),
    }
}

/// Trim `result` when it exceeds `max_chars`, spilling the full text under
/// `root`. Returns the original string when: spilling is off (0), the budget
/// is not hit, the content already carries a spill marker, or any write
/// fails (fail-safe). Deterministic: same inputs → same output bytes.
pub fn maybe_spill_in(
    root: &Path,
    session_id: &str,
    tool_name: &str,
    result: &str,
    max_chars: usize,
) -> String {
    if max_chars == 0 || result.contains(MARKER_PREFIX) {
        return result.to_string();
    }
    let total = result.chars().count();
    if total <= max_chars {
        return result.to_string();
    }
    let dir = session_dir(root, session_id);
    let hash = hex::encode(&Sha256::digest(result.as_bytes())[..8]);
    let path = dir.join(format!("{hash}.txt"));
    // write-once per content hash; a concurrent duplicate write lands on the
    // same bytes, and an existing file is kept as-is
    if std::fs::create_dir_all(&dir).is_err() {
        return result.to_string();
    }
    if !path.exists() && std::fs::write(&path, result).is_err() {
        return result.to_string();
    }
    format_trimmed(result, tool_name, &path.to_string_lossy())
}

/// The trimmed context form: head + locator marker + tail. Pure function.
fn format_trimmed(result: &str, tool_name: &str, path: &str) -> String {
    let total = result.chars().count();
    let head: String = result.chars().take(SPILL_HEAD_CHARS).collect();
    let tail: String = result.chars().skip(total - SPILL_TAIL_CHARS).collect();
    let cut = total - SPILL_HEAD_CHARS - SPILL_TAIL_CHARS;
    format!(
        "{head}\n\n{MARKER_PREFIX}{tool_name} 共 {total} 字符，中段 {cut} 字符已修剪；完整输出已保存到 {path}，需要时可用 read_file 读取该绝对路径]\n\n{tail}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ccharness-spill-test-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn big(n: usize) -> String {
        (0..n).map(|i| char::from_u32(0x4e00 + (i % 3000) as u32).unwrap()).collect()
    }

    #[test]
    fn under_threshold_untouched() {
        let root = temp_root("under");
        let s = big(100);
        assert_eq!(maybe_spill_in(&root, "sid", "run_command", &s, 24_000), s);
        assert!(spill_root().is_none() || !root.join("spills").exists());
    }

    #[test]
    fn zero_disables() {
        let root = temp_root("zero");
        let s = big(50_000);
        assert_eq!(maybe_spill_in(&root, "sid", "run_command", &s, 0), s);
    }

    #[test]
    fn over_threshold_trims_and_writes_full() {
        let root = temp_root("over");
        let s = big(30_000);
        let out = maybe_spill_in(&root, "sid", "run_command", &s, 24_000);
        assert!(out.contains(MARKER_PREFIX));
        assert!(out.contains("共 30000 字符"));
        assert!(out.contains("中段 20000 字符已修剪"));
        // head and tail preserved byte-exact
        let head: String = s.chars().take(SPILL_HEAD_CHARS).collect();
        let tail: String = s.chars().skip(30_000 - SPILL_TAIL_CHARS).collect();
        assert!(out.starts_with(&head));
        assert!(out.ends_with(&tail));
        // spilled file holds the complete original
        let marker_line = out.lines().find(|l| l.contains(MARKER_PREFIX)).unwrap();
        let file_part = marker_line.split("保存到 ").nth(1).unwrap();
        let file_path = file_part.split("，需要时").next().unwrap();
        let stored = std::fs::read_to_string(file_path).unwrap();
        assert_eq!(stored, s);
    }

    #[test]
    fn deterministic_same_content_same_bytes() {
        let root = temp_root("deterministic");
        let s = big(30_000);
        let a = maybe_spill_in(&root, "sid", "run_command", &s, 24_000);
        let b = maybe_spill_in(&root, "sid", "run_command", &s, 24_000);
        assert_eq!(a, b);
        // one file per content, not per call
        let dir = session_dir(&root, "sid");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    }

    #[test]
    fn idempotent_marker_never_nests() {
        let root = temp_root("idempotent");
        let s = big(30_000);
        let once = maybe_spill_in(&root, "sid", "run_command", &s, 24_000);
        let twice = maybe_spill_in(&root, "sid", "run_command", &once, 100);
        assert_eq!(once, twice);
    }

    #[test]
    fn write_failure_falls_back_to_original() {
        // root path collides with a regular file → create_dir_all fails
        let base = temp_root("fail");
        let root = base.join("blocked");
        std::fs::write(&root, "not a dir").unwrap();
        let s = big(30_000);
        assert_eq!(maybe_spill_in(&root, "sid", "run_command", &s, 24_000), s);
    }

    #[test]
    fn unsafe_session_ids_sanitized() {
        let root = temp_root("sanitize");
        let s = big(30_000);
        let out = maybe_spill_in(&root, "../../etc", "run_command", &s, 24_000);
        assert!(out.contains(MARKER_PREFIX));
        // no traversal escape: everything stays under the root
        let entries: Vec<_> = std::fs::read_dir(&root).unwrap().flatten().collect();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn resolve_spill_file_shape_gated() {
        let root = temp_root("resolve");
        let _ = SPILL_ROOT.set(root.clone());
        let valid = root.join("abc").join("0123456789abcdef.txt");
        assert_eq!(resolve_spill_file(valid.to_str().unwrap()), Some(valid.clone()));
        // wrong extension / non-hex name / outside root / relative
        assert_eq!(resolve_spill_file(root.join("abc").join("0123456789abcdef.exe").to_str().unwrap()), None);
        assert_eq!(resolve_spill_file(root.join("abc").join("zzzzzzzzzzzzzzzz.txt").to_str().unwrap()), None);
        assert_eq!(resolve_spill_file(root.parent().unwrap().join("x.txt").to_str().unwrap()), None);
        assert_eq!(resolve_spill_file("relative/path.txt"), None);
    }
}
