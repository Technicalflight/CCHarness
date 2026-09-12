// Compaction engine: the L6 ladder that keeps long sessions inside
// the context window — token estimation, summarize-and-fold (manual
// + auto), free rungs (prune oversized / elide stale tool outputs),
// payback economics and the rolling session memo. Split out of
// commands.rs (P2-13).

use crate::chat;
use crate::commands::{next_record_ts, prefixes_lock, AppState, warn_save};
use crate::config::{self, AppConfig, Provider};
use crate::prefix::ChatMessage;
use crate::send_engine::resolve_provider;
use crate::sessions::{now_ms, SessionStore};
use crate::types_rs::MessageRecord;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::State;

/// Auto-compaction threshold: compact at a user boundary once the last
/// request's input tokens reached this fraction of the context window.
const COMPACT_AT_FRACTION: f64 = 0.7;

/// Hysteresis target: free rungs (prune / elision) count as "rescued" only
/// when they push the projected input back BELOW this fraction — landing
/// between the two lines prevents immediate re-triggering (0.70 trigger →
/// 0.55 target leaves a 15% headroom band).
const COMPACT_TARGET_FRACTION: f64 = 0.55;

/// Don't bother compacting tiny conversations.
const COMPACT_MIN_MESSAGES: usize = 8;

/// Character budget fed to the summarizer.
const SUMMARIZE_INPUT_CAP: usize = 24_000;

/// Build the text fed to the summarizer: previous summary (if any) plus a
/// bounded excerpt of the transcript.
fn summarize_input(
    sf: &crate::sessions::SessionFile,
    keep_from_ts: u64,
    memo: Option<&str>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(m) = memo {
        parts.push(format!("[当前会话备忘]\n{m}"));
    }
    if let Some(prev) = &sf.compaction {
        parts.push(format!("[上次摘要]\n{}", prev.summary));
    }
    let upto = sf.compaction.as_ref().map(|c| c.upto_ts).unwrap_or(0);
    for m in &sf.messages {
        // fold range: after the previous boundary, strictly before KEEP
        if m.ts < upto || m.ts >= keep_from_ts || m.status == "error" {
            continue;
        }
        let role = match m.role.as_str() {
            "user" => "用户",
            "assistant" => "助手",
            "tool" => "工具",
            _ => continue,
        };
        let head: String = m.content.chars().take(1200).collect();
        parts.push(format!("{role}: {head}"));
    }
    let mut text = parts.join("\n\n");
    if text.chars().count() > SUMMARIZE_INPUT_CAP {
        text = text.chars().take(SUMMARIZE_INPUT_CAP).collect::<String>() + "\n…[已截断]";
    }
    text
}

/// Structured-summary prompt (L6 §5.2): the output shape mirrors
/// RollingMemo so the summary can be re-consumed by the same machinery,
/// and `CompactionRecord` needs no format migration.
const COMPACT_PROMPT: &str = "你是对话摘要器。把输入的历史压缩为信息密集的 YAML 会话备忘（全部内容合计不超过 1200 字符），字段固定：goal（当前目标，一行）、decisions（已确定的决定，每条一行）、files（涉及文件路径及其最后动作）、errors_fixed（已修复的错误）、open_items（未尽事项）。必须保留具体文件路径与数值。只输出 YAML 本身，不要代码块围栏。";

/// Mixed-script token estimate shared by every compaction accounting site.
/// The old flat `bytes/4` underestimated CJK 3-4× (a Chinese char is 3
/// UTF-8 bytes but ≈1 token): ASCII runs at ~4 chars/token, everything
/// else (CJK, cyrillic, emoji…) at ~1.25 chars/token. KEEP walks use the
/// same function so budgets, payback and hysteresis stay in sync (P2).
pub fn est_tokens(content: &str) -> u64 {
    let mut ascii = 0u64;
    let mut wide = 0u64;
    for ch in content.chars() {
        if (ch as u32) < 0x80 {
            ascii += 1;
        } else {
            wide += 1;
        }
    }
    // ≈ ascii/4 + wide/1.25, no floats
    ascii / 4 + (wide * 4 + 2) / 5
}

/// Per-record overhead in the same token unit (role/id/ts JSON frame).
const RECORD_OVERHEAD_TOKENS: u64 = 24;

/// KEEP-bucket boundary (L6 §5.1): walk user turns newest-first and keep
/// including whole turns while the accumulated estimate fits the budget;
/// always keep at least the newest turn. Returns the ts such that every
/// record with ts ≥ it belongs to KEEP (accounting matches the request
/// estimator: est_tokens(content) + fixed overhead).
fn compute_keep_from_ts(messages: &[MessageRecord], keep_budget_tokens: usize) -> u64 {
    let starts: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == "user")
        .map(|(i, _)| i)
        .collect();
    if starts.is_empty() {
        return messages.first().map(|m| m.ts).unwrap_or(0);
    }
    let mut acc = 0usize;
    let mut kept = 0usize;
    let mut boundary = messages.last().map(|m| m.ts).unwrap_or(0);
    for n in (0..starts.len()).rev() {
        let start = starts[n];
        let end = starts.get(n + 1).copied().unwrap_or(messages.len());
        let span: usize = messages[start..end]
            .iter()
            .map(|m| (est_tokens(&m.content) + RECORD_OVERHEAD_TOKENS) as usize)
            .sum();
        if kept > 0 && acc + span > keep_budget_tokens {
            break;
        }
        acc += span;
        kept += 1;
        boundary = messages[start].ts;
    }
    boundary
}

