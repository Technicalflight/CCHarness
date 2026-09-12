//! Cache warmer (opt-in keepalive, "one-shot heartbeat"): after a
//! successful turn, schedule ONE non-streaming probe request a margin
//! under the ~5min Short-TTL window. It replays the exact Zone S+H bytes
//! the next real request will reuse, refreshing the upstream cache at the
//! cached-input rate instead of letting the whole prefix go cold and be
//! re-billed at full input price.
//!
//! Cost model (per fired warmup): prefix tokens × cached rate + a few
//! tail tokens + 1 output token (`max_tokens:1`). Payback condition:
//! P(return within the next TTL window) > cached ÷ (input − cached) —
//! ≈14% for DeepSeek, ≈5% for OpenAI pricing. One shot per turn end,
//! never a loop, so the worst case waste is a single cached-rate
//! request per idle gap.
//!
//! Safety rails:
//! - opt-in via `settings.cache_warmup` (default off)
//! - OpenAI-compatible providers only; tier None (no caching) skipped
//! - reasoning_effort-bound prefixes skipped — reasoning models don't
//!   cap their CoT with `max_tokens`, so a probe could burn a real
//!   thinking tail for nothing
//! - any failure is silent (worst case: a cold cache) — never surfaces
//!   as a session error
//! - a new turn on the same lane cancels the pending probe before any
//!   pre-stream phase runs

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Delay before the keepalive fires: a margin under the ~5min Short-TTL
/// window, so the refreshed cache still has useful lifetime when the user
/// comes back.
pub const WARMUP_DELAY_SECS: u64 = 240;
/// Fixed probe tail — tiny, never persisted, never enters the session.
pub const WARMUP_PING: &str = "ping";

/// Snapshot of everything a keepalive request needs. Taken right after the
/// turn tail falls into Zone H, so the prefix bytes ARE the next real
/// request's cached span.
pub struct WarmSlot {
    pub prefix: crate::prefix::LanePrefix,
    pub model: String,
    pub tools: Option<serde_json::Value>,
}

/// (session, lane) → pending probe task + generation tag. The tag makes
/// self-cleanup safe: a newer schedule replaces the entry, and the older
/// task's cleanup must not remove it.
static WARMERS: Mutex<Option<HashMap<(String, u32), (u64, tauri::async_runtime::JoinHandle<()>)>>> =
    Mutex::new(None);
static GEN: AtomicU64 = AtomicU64::new(0);

/// Drop (and abort) any pending keepalive for this lane. Called at the
/// start of every lane task so a probe never fires mid-turn.
pub fn cancel(session_id: &str, lane: u32) {
    if let Some(handle) = WARMERS
        .lock()
        .unwrap()
        .as_mut()
        .and_then(|m| m.remove(&(session_id.to_string(), lane)))
    {
        handle.1.abort();
    }
}

/// Schedule a one-shot keepalive for this lane. Replaces (and aborts) any
/// previously pending probe for the same key.
pub fn schedule(
    client: reqwest::Client,
    provider: crate::config::Provider,
    session_id: String,
    lane: u32,
    slot: WarmSlot,
) {
    cancel(&session_id, lane);
    let gen = GEN.fetch_add(1, Ordering::Relaxed);
    let key = (session_id.clone(), lane);
    let task_key = key.clone();
    let handle = tauri::async_runtime::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(WARMUP_DELAY_SECS)).await;
        let res = fire(&client, &provider, &slot).await;
        if let Err(e) = res {
            eprintln!("[warmer] 保温请求失败（忽略，缓存保持原状）: {e}");
        }
        // self-cleanup only if we still own the registry entry
        if let Some(map) = WARMERS.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
            if map.get(&task_key).map(|(g, _)| *g) == Some(gen) {
                map.remove(&task_key);
            }
        }
    });
    WARMERS
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(key, (gen, handle));
}

/// Eligibility gate shared by the call site and tests: warmup must be
/// opt-in, cache-capable (tier != None), non-reasoning, OpenAI-compatible.
pub fn eligible(
    cache_warmup_on: bool,
    kind: crate::config::ProviderKind,
    tier: crate::config::CacheTier,
    reasoning_bound: bool,
) -> bool {
    cache_warmup_on
        && kind == crate::config::ProviderKind::OpenaiCompatible
        && tier != crate::config::CacheTier::None
        && !reasoning_bound
}