/// Stable hash of the tool-schema loadout sent in the request head. Fed to
/// LanePrefix::bind_tools_hash so MCP join/leave mid-session is honest about
/// the epoch rebuild it causes (tools serialize before messages).
/// SHA-256-derived: the old DefaultHasher output is not stable across rustc
/// releases, which would silently falsify every recorded tools_hash after a
/// toolchain upgrade.
pub(crate) fn tools_hash(tools: Option<&Value>) -> Option<u64> {
    tools.map(|t| {
        use sha2::Digest;
        let digest = sha2::Sha256::digest(t.to_string().as_bytes());
        let mut b = [0u8; 8];
        b.copy_from_slice(&digest[..8]);
        u64::from_be_bytes(b)
    })
}

/// Run boundary compaction for a chat session: summarize everything up to
/// now, persist the record, invalidate the lane prefix (next request
/// rebuilds from the compacted transcript — an expected epoch bump).
async fn compact_now(
    state: &AppState,
    client: &reqwest::Client,
    data_dir: &std::path::Path,
    save_lock: &tokio::sync::Mutex<()>,
    session_id: &str,
    provider: &Provider,
    model: &str,
    trigger: &str,
) -> Result<(), String> {
    let store = SessionStore::new(data_dir);
    let sf0 = store.load(session_id)?;
    if sf0.messages.len() < COMPACT_MIN_MESSAGES {
        return Err(format!("对话过短（{} 条），无需压缩", sf0.messages.len()));
    }
    let window = provider.context_window.unwrap_or(131_072) as f64;
    // KEEP bucket (L6 §5.1): recent user turns within window × 20%, always
    // at least the newest turn — records from keep_from_ts on stay verbatim.
    let keep_budget = (window * 0.20) as usize; // token budget (est_tokens unit)
    let keep_from_ts = compute_keep_from_ts(&sf0.messages, keep_budget);
    // DROP rung before folding (idempotent): stale oversized outputs →
    // stubs, so the summarizer reads a smaller input. The load-modify-save
    // inside runs under save_lock — a lane save landing mid-rewrite would
    // otherwise be rolled back by the full-file write.
    let cfg = config::load(data_dir);
    let (dropped, stubs) = {
        let _guard = save_lock.lock().await;
        elide_stale_tool_records(&state, data_dir, session_id, cfg.settings.spill_max_chars, keep_from_ts)
    };
    let sf = store.load(session_id)?;
    let prev_upto = sf.compaction.as_ref().map(|c| c.upto_ts).unwrap_or(0);
    let foldable: Vec<&MessageRecord> = sf
        .messages
        .iter()
        .filter(|m| m.ts >= prev_upto && m.ts < keep_from_ts)
        .filter(|m| matches!(m.role.as_str(), "user" | "assistant" | "tool"))
        .collect();
    // nothing to fold → the elision rung was the whole job
    if foldable.is_empty() {
        return Ok(());
    }
    let memo_render = sf.meta.rolling_memo.as_ref().and_then(render_memo);
    let memo_chars = memo_render.as_ref().map(|t| t.chars().count()).unwrap_or(0);
    let input = summarize_input(&sf, keep_from_ts, memo_render.as_deref());
    let outcome = chat::complete_once(client, provider, model, COMPACT_PROMPT, &input).await?;
    let summary: String = outcome.text.trim().chars().take(1600).collect();
    let record = crate::types_rs::CompactionRecord {
        summary,
        upto_ts: keep_from_ts,
        created_at: now_ms(),
    };
    // third ledger (L6 §6.1): what fired, what it folded, what it saved
    let folded_tokens =
        foldable.iter().map(|m| est_tokens(&m.content) + RECORD_OVERHEAD_TOKENS).sum::<u64>();
    let completed_turns = sf.messages.iter().filter(|m| m.role == "user").count() as u64;
    let epoch_before = prefixes_lock(&state)
        .as_ref()
        .and_then(|m| m.get(&(session_id.to_string(), 0)))
        .map(|lp| lp.epoch)
        .unwrap_or(0);
    let stat = crate::types_rs::CompactionStat {
        ts: now_ms(),
        trigger: trigger.to_string(),
        folded_tokens,
        dropped_tokens: dropped as u64, // free rungs already account in tokens
        stubs,
        summary_tokens: 800,
        memo_chars,
        payback_turns: provider
            .pricing
            .get(model)
            .and_then(|pr| payback_turns(folded_tokens, 800, pr)),
        completed_turns,
        epoch_before,
    };
    // final apply is a short critical section: reload under save_lock so a
    // lane save that landed while the summary was in flight is preserved —
    // the long-running summary call itself deliberately stays outside
    {
        let _guard = save_lock.lock().await;
        let mut sf = store.load(session_id)?;
        sf.compaction = Some(record);
        sf.meta.compactions.push(stat);
        if sf.meta.compactions.len() > 60 {
            let overflow = sf.meta.compactions.len() - 60;
            sf.meta.compactions.drain(..overflow);
        }
        sf.meta.updated_at = now_ms();
        store.save(&sf)?;
    }
    if let Some(map) = prefixes_lock(&state).as_mut() {
        map.remove(&(session_id.to_string(), 0));
    }
    if let Some(map) = state.last_span.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
        map.remove(&(session_id.to_string(), 0));
    }
    Ok(())
}

#[tauri::command]
pub async fn compact_session(state: State<'_, AppState>, session_id: String) -> Result<String, String> {
    let cfg = config::load(&state.data_dir);
    let sf = state.store.load(&session_id)?;
    if sf.meta.kind != "chat" {
        return Err("竞技场会话暂不支持压缩".into());
    }
    let binding = sf.meta.bindings.first().cloned().ok_or("会话未绑定模型")?;
    let provider = resolve_provider(&cfg, &binding).cloned().ok_or("Provider 未配置")?;
    compact_now(&state, &state.client, &state.data_dir, &state.save_lock, &session_id, &provider, &binding.model, "manual").await?;
    Ok(sf.compaction.as_ref().map(|_| "已在旧摘要基础上再次压缩".to_string()).unwrap_or_else(|| "已压缩".into()))
}

#[tauri::command]
pub fn get_session_compaction(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<Option<crate::types_rs::CompactionRecord>, String> {
    Ok(state.store.load(&session_id)?.compaction)
}

/// Pre-compaction break-even estimate (pi pruning economics): the summary
/// re-bills at the full input price once (the cache-write premium), while
/// every later turn saves re-reading the folded tokens from cache. Sync —
/// no model call, numbers come from byte counts + configured pricing.
#[tauri::command]
pub fn compact_estimate(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<crate::types_rs::CompactEstimate, String> {
    let cfg = config::load(&state.data_dir);
    let sf = state.store.load(&session_id)?;
    if sf.meta.kind != "chat" {
        return Err("竞技场会话暂不支持压缩".into());
    }
    let binding = sf.meta.bindings.first().cloned().ok_or("会话未绑定模型")?;
    let provider = resolve_provider(&cfg, &binding).cloned().ok_or("Provider 未配置")?;
    let model = binding.model.clone();

    // folded = what compaction replaces: every record that would go over
    // the wire now (same mixed-script estimator as the KEEP walk and the
    // auto gate, so UI numbers and gate decisions never drift apart)
    let folded_tokens: u64 = sf
        .messages
        .iter()
        .filter(|m| m.role == "user" || m.role == "assistant" || m.role == "tool")
        .map(|m| est_tokens(&m.content) + RECORD_OVERHEAD_TOKENS)
        .sum();
    let summary_tokens: u64 = 800; // 1200-char YAML cap ≈ 800–1000 CJK tokens

    let pricing = provider.pricing.get(&model);
    let (rewrite_cost_usd, save_per_turn_usd, payback) = match pricing {
        Some(pr) => {
            let spread = (pr.input_per_m - pr.cached_per_m).max(0.0);
            let rewrite = summary_tokens as f64 / 1e6 * spread;
            let save = folded_tokens as f64 / 1e6 * pr.cached_per_m;
            (
                Some((rewrite * 10000.0).round() / 10000.0),
                Some((save * 10000.0).round() / 10000.0),
                payback_turns(folded_tokens, summary_tokens, pr),
            )
        }
        None => (None, None, None),
    };
    Ok(crate::types_rs::CompactEstimate {
        folded_tokens,
        summary_tokens,
        rewrite_cost_usd,
        save_per_turn_usd,
        payback_turns: payback,
    })
}

/// Render budget for the memo block (L6 §4.1).
const MEMO_CAP_CHARS: usize = 1200;

/// User-correction heuristics for the decisions bucket (§4.2). Deliberately
/// narrow: only file/number facts are recorded, never guessed semantics.
const MEMO_CORRECTION_KEYS: [&str; 6] = ["不对", "改成", "还是用", "换成", "不是这个", "回退"];

/// Compaction ladder L1 (free pruning): rewrite every tool record exceeding
/// `max_chars` through the spill trim — full output to the spill dir, bounded
/// head/tail + locator into the record. Persisted once; deterministic and
/// idempotent (marker guard), so Zone H replay and restart rebuilds stay
/// byte-identical. Returns the recovered character count (0 = nothing pruned
/// or the save failed).
/// Best-effort target extraction from a tool call's arguments JSON: the
/// first present string among common path/url/command keys (capped). Empty
/// when the tool has no meaningful target — the record then never elides.
fn extract_tool_target(arguments: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return String::new();
    };
    for key in ["path", "file_path", "url", "target", "command"] {
        if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
            return s.chars().take(120).collect();
        }
    }
    String::new()
}

/// Rule-based memo update from one finished turn (L6 §4.2): file targets
/// (last action wins), user corrections, and error→fix pairs within the
/// turn. Pure function over the wire messages; returns whether anything
/// changed. Idempotent on replay — the same turn applied twice is a no-op.
pub(crate) fn rolling_memo_apply(memo: &mut crate::types_rs::RollingMemo, turn: &[ChatMessage]) -> bool {
    let mut changed = false;
    if let Some(user) = turn.iter().find(|m| m.role == "user") {
        let text = user.content.trim();
        if text.chars().count() > 4 && MEMO_CORRECTION_KEYS.iter().any(|k| text.contains(k)) {
            let excerpt: String = text.chars().take(80).collect();
            if !memo.decisions.iter().any(|d| d == &excerpt) {
                memo.decisions.push(excerpt);
                if memo.decisions.len() > 12 {
                    memo.decisions.remove(0);
                }
                changed = true;
            }
        }
    }
    // collect (target, tool, errored) in call order, joining results by id
    let mut seq: Vec<(String, String, bool)> = Vec::new();
    for m in turn.iter() {
        let Some(calls) = m.tool_calls.as_ref().and_then(|v| v.as_array()) else {
            continue;
        };
        for call in calls {
            let id = call.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let fname =
                call.get("function").and_then(|f| f.get("name")).and_then(|v| v.as_str()).unwrap_or("");
            let args = call
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if id.is_empty() || fname.is_empty() {
                continue;
            }
            let Some(result) = turn
                .iter()
                .find(|r| r.role == "tool" && r.tool_call_id.as_deref() == Some(id))
            else {
                continue;
            };
            let errored = result.content.starts_with("ERROR");
            seq.push((extract_tool_target(args), fname.to_string(), errored));
        }
    }
    // last action per target wins
    for (target, fname, errored) in &seq {
        if target.is_empty() {
            continue;
        }
        let entry = format!("{fname}{}", if *errored { "（失败）" } else { "" });
        if memo.files.get(target).map(|v| v.as_str()) != Some(entry.as_str()) {
            memo.files.insert(target.clone(), entry);
            changed = true;
        }
    }
    // error→fix: last result for a target is ok, an earlier one errored
    let mut counted: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for i in (0..seq.len()).rev() {
        let (target, fname, errored) = &seq[i];
        if *errored || target.is_empty() || !counted.insert(target.clone()) {
            continue;
        }
        if seq[..i].iter().any(|(t2, _, e2)| t2 == target && *e2) {
            let item = format!("{target}（{fname}）");
            if !memo.errors_fixed.iter().any(|x| x == &item) {
                memo.errors_fixed.push(item);
                if memo.errors_fixed.len() > 8 {
                    memo.errors_fixed.remove(0);
                }
                changed = true;
            }
        }
    }
    changed
}