/// Send the keepalive request. The body carries the same head identity
/// (model / cache key / tools schema) and the same Zone S+H messages as
/// the next real request; only the transport flags differ (non-streaming,
/// max_tokens:1) — prompt-cache identity doesn't include those.
async fn fire(
    client: &reqwest::Client,
    provider: &crate::config::Provider,
    slot: &WarmSlot,
) -> Result<(), String> {
    let body = slot.prefix.build_warmup_body(&slot.model, slot.tools.as_ref());
    let mut req = client
        .post(crate::chat::endpoint_chat(provider))
        .header("content-type", "application/json")
        // same affinity header the real requests send — a replica-routed
        // gateway would otherwise warm a shard the next turn never hits
        .header("x-session-affinity", slot.prefix.cache_key())
        .body(body)
        .timeout(std::time::Duration::from_secs(120));
    for (k, v) in crate::chat::auth_for(provider).headers().iter() {
        req = req.header(k, v);
    }
    let resp = req.send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    let v: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }
    if let Some(u) = v.get("usage") {
        let total = u.get("prompt_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
        let cached = u
            .get("prompt_cache_hit_tokens")
            .or_else(|| {
                u.get("prompt_tokens_details")
                    .and_then(|d| d.get("cached_tokens"))
            })
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        eprintln!("[warmer] 缓存已保温: 输入 {total} tokens（命中 {cached}）");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lp() -> crate::prefix::LanePrefix {
        crate::prefix::LanePrefix::new("你是严谨的编程助手", "sess-warm")
    }

    #[test]
    fn warmup_body_is_nonstreaming_one_token_and_carries_cache_identity() {
        let mut slot_lp = lp();
        slot_lp.bind_cache_tier(crate::config::CacheTier::Short);
        slot_lp.append(&crate::prefix::ChatMessage::plain("user", "写个快排"));
        slot_lp.append(&crate::prefix::ChatMessage::plain("assistant", "好的，以下是实现…"));
        let tools = serde_json::json!([{"type":"function","function":{"name":"read_file"}}]);
        let body = slot_lp.build_warmup_body("deepseek-chat", Some(&tools));
        // transport flags differ from the real request on purpose
        assert!(body.contains("\"stream\":false"));
        assert!(body.contains("\"max_tokens\":1"));
        assert!(!body.contains("stream_options"));
        // cache identity survives: routing key + tools + the full Zone S+H
        assert!(body.contains("\"prompt_cache_key\":\"sess-warm\""));
        assert!(body.contains("read_file"));
        assert!(body.contains("你是严谨的编程助手"));
        assert!(body.contains("写个快排"));
        // the probe tail rides last as a plain user message
        assert!(body.contains(WARMUP_PING));
    }

    #[test]
    fn warmup_body_omits_key_for_tier_none() {
        let mut slot_lp = lp();
        slot_lp.bind_cache_tier(crate::config::CacheTier::None);
        let body = slot_lp.build_warmup_body("m", None);
        assert!(!body.contains("prompt_cache_key"));
    }

    #[test]
    fn eligibility_gate_blocks_reasoning_and_tier_none() {
        use crate::config::{CacheTier, ProviderKind};
        let ok = eligible(true, ProviderKind::OpenaiCompatible, CacheTier::Short, false);
        assert!(ok);
        // opt-in off
        assert!(!eligible(false, ProviderKind::OpenaiCompatible, CacheTier::Short, false));
        // no caching through routing ⇒ nothing to keep warm
        assert!(!eligible(true, ProviderKind::OpenaiCompatible, CacheTier::None, false));
        // reasoning models don't cap CoT with max_tokens ⇒ skip
        assert!(!eligible(true, ProviderKind::OpenaiCompatible, CacheTier::Short, true));
        // non-OpenAI-compatible kinds use different cache mechanics
        assert!(!eligible(true, ProviderKind::Anthropic, CacheTier::Short, false));
    }

    #[test]
    fn cancel_on_empty_registry_is_noop() {
        // cancel with nothing pending must not panic
        cancel("sess-x", 0);
        cancel("sess-x", 0);
    }
}