/// Deterministic memo render (L6 §4.1): fixed section order, BTreeMap file
/// ordering, hard char cap with a truncation marker. None when nothing has
/// been extracted yet.
pub(crate) fn render_memo(memo: &crate::types_rs::RollingMemo) -> Option<String> {
    let has_goal = memo.goal.as_deref().is_some_and(|g| !g.trim().is_empty());
    if !has_goal
        && memo.decisions.is_empty()
        && memo.files.is_empty()
        && memo.errors_fixed.is_empty()
        && memo.open_items.is_empty()
    {
        return None;
    }
    let mut out = String::from("[会话备忘·自动维护]\n");
    if let Some(goal) = memo.goal.as_deref().filter(|g| !g.trim().is_empty()) {
        out.push_str(&format!("goal: {goal}\n"));
    }
    let sections: Vec<(&str, Vec<String>)> = vec![
        ("decisions", memo.decisions.clone()),
        (
            "files",
            memo.files.iter().map(|(k, v)| format!("{k} ← {v}")).collect(),
        ),
        ("errors_fixed", memo.errors_fixed.clone()),
        ("open_items", memo.open_items.clone()),
    ];
    for (title, items) in sections {
        if items.is_empty() {
            continue;
        }
        let mut block = format!("{title}:\n");
        for it in items {
            let line = format!("- {it}\n");
            if out.chars().count() + block.chars().count() + line.chars().count() > MEMO_CAP_CHARS
            {
                block.push_str("- …[截断]\n");
                break;
            }
            block.push_str(&line);
        }
        out.push_str(&block);
    }
    Some(out)
}

fn prune_oversized_tool_records(
    state: &AppState,
    data_dir: &std::path::Path,
    session_id: &str,
    max_chars: usize,
) -> usize {
    if max_chars == 0 {
        return 0;
    }
    let store = SessionStore::new(data_dir);
    let Ok(mut sf) = store.load(session_id) else {
        return 0;
    };
    let mut saved = 0usize;
    let mut changed = false;
    for m in sf.messages.iter_mut().filter(|m| m.role == "tool") {
        if m.content.chars().count() <= max_chars {
            continue;
        }
        let trimmed = crate::spill::maybe_spill_in(
            &data_dir.join("spills"),
            session_id,
            "tool",
            &m.content,
            max_chars,
        );
        // accounting in the shared token unit so the projected input drop
        // matches what the request estimator would see
        let before = est_tokens(&m.content);
        let after = est_tokens(&trimmed);
        if after < before {
            saved += (before - after) as usize;
            m.content = trimmed;
            changed = true;
        }
    }
    if !changed {
        return 0;
    }
    sf.meta.updated_at = now_ms();
    if store.save(&sf).is_err() {
        return 0;
    }
    // expected rebuild: the next request re-creates the prefix from the
    // smaller transcript instead of judging the byte change an upstream miss
    if let Some(map) = prefixes_lock(&state).as_mut() {
        map.remove(&(session_id.to_string(), 0));
    }
    if let Some(map) = state.last_span.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
        map.remove(&(session_id.to_string(), 0));
    }
    saved
}

/// Free rung 2 (L6 §3): downgrade stale oversized tool outputs to one-line
/// stubs. A tool record is elidable when it is big, older than the recent
/// window (`keep_from_ts`), and SUPERSEDED — a later tool record for the
/// same (tool, target) exists, so the stub loses nothing current. Same
/// materialized-rewrite pattern as `prune_oversized_tool_records`: the
/// persisted record is rewritten once, the live prefix is dropped, and
/// restart rebuilds read the rewritten bytes. The stub keeps role=tool +
/// tool_call_id, so call/result pairing survives (design invariant I1).
/// Returns (saved tokens, stubs written); (0, 0) = nothing changed.
fn elide_stale_tool_records(
    state: &AppState,
    data_dir: &std::path::Path,
    session_id: &str,
    max_chars: usize,
    keep_from_ts: u64,
) -> (usize, u32) {
    if max_chars == 0 {
        return (0, 0);
    }
    let store = SessionStore::new(data_dir);
    let Ok(mut sf) = store.load(session_id) else {
        return (0, 0);
    };
    let min_chars = (max_chars / 2).max(1);
    // tool_call_id → (tool name, target) from the assistant batches
    let mut call_meta: HashMap<String, (String, String)> = HashMap::new();
    for m in &sf.messages {
        if let Some(calls) = &m.tool_calls {
            for c in calls {
                let target = extract_tool_target(&c.arguments);
                if !target.is_empty() {
                    call_meta.insert(c.id.clone(), (c.name.clone(), target));
                }
            }
        }
    }
    // newest ts per (tool, target): the latest read/write stays verbatim
    let mut latest: HashMap<(String, String), u64> = HashMap::new();
    for m in sf.messages.iter().filter(|m| m.role == "tool") {
        if let Some(id) = &m.tool_call_id {
            if let Some((name, target)) = call_meta.get(id) {
                let entry = latest.entry((name.clone(), target.clone())).or_insert(0);
                if m.ts > *entry {
                    *entry = m.ts;
                }
            }
        }
    }
    let mut saved = 0usize;
    let mut stubs = 0u32;
    let mut changed = false;
    for m in sf.messages.iter_mut() {
        if m.role != "tool" || m.ts >= keep_from_ts {
            continue;
        }
        if m.content.chars().count() <= min_chars {
            continue;
        }
        let Some(id) = &m.tool_call_id else { continue };
        let Some((name, target)) = call_meta.get(id) else { continue };
        match latest.get(&(name.clone(), target.clone())) {
            Some(latest_ts) if *latest_ts > m.ts => {}
            _ => continue,
        }
        let stub = format!(
            "[已降级] {name} {target} @ts={}。原输出 {} 字符已过时；如需当前内容请重新读取。",
            m.ts,
            m.content.chars().count()
        );
        // token-unit accounting, consistent with the prune rung above
        let before = est_tokens(&m.content);
        let after = est_tokens(&stub);
        if after < before {
            saved += (before - after) as usize;
            stubs += 1;
            m.content = stub;
            changed = true;
        }
    }
    if !changed {
        return (0, 0);
    }
    sf.meta.updated_at = now_ms();
    if store.save(&sf).is_err() {
        return (0, 0);
    }
    if let Some(map) = prefixes_lock(&state).as_mut() {
        map.remove(&(session_id.to_string(), 0));
    }
    if let Some(map) = state.last_span.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
        map.remove(&(session_id.to_string(), 0));
    }
    (saved, stubs)
}

/// Payback math shared by the manual estimate UI and the automatic gate
/// (single source so the two never drift): how many turns of cached-read
/// savings it takes to amortize one summary rewrite. None = economics
/// undefined (no savings because cached reads are free, or no spread).
fn payback_turns(
    folded_tokens: u64,
    summary_tokens: u64,
    pr: &crate::config::Pricing,
) -> Option<u64> {
    let spread = (pr.input_per_m - pr.cached_per_m).max(0.0);
    let rewrite = summary_tokens as f64 / 1e6 * spread;
    let save = folded_tokens as f64 / 1e6 * pr.cached_per_m;
    if save > 0.0 && rewrite > 0.0 {
        Some((rewrite / save).ceil() as u64)
    } else {
        None
    }
}

/// Automatic-compaction payback gate (L6 §2): a summary is only worth its
/// rewrite cost when the session is expected to live long enough to
/// amortize it. Remaining life heuristic: max(8, half of the completed
/// user turns) — fresh sessions are assumed short-lived, long sessions
/// have proven longevity. No pricing data ⇒ pressure-only (gate open);
/// cached reads free ⇒ folding saves nothing (gate closed).
fn auto_compact_payback_ok(
    pricing: Option<&crate::config::Pricing>,
    folded_tokens: u64,
    summary_tokens: u64,
    completed_turns: u64,
) -> bool {
    let Some(pr) = pricing else { return true };
    let save = folded_tokens as f64 / 1e6 * pr.cached_per_m;
    if save <= 0.0 {
        return false;
    }
    match payback_turns(folded_tokens, summary_tokens, pr) {
        // spread = 0 ⇒ the rewrite itself is free, folding always wins
        None => true,
        Some(payback) => payback <= (completed_turns / 2).max(8),
    }
}

/// Auto boundary compaction: called at the user boundary inside run_send.
pub(crate) async fn maybe_auto_compact(
    state: &State<'_, AppState>,
    session_id: &str,
    cfg: &AppConfig,
    stop: &AtomicBool,
) {
    let Ok(sf) = state.store.load(session_id) else { return };
    if sf.meta.kind != "chat" || sf.messages.len() < COMPACT_MIN_MESSAGES {
        return;
    }
    let binding = match sf.meta.bindings.first() {
        Some(b) => b.clone(),
        None => return,
    };
    let Some(provider) = resolve_provider(cfg, &binding) else { return };
    let window = provider.context_window.unwrap_or(131_072) as f64;
    let last_input = sf
        .telemetry
        .iter()
        .filter_map(|r| r.input_tokens)
        .next_back()
        .unwrap_or(0) as f64;
    let completed_turns = sf.messages.iter().filter(|m| m.role == "user").count() as u64;
    // thrash guard (L6 §6.2): while boosted, this session's trigger line
    // sits at 80% so rapid re-compaction can't loop
    let trigger_line = if sf.meta.compact_boost_until_turn > completed_turns {
        0.80
    } else {
        COMPACT_AT_FRACTION
    };
    if last_input < window * trigger_line {
        return;
    }
    // the summary call is a full one-shot completion (up to 120s) — never
    // start it for a turn the user already stopped
    if stop.load(Ordering::Relaxed) {
        return;
    }
    // Compaction ladder L1 — free retro-prune: recover oversized tool
    // results without any model call. When that alone projects the request
    // back under the compaction line, skip the summary entirely; otherwise
    // fall through and let the summarizer read the now-smaller input.
    // free rungs run their load-modify-save under save_lock: both rewrite
    // the session file whole, so a lane save landing mid-rewrite would be
    // rolled back (the reviewer's lost-message scenario)
    let saved = {
        let _guard = state.save_lock.lock().await;
        prune_oversized_tool_records(&state, &state.data_dir, session_id, cfg.settings.spill_max_chars)
    };
    // free rung 2 (L6 §3): superseded stale tool outputs → stubs. The
    // recent window is everything from the last user turn onward.
    let recent_from = sf
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| m.ts)
        .unwrap_or(0);
    let (elided, _stubs) = {
        let _guard = state.save_lock.lock().await;
        elide_stale_tool_records(&state, &state.data_dir, session_id, cfg.settings.spill_max_chars, recent_from)
    };
    let saved = saved + elided;
    if saved > 0 {
        let projected = last_input - (saved as f64 / 4.0);
        // hysteresis: free rungs count as "rescued" only below the TARGET
        // line, not merely back under the trigger — otherwise the session
        // sits a hair under 0.70 and re-triggers every turn
        if projected < window * COMPACT_TARGET_FRACTION {
            return;
        }
    }
    // payback gate (L6 §2): only summarize when the session's expected
    // remaining life amortizes the rewrite cost. No pricing data ⇒ the
    // gate stays open (pressure-only, previous behavior).
    let folded_tokens: u64 = sf
        .messages
        .iter()
        .filter(|m| m.role == "user" || m.role == "assistant" || m.role == "tool")
        .map(|m| est_tokens(&m.content) + RECORD_OVERHEAD_TOKENS)
        .sum();
    // pressure hard line (P2): the payback gate can stay shut forever when
    // cached-token pricing is unknown (cached_per_m <= 0) and the free
    // rungs have already run dry — such a session grows past the window
    // until the upstream refuses requests. Survival beats savings: past
    // 90% of the window, summarize regardless of economics.
    let pressure = last_input >= window * 0.90;
    if !pressure
        && !auto_compact_payback_ok(
            provider.pricing.get(&binding.model),
            folded_tokens,
            800,
            completed_turns,
        )
    {
        return;
    }
    // thrash engage (L6 §6.2): this is the ≥2nd auto compaction within the
    // last 10 user turns → raise the trigger line for 30 turns and surface
    // a one-time notice alongside the compaction
    let recent_auto = sf
        .meta
        .compactions
        .iter()
        .filter(|c| c.trigger == "auto" && c.completed_turns + 10 > completed_turns)
        .count();
    // one-time notice: while a boost from an earlier engage is still active
    // (e.g. the ≥90% pressure hardline keeps forcing compactions), writing
    // the notice again would spam an identical row into the transcript on
    // every turn — extend silently instead
    if recent_auto >= 1 && sf.meta.compact_boost_until_turn <= completed_turns {
        let _guard = state.save_lock.lock().await;
        if let Ok(mut s) = state.store.load(session_id) {
            s.meta.compact_boost_until_turn = completed_turns + 30;
            s.messages.push(MessageRecord {
                id: uuid::Uuid::new_v4().to_string(),
                lane: 0,
                role: "notice".into(),
                content: format!(
                    "缓存压缩抖动：近 10 轮内第 {} 次自动压缩。上下文增长过快——建议检查是否有循环读取大文件，或调高设置中的溢出阈值。已将本会话压缩触发线临时上调至 80%，30 轮后自动回落。",
                    recent_auto + 1
                ),
                reasoning: None,
                ts: next_record_ts(&state),
                model: None,
                status: "ok".into(),
                usage: None,
                cost_usd: None,
                confidence: None,
                tool_calls: None,
                tool_call_id: None,
                skill_calls: None,
                workflow: None,
                images: Vec::new(),
            });
            s.meta.updated_at = now_ms();
            warn_save(session_id, "thrash notice", state.store.save(&s));
        }
    }
    let _ =
        compact_now(&state, &state.client, &state.data_dir, &state.save_lock, session_id, provider, &binding.model, "auto").await;
}

#[cfg(test)]
mod compact_tests {
    use super::*;

    fn pr(input: f64, cached: f64) -> crate::config::Pricing {
        crate::config::Pricing { input_per_m: input, cached_per_m: cached, output_per_m: 10.0 }
    }

    #[test]
    fn payback_math_matches_estimate_ui() {
        // DeepSeek-ish: input 4, cached 0.5 → spread 3.5
        let p = pr(4.0, 0.5);
        // 100k folded: rewrite 0.0028 vs save 0.05/turn → 1 turn
        assert_eq!(payback_turns(100_000, 800, &p), Some(1));
        // 10k folded: save 0.005 → 0.56 → 1 turn
        assert_eq!(payback_turns(10_000, 800, &p), Some(1));
        // 1600 folded: save 0.0008 → ratio 3.5 → 4 turns
        assert_eq!(payback_turns(1600, 800, &p), Some(4));
    }

    #[test]
    fn payback_none_when_no_spread_or_no_savings() {
        // cached == input → spread 0 → rewrite free → economics undefined
        assert_eq!(payback_turns(100_000, 800, &pr(2.0, 2.0)), None);
        // cached 0 → save 0 → nothing to amortize against
        assert_eq!(payback_turns(100_000, 800, &pr(4.0, 0.0)), None);
    }

    #[test]
    fn gate_opens_without_pricing() {
        assert!(auto_compact_payback_ok(None, 100_000, 800, 4));
    }

    #[test]
    fn gate_blocks_when_folding_saves_nothing() {
        // cached price 0 → every turn saves nothing → never summarize
        assert!(!auto_compact_payback_ok(Some(&pr(4.0, 0.0)), 100_000, 800, 100));
    }

    #[test]
    fn gate_blocks_when_payback_exceeds_expected_life() {
        let p = pr(4.0, 0.5);
        // fresh session (8 turns): life = max(4, 8) = 8
        assert!(auto_compact_payback_ok(Some(&p), 1600, 800, 8));
        // slow payback (19 turns) exceeds fresh-session life
        assert!(!auto_compact_payback_ok(Some(&p), 300, 800, 8));
        // long-lived session (40 turns → life 20) amortizes it
        assert!(auto_compact_payback_ok(Some(&p), 300, 800, 40));
    }
}

#[cfg(test)]
mod memo_tests {
    use super::*;
    use crate::types_rs::{MessageRecord, RollingMemo, ToolCallWire};

    fn call(id: &str, name: &str, path: &str) -> serde_json::Value {
        serde_json::json!([
            {"id": id, "type": "function",
             "function": {"name": name, "arguments": format!("{{\"path\":\"{path}\"}}")}}
        ])
    }

    fn rec(lane: u32, role: &str, content: &str, ts: u64) -> MessageRecord {
        MessageRecord {
            id: uuid::Uuid::new_v4().to_string(),
            lane,
            role: role.into(),
            content: content.into(),
            reasoning: None,
            ts,
            model: None,
            status: "ok".into(),
            usage: None,
            cost_usd: None,
            confidence: None,
            tool_calls: None,
            tool_call_id: None,
            skill_calls: None,
            workflow: None,
            images: Vec::new(),
        }
    }

    #[test]
    fn memo_extracts_files_decisions_and_error_fix() {
        let mut memo = RollingMemo::default();
        let turn = vec![
            ChatMessage::plain("user", "这个不对，改成用 tokio 实现"),
            ChatMessage {
                role: "assistant".into(),
                content: String::new(),
                tool_calls: Some(call("c1", "read_file", "/a/b.rs")),
                tool_call_id: None,
                images: Vec::new(),
            },
            ChatMessage {
                role: "tool".into(),
                content: "fn main() {}".into(),
                tool_calls: None,
                tool_call_id: Some("c1".into()),
                images: Vec::new(),
            },
        ];
        assert!(rolling_memo_apply(&mut memo, &turn));
        assert_eq!(memo.files.get("/a/b.rs").map(String::as_str), Some("read_file"));
        assert!(memo.decisions.iter().any(|d| d.contains("改成用 tokio")));
        // idempotent on replay
        let mut memo2 = memo.clone();
        assert!(!rolling_memo_apply(&mut memo2, &turn));
        assert_eq!(memo, memo2);
    }

    #[test]
    fn memo_records_error_then_fix_in_same_turn() {
        let mut memo = RollingMemo::default();
        let turn = vec![
            ChatMessage::plain("user", "继续"),
            ChatMessage {
                role: "assistant".into(),
                content: String::new(),
                tool_calls: Some(call("c2", "write_file", "/a/c.txt")),
                tool_call_id: None,
                images: Vec::new(),
            },
            ChatMessage {
                role: "tool".into(),
                content: "ERROR: 权限拒绝".into(),
                tool_calls: None,
                tool_call_id: Some("c2".into()),
                images: Vec::new(),
            },
            ChatMessage {
                role: "assistant".into(),
                content: String::new(),
                tool_calls: Some(call("c3", "write_file", "/a/c.txt")),
                tool_call_id: None,
                images: Vec::new(),
            },
            ChatMessage {
                role: "tool".into(),
                content: "written".into(),
                tool_calls: None,
                tool_call_id: Some("c3".into()),
                images: Vec::new(),
            },
        ];
        assert!(rolling_memo_apply(&mut memo, &turn));
        // last action wins, no failure tag on the final entry
        assert_eq!(memo.files.get("/a/c.txt").map(String::as_str), Some("write_file"));
        assert!(memo.errors_fixed.iter().any(|x| x == "/a/c.txt（write_file）"));
    }

    #[test]
    fn memo_render_is_deterministic_and_capped() {
        assert!(render_memo(&RollingMemo::default()).is_none());
        let mut memo = RollingMemo::default();
        memo.goal = Some("完成 L6".into());
        memo.decisions.push("改用 tokio".into());
        memo.files.insert("/a/b.rs".into(), "read_file".into());
        let r1 = render_memo(&memo).unwrap();
        let r2 = render_memo(&memo).unwrap();
        assert_eq!(r1, r2);
        assert!(r1.contains("[会话备忘·自动维护]"));
        assert!(r1.contains("goal: 完成 L6"));
        assert!(r1.contains("- /a/b.rs ← read_file"));
        // cap: 12 fat decisions must not blow the budget
        let mut big = RollingMemo::default();
        for i in 0..12 {
            big.decisions.push(format!("决定{i}:{}", "很长的决定内容".repeat(40)));
        }
        let rendered = render_memo(&big).unwrap();
        assert!(rendered.chars().count() < MEMO_CAP_CHARS + 60);
        assert!(rendered.contains("…[截断]"));
    }

    #[test]
    fn elide_stubs_superseded_stale_outputs_only() {
        let dir = std::env::temp_dir().join(format!("ccharness-elide-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // struct literal, NOT AppState::new(): the constructor runs global
        // one-time init (spill root OnceLock, privacy log) that would poison
        // other tests in the same process
        let state = AppState {
            data_dir: dir.clone(),
            store: SessionStore::new(&dir),
            client: reqwest::Client::new(),
            stream_client: reqwest::Client::new(),
            stops: std::sync::Mutex::new(HashMap::new()),
            save_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
            prefixes: std::sync::Mutex::new(None),
            seq: std::sync::atomic::AtomicU64::new(1),
            last_ts: std::sync::atomic::AtomicU64::new(0),
            approvals: std::sync::Mutex::new(None),
            grants: std::sync::Mutex::new(None),
            last_span: std::sync::Mutex::new(None),
            permissions: std::sync::Mutex::new(None),
            workflow: std::sync::Mutex::new(None),
            sm_state: std::sync::Mutex::new(None),
            force_quit: std::sync::atomic::AtomicBool::new(false),
        };
        let store = SessionStore::new(&dir);
        let sf = store.create("chat", vec![crate::types_rs::SessionBinding {
            provider_id: "p".into(),
            model: "m".into(),
        }], "elide test")
        .unwrap();
        let sid = sf.meta.id.clone();
        let big_old = "x".repeat(40_000);
        let big_keep = "y".repeat(30_000);
        let mut s = store.load(&sid).unwrap();
        // old big read of /big.txt (superseded below) → elidable
        s.messages.push(rec(0, "user", "第一轮", 1));
        s.messages.push(MessageRecord {
            tool_calls: Some(vec![ToolCallWire {
                id: "c1".into(),
                name: "read_file".into(),
                arguments: "{\"path\":\"/big.txt\"}".into(),
            }]),
            ..rec(0, "assistant", "", 2)
        });
        s.messages.push(MessageRecord {
            tool_call_id: Some("c1".into()),
            ..rec(0, "tool", &big_old, 3)
        });
        // big read of /keep.txt — never superseded → must stay verbatim
        s.messages.push(MessageRecord {
            tool_calls: Some(vec![ToolCallWire {
                id: "c3".into(),
                name: "read_file".into(),
                arguments: "{\"path\":\"/keep.txt\"}".into(),
            }]),
            ..rec(0, "assistant", "", 4)
        });
        s.messages.push(MessageRecord {
            tool_call_id: Some("c3".into()),
            ..rec(0, "tool", &big_keep, 5)
        });
        // superseding re-read of /big.txt: newer same-target record makes
        // the ts=3 output stale
        s.messages.push(MessageRecord {
            tool_calls: Some(vec![ToolCallWire {
                id: "c2".into(),
                name: "read_file".into(),
                arguments: "{\"path\":\"/big.txt\"}".into(),
            }]),
            ..rec(0, "assistant", "", 6)
        });
        s.messages.push(MessageRecord {
            tool_call_id: Some("c2".into()),
            ..rec(0, "tool", "new content", 7)
        });
        // recent window: last user turn from ts=9 onward
        s.messages.push(rec(0, "assistant", "新一轮开始", 8));
        s.messages.push(rec(0, "user", "第二轮", 9));
        store.save(&s).unwrap();

        let (saved, stubs) = elide_stale_tool_records(&state, &dir, &sid, 24_000, 9);
        // token-unit accounting: 40k ASCII chars ≈ 10k tokens, stub ≈ 30
        assert!(saved > 10_000 - 500, "{saved}");
        assert_eq!(stubs, 1);

        let s2 = store.load(&sid).unwrap();
        let stubbed = s2.messages.iter().find(|m| m.tool_call_id.as_deref() == Some("c1")).unwrap();
        assert!(stubbed.content.starts_with("[已降级] read_file /big.txt @ts=3"));
        assert!(stubbed.content.contains("原输出 40000 字符已过时"));
        // pairing survives (I1): still a tool record on the same call id
        assert_eq!(stubbed.role, "tool");
        let kept = s2.messages.iter().find(|m| m.tool_call_id.as_deref() == Some("c3")).unwrap();
        assert_eq!(kept.content, big_keep);
        // idempotent: second run saves nothing
        assert_eq!(elide_stale_tool_records(&state, &dir, &sid, 24_000, 9), (0, 0));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod bucket_tests {
    use super::*;
    use crate::types_rs::{MessageRecord, SessionBinding};

    fn rec(role: &str, content: &str, ts: u64) -> MessageRecord {
        MessageRecord {
            id: uuid::Uuid::new_v4().to_string(),
            lane: 0,
            role: role.into(),
            content: content.into(),
            reasoning: None,
            ts,
            model: None,
            status: "ok".into(),
            usage: None,
            cost_usd: None,
            confidence: None,
            tool_calls: None,
            tool_call_id: None,
            skill_calls: None,
            workflow: None,
            images: Vec::new(),
        }
    }

    fn big_tool(ts: u64, n: usize) -> MessageRecord {
        MessageRecord {
            tool_call_id: Some(format!("t{ts}")),
            ..rec("tool", &"x".repeat(n), ts)
        }
    }

    #[test]
    fn keep_boundary_respects_budget_but_keeps_newest_turn() {
        let msgs = vec![
            rec("user", "第一轮", 1),
            rec("assistant", "回复一", 2),
            rec("user", "第二轮", 3),
            big_tool(4, 20_000),
            rec("user", "第三轮", 5),
            rec("assistant", "回复三", 6),
        ];
        // tiny budget: only the newest turn fits → boundary at its user ts
        assert_eq!(compute_keep_from_ts(&msgs, 1_000), 5);
        // generous budget: all three turns kept → boundary at the first ts
        assert_eq!(compute_keep_from_ts(&msgs, 1_000_000), 1);
    }

    #[test]
    fn keep_boundary_keeps_at_least_one_turn_even_when_it_alone_exceeds() {
        let msgs = vec![
            rec("user", "第一轮", 1),
            rec("user", "第二轮超大", 2),
            big_tool(3, 50_000),
        ];
        assert_eq!(compute_keep_from_ts(&msgs, 1_000), 2);
    }

    #[test]
    fn est_tokens_handles_cjk_not_as_bytes_over_four() {
        // 400 ASCII chars ≈ 100 tokens
        assert_eq!(est_tokens(&"a".repeat(400)), 100);
        // 400 CJK chars are 1200 UTF-8 bytes: flat bytes/4 said 300,
        // reality is ≈320 (1.25 chars/token) — the old estimate
        // systematically inflated KEEP budgets and payback (P2)
        let t = est_tokens(&"中".repeat(400));
        assert!((300..=340).contains(&t), "{t}");
        let mixed = est_tokens(&format!("{}{}", "a".repeat(400), "中".repeat(400)));
        assert!((400..=440).contains(&mixed), "{mixed}");
    }

    #[test]
    fn summarize_input_covers_fold_range_only_and_prepends_memo() {
        let dir = std::env::temp_dir().join(format!("ccharness-bucket-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = SessionStore::new(&dir);
        let sf = store
            .create("chat", vec![SessionBinding { provider_id: "p".into(), model: "m".into() }], "t")
            .unwrap();
        let sid = sf.meta.id.clone();
        let mut s = store.load(&sid).unwrap();
        s.messages.push(rec("user", "第一轮旧内容", 1));
        s.messages.push(rec("assistant", "旧回复", 2));
        s.messages.push(rec("user", "最新一轮保留", 9));
        store.save(&s).unwrap();
        let sf = store.load(&sid).unwrap();
        let input = summarize_input(&sf, 9, Some("[会话备忘]\nfiles:/n- /a ← read"));
        assert!(input.starts_with("[当前会话备忘]"));
        assert!(input.contains("第一轮旧内容"));
        assert!(!input.contains("最新一轮保留"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}



