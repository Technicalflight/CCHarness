// Provider HTTP + SSE streaming + send orchestration.
//
// Design notes (docs/design/cache-hit-mechanism.md):
// - OpenAI-compatible path builds the request body by byte concatenation of
//   pre-serialized fragments so Zone S+H is byte-identical across turns.
// - Usage is read from provider chunks (OpenAI `usage` /
//   DeepSeek `prompt_cache_hit_tokens` / OpenAI `prompt_tokens_details.cached_tokens`
//   / Anthropic `cache_read_input_tokens`) and lands in the telemetry ledger.
// - Cost is computed from per-model pricing: non-cached input at input price,
//   cached input at cached price, output at output price.

use crate::config::{Provider, ProviderKind};use crate::prefix::{self, ChatMessage, LanePrefix};
use crate::sessions::SessionFile;
use crate::types_rs::{MessageRecord, StreamEvent, ToolCallWire, UsageStat};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::ipc::Channel;

/// One parsed tool invocation from an assistant turn.
#[derive(Debug, Clone)]
pub struct ParsedToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// Incremental accumulator for streamed `delta.tool_calls` fragments.
#[derive(Default)]
struct ToolCallAcc {
    id: String,
    name: String,
    args: String,
}

pub struct LaneOutcome {
    pub content: String,
    pub reasoning: String,
    pub status: String, // ok | stopped | error
    pub usage: UsageStat,
    pub confidence: Option<u32>,
    pub tool_calls: Vec<ParsedToolCall>,
}

/// Live-progress tap for delegated sub-agents: forwards the sub lane's text
/// and reasoning deltas to the parent UI as SubProgress events, so the
/// delegate tool card streams in real time while the parent round waits.
#[derive(Clone)]
pub struct ProgressTap {
    /// Parent lane that owns the delegation (for the UI's stream index).
    pub lane: u32,
    /// Tool-call id of the delegate_subagent invocation this tap belongs to.
    pub call_id: String,
    /// Short sub-task title for display.
    pub title: String,
    pub channel: Channel<StreamEvent>,
}

impl ProgressTap {
    pub fn forward(&self, text: &str, done: bool) {
        let _ = self.channel.send(StreamEvent::SubProgress {
            lane: self.lane,
            call_id: self.call_id.clone(),
            title: self.title.clone(),
            text: text.to_string(),
            done,
        });
    }
}

pub struct SendCtx<'a> {
    pub client: &'a reqwest::Client,
    pub provider: &'a Provider,
    pub model: &'a str,
    pub lane: u32,
    pub message_id: String,
    pub channel: Channel<StreamEvent>,
    pub stop: Arc<AtomicBool>,
    /// Present only for delegated sub-agents (None in main/arena lanes).
    pub progress_tap: Option<ProgressTap>,
    /// `x-session-affinity` header value (the lane's stable cache key).
    /// Providers with replica-level KV caches (Fireworks, Anthropic-compatible
    /// gateways, …) route requests carrying the same affinity value back to
    /// the same worker, which is what makes follow-up turns hit the prefix
    /// cache. Unknown headers are ignored by every other gateway, so sending
    /// it is always safe.
    pub affinity: Option<String>,
}

impl SendCtx<'_> {
    /// Emit a text delta to this lane's channel; when a progress tap is
    /// attached, also forward the same bytes to the parent UI.
    fn emit_delta(&self, text: &str) {
        let _ = self.channel.send(StreamEvent::Delta {
            lane: self.lane,
            message_id: self.message_id.clone(),
            text: text.to_string(),
        });
        if let Some(tap) = &self.progress_tap {
            tap.forward(text, false);
        }
    }

    fn emit_reasoning(&self, text: &str) {
        let _ = self.channel.send(StreamEvent::Reasoning {
            lane: self.lane,
            message_id: self.message_id.clone(),
            text: text.to_string(),
        });
        if let Some(tap) = &self.progress_tap {
            tap.forward(text, false);
        }
    }
}

/// Stream one assistant turn on a lane. Returns the outcome; the caller owns
/// persistence (session file + prefix append).
pub async fn stream_lane(
    ctx: &SendCtx<'_>,
    body: String,
    auth: AuthHeader,
) -> Result<LaneOutcome, String> {
    let _ = ctx.channel.send(StreamEvent::Started {
        lane: ctx.lane,
        model: ctx.model.to_string(),
        message_id: ctx.message_id.clone(),
    });

    let req = ctx
        .client
        .post(&endpoint_chat(ctx.provider))
        .header("Content-Type", "application/json")
        .headers(auth.headers())
        .headers(affinity_headers(ctx))
        .body(body)
        .send()
        .await;

    let resp = match req {
        Ok(r) => r,
        Err(e) => return Err(format!("网络错误: {e}")),
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let body_text = resp.text().await.unwrap_or_default();
        let snippet: String = body_text.chars().take(400).collect();
        return Err(format!("上游返回 {status}: {snippet}"));
    }

    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut usage = UsageStat::default();
    let mut stopped = false;
    let mut cfilter = crate::confidence::ConfidenceFilter::new();
    let mut tool_accs: Vec<ToolCallAcc> = Vec::new();
    let mut finish_reason: Option<String> = None;
    // Anthropic block index → tool_accs position (text blocks interleave)
    let mut anthropic_blocks: std::collections::HashMap<u64, usize> =
        std::collections::HashMap::new();

    use futures_util::StreamExt;
    'read: loop {
        if ctx.stop.load(Ordering::Relaxed) {
            stopped = true;
            break;
        }
        let chunk = match stream.next().await {
            Some(Ok(b)) => b,
            Some(Err(e)) => {
                if content.is_empty() {
                    return Err(format!("流中断: {e}"));
                }
                // partial content already delivered — treat as stop
                stopped = true;
                break;
            }
            None => break,
        };
        buf.extend_from_slice(&chunk);

        // extract complete SSE frames separated by \n\n
        while let Some(pos) = find_frame_end(&buf) {
            let frame: Vec<u8> = buf.drain(..pos).collect();
            let frame = String::from_utf8_lossy(&frame);
            for line in frame.lines() {
                let data = match line.strip_prefix("data:") {
                    Some(d) => d.trim(),
                    None => continue,
                };
                if data == "[DONE]" {
                    continue;
                }
                let v: Value = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if let Some(err) = dispatch_sse_frame(
                    ctx,
                    &v,
                    &mut content,
                    &mut reasoning,
                    &mut usage,
                    &mut cfilter,
                    &mut tool_accs,
                    &mut finish_reason,
                    &mut anthropic_blocks,
                ) {
                    // HTTP was 200, but the stream itself reported a fatal
                    // error. With nothing delivered yet, fail the turn so
                    // the user sees the real upstream message instead of an
                    // empty ok record; partial output already on screen
                    // closes as a stopped turn (same philosophy as a broken
                    // transport with partial content).
                    if content.is_empty() && reasoning.is_empty() && tool_accs.is_empty() {
                        return Err(format!("上游流内错误: {err}"));
                    }
                    stopped = true;
                    break 'read;
                }
            }
        }
    }

    // Some gateways close the connection without the trailing blank line —
    // the final buffered frame (typically the usage block or finish_reason)
    // must still be parsed, or telemetry silently loses usage for every
    // such provider
    {
        let frame = String::from_utf8_lossy(&buf);
        for line in frame.lines() {
            let data = match line.strip_prefix("data:") {
                Some(d) => d.trim(),
                None => continue,
            };
            if data == "[DONE]" {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<Value>(data) {
                let _ = dispatch_sse_frame(
                    ctx,
                    &v,
                    &mut content,
                    &mut reasoning,
                    &mut usage,
                    &mut cfilter,
                    &mut tool_accs,
                    &mut finish_reason,
                    &mut anthropic_blocks,
                );
            }
        }
    }

    // flush any holdback that never completed into a marker
    let tail = cfilter.finish();
    if !tail.is_empty() {
        content.push_str(&tail);
        ctx.emit_delta(&tail);
    }

    let tool_calls = tool_accs
        .into_iter()
        .enumerate()
        .filter(|(_, tc)| !tc.name.is_empty())
        .map(|(i, tc)| ParsedToolCall {
            id: if tc.id.is_empty() { format!("call_{}_{i}", ctx.lane) } else { tc.id },
            name: tc.name,
            arguments: if tc.args.trim().is_empty() { "{}".into() } else { tc.args },
        })
        .collect::<Vec<_>>();
    // The canonical signal is finish_reason == "tool_calls", but legacy
    // gateways speak function_call semantics or just close with "stop"
    // while still streaming tool_calls — dropping the batch there would
    // silently lose a tool turn. Fall back to the observed accumulators:
    // anything actually assembled counts on the lenient finishes.
    let finish = finish_reason.as_deref();
    let wants_tools = if finish == Some("tool_calls") {
        !tool_calls.is_empty()
    } else {
        matches!(finish, Some("function_call") | None | Some("stop")) && !tool_calls.is_empty()
    };

    let status = if stopped { "stopped" } else { "ok" };
    Ok(LaneOutcome {
        content,
        reasoning,
        status: status.into(),
        usage,
        confidence: cfilter.confidence(),
        tool_calls: if wants_tools { tool_calls } else { Vec::new() },
    })
}

fn find_frame_end(buf: &[u8]) -> Option<usize> {
    // Providers differ in wire framing: LF-LF (\n\n) is the common case,
    // but some gateways emit CRLF-CRLF (\r\n\r\n). Accept whichever
    // terminator appears first; a pure CRLF stream never contains a bare
    // \n\n pair (bytes interleave as 0d 0a 0d 0a), so the two cannot
    // mis-slice each other. Line handling downstream (str::lines + trim)
    // already tolerates \r.
    let lf = buf.windows(2).position(|w| w == b"\n\n").map(|p| p + 2);
    let crlf = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4);
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

#[cfg(test)]
mod sse_tests {
    use super::find_frame_end;

    #[test]
    fn frame_end_accepts_lf_and_crlf() {
        assert_eq!(find_frame_end(b"data: {\"a\":1}\n\ndata: {\"b\":2}\n\n"), Some(15));
        assert_eq!(find_frame_end(b"data: {\"a\":1}\r\n\r\ndata: x"), Some(17));
        assert_eq!(find_frame_end(b"data: partial"), None);
        // mixed streams: whichever terminator completes first wins
        assert_eq!(find_frame_end(b"a\n\rb\r\n\r\nc"), Some(8));
        // plain LF framing is still found when no CRLF terminator exists
        assert_eq!(find_frame_end(b"x\ry\n\nz"), Some(5));
    }
}

fn affinity_header_map(affinity: Option<&str>) -> reqwest::header::HeaderMap {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
    let mut hm = HeaderMap::new();
    if let Some(key) = affinity {
        if let Ok(v) = HeaderValue::from_str(key) {
            hm.insert(HeaderName::from_static("x-session-affinity"), v);
        }
    }
    hm
}

/// `x-session-affinity` header on every lane request (pi-runtime parity,
/// now including the Anthropic path — pi's anthropic adapter sends the same
/// header): providers with replica-level KV caches route requests carrying
/// the same affinity value back to the same worker, which is what makes
/// follow-up turns hit the prefix cache. api.anthropic.com and every other
/// gateway simply ignore the unknown header, so sending it is always safe.
fn affinity_headers(ctx: &SendCtx<'_>) -> reqwest::header::HeaderMap {
    affinity_header_map(ctx.affinity.as_deref())
}

/// Outcome of the significant-miss analysis for one request (pi-runtime
/// parity: turns provider-reported usage into "how much did this miss
/// actually cost me").
#[derive(Debug, Clone)]
pub struct MissAnalysis {
    /// The stable prefix should have hit but was re-billed at full price.
    pub significant: bool,
    /// Tokens billed at the uncached input rate beyond the expected new tail.
    pub rebilled_tokens: u64,
    /// Why: "upstream" (idle TTL / eviction / routing), "client" (local
    /// prefix rewrite — chain broken), "expected" (new epoch / first request).
    pub cause: &'static str,
}

/// Heuristic significant-miss detector, pi `cache-stats` style but exploiting
/// CCHarness's exact byte accounting: the request legitimately re-bills only
/// its new tail (`added_bytes`, ≈4 bytes/token mixed-script); everything
/// beyond that was supposed to be a cache read. Bucket semantics differ by
/// provider: OpenAI-style usage reports cached tokens as a SUBSET of
/// prompt_tokens, while Anthropic reports three DISJOINT buckets (input tail
/// / cache_write / cache_read — pi sums them for totalTokens), so the
/// re-billed portion on the Anthropic path is `input + cache_write`.
pub fn analyze_cache_miss(
    usage: &UsageStat,
    kind: &ProviderKind,
    added_bytes: usize,
    chain_ok: bool,
    epoch_bumped: bool,
) -> MissAnalysis {
    let cause = if !chain_ok {
        "client"
    } else if epoch_bumped {
        "expected"
    } else {
        "upstream"
    };
    let (input, cached) = match (usage.input, usage.cached) {
        (Some(i), Some(c)) => (i, c),
        _ => return MissAnalysis { significant: false, rebilled_tokens: 0, cause },
    };
    let uncached = match kind {
        ProviderKind::Anthropic => input.saturating_add(usage.cache_write.unwrap_or(0)),
        _ => input.saturating_sub(cached),
    };
    let expected_new = (added_bytes as u64) / 4; // mixed-script token estimate
    // Significant: the re-billed portion dwarfs the legitimate new material
    // (2×) AND is large in absolute terms — tiny prompts below OpenAI's
    // 1024-token cache floor are noise, not misses.
    let rebilled = uncached.saturating_sub(expected_new);
    let significant = cause != "expected" && uncached >= 10_000 && uncached > expected_new.saturating_mul(2);
    MissAnalysis { significant, rebilled_tokens: if significant { rebilled } else { 0 }, cause }
}

/// Anthropic 1h-TTL cache-write premium: writes bill at 2× the base input
/// token price (5m writes would be 1.25×; the request path always marks
/// `ttl:"1h"`). OpenAI-style providers bill writes at the plain input rate.
const ANTHROPIC_1H_WRITE_PREMIUM: f64 = 2.0;

fn write_rate_per_m(p: &Provider, pr: &crate::config::Pricing) -> f64 {
    match p.kind {
        ProviderKind::Anthropic => pr.input_per_m * ANTHROPIC_1H_WRITE_PREMIUM,
        _ => pr.input_per_m,
    }
}

/// Cost of the re-billed tokens at the uncached-minus-cached spread (the
/// money a miss wastes compared to the hit it should have been). pi
/// `cache-stats` precision: derive the effective paid rate from the
/// request's OWN bucket split instead of assuming every re-billed token
/// lands in the plain input bucket — on Anthropic a missed prefix is
/// typically re-WRITTEN to cache, billing at the 1h write premium. None
/// when the model has no pricing configured — callers then show tokens only.
pub fn rebill_cost(rebilled_tokens: u64, usage: &UsageStat, p: &Provider, model: &str) -> Option<f64> {
    let pr = p.pricing.get(model)?;
    let paid_per_m = match p.kind {
        ProviderKind::Anthropic => {
            let i = usage.input.unwrap_or(0) as f64;
            let w = usage.cache_write.unwrap_or(0) as f64;
            let total = i + w;
            if total > 0.0 {
                (i * pr.input_per_m + w * write_rate_per_m(p, pr)) / total
            } else {
                pr.input_per_m
            }
        }
        _ => pr.input_per_m,
    };
    let spread = paid_per_m - pr.cached_per_m;
    if spread <= 0.0 {
        return None;
    }
    Some((rebilled_tokens as f64 / 1e6 * spread * 10000.0).round() / 10000.0)
}

#[allow(clippy::too_many_arguments)]
fn handle_openai_frame(
    ctx: &SendCtx<'_>,
    v: &Value,
    content: &mut String,
    reasoning: &mut String,
    usage: &mut UsageStat,
    cfilter: &mut crate::confidence::ConfidenceFilter,
    tool_accs: &mut Vec<ToolCallAcc>,
    finish_reason: &mut Option<String>,
) -> Option<String> {
    if let Some(u) = v.get("usage") {
        usage.input = u.get("prompt_tokens").and_then(|x| x.as_u64());
        usage.output = u.get("completion_tokens").and_then(|x| x.as_u64());
        // OpenAI style
        if let Some(c) = u.pointer("/prompt_tokens_details/cached_tokens").and_then(|x| x.as_u64()) {
            usage.cached = Some(c);
        }
        // DeepSeek style (takes precedence if present)
        if let Some(c) = u.get("prompt_cache_hit_tokens").and_then(|x| x.as_u64()) {
            usage.cached = Some(c);
        }
    }
    let choice0 = v.pointer("/choices/0");
    if let Some(choice) = choice0 {
        if let Some(fr) = choice.get("finish_reason").and_then(|x| x.as_str()) {
            if !fr.is_empty() {
                *finish_reason = Some(fr.to_string());
            }
        }
    }
    let delta = v.pointer("/choices/0/delta");
    if let Some(d) = delta {
        // reasoning streams under `reasoning_content` on most providers;
        // OpenRouter-style gateways use the bare `reasoning` key — without
        // the fallback the whole chain of thought silently disappears
        let rt = d
            .get("reasoning_content")
            .and_then(|x| x.as_str())
            .or_else(|| d.get("reasoning").and_then(|x| x.as_str()));
        if let Some(t) = rt {
            if !t.is_empty() {
                reasoning.push_str(t);
                ctx.emit_reasoning(t);
            }
        }
        if let Some(t) = d.get("content").and_then(|x| x.as_str()) {
            if !t.is_empty() {
                let clean = cfilter.push(t);
                if !clean.is_empty() {
                    content.push_str(&clean);
                    ctx.emit_delta(&clean);
                }
            }
        }
        // streamed tool-call fragments, indexed assembly. The index comes
        // from the wire — a hostile/broken frame with a huge index would
        // allocate millions of accumulator slots here (OOM). Legal streams
        // count up from 0 with no gaps, so anything beyond a small window
        // past the known tail is a malformed frame: skip it.
        if let Some(arr) = d.get("tool_calls").and_then(|x| x.as_array()) {
            for tc in arr {
                let idx = tc.get("index").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
                if idx > 1024 || idx > tool_accs.len() + 64 {
                    continue;
                }
                while tool_accs.len() <= idx {
                    tool_accs.push(ToolCallAcc::default());
                }
                let acc = &mut tool_accs[idx];
                if let Some(id) = tc.get("id").and_then(|x| x.as_str()) {
                    if !id.is_empty() {
                        acc.id = id.to_string();
                    }
                }
                if let Some(name) = tc.pointer("/function/name").and_then(|x| x.as_str()) {
                    acc.name.push_str(name);
                }
                if let Some(args) = tc.pointer("/function/arguments").and_then(|x| x.as_str()) {
                    acc.args.push_str(args);
                }
            }
        }
    }
    stream_error_message(v)
}

#[allow(clippy::too_many_arguments)]
fn handle_anthropic_frame(
    ctx: &SendCtx<'_>,
    v: &Value,
    content: &mut String,
    reasoning: &mut String,
    usage: &mut UsageStat,
    cfilter: &mut crate::confidence::ConfidenceFilter,
    tool_accs: &mut Vec<ToolCallAcc>,
    finish_reason: &mut Option<String>,
    blocks: &mut std::collections::HashMap<u64, usize>,
) -> Option<String> {
    // Anthropic signals mid-stream failures (overloaded_error etc.) as a
    // 200-level `type:"error"` event — it used to fall into the catch-all
    // arm and the turn recorded an empty ok reply
    if v.get("type").and_then(|x| x.as_str()) == Some("error") {
        return stream_error_message(v);
    }
    match v.get("type").and_then(|x| x.as_str()) {
        Some("message_start") => {
            if let Some(inp) = v.pointer("/message/usage/input_tokens").and_then(|x| x.as_u64()) {
                usage.input = Some(inp);
            }
            if let Some(c) = v.pointer("/message/usage/cache_read_input_tokens").and_then(|x| x.as_u64()) {
                usage.cached = Some(c);
            }
            // cache_write is a bucket DISJOINT from input_tokens (pi sums all
            // of input + output + cacheRead + cacheWrite for totalTokens)
            if let Some(w) = v
                .pointer("/message/usage/cache_creation_input_tokens")
                .and_then(|x| x.as_u64())
            {
                usage.cache_write = Some(w);
            }
        }
        Some("content_block_start") => {
            anthropic_tool_start(v, tool_accs, blocks);
        }
        Some("content_block_delta") => {
            let d = v.get("delta");
            match d.map(|d| d.get("type").and_then(|t| t.as_str())) {
                Some(Some("thinking_delta")) => {
                    if let Some(t) = d.and_then(|d| d.get("thinking")).and_then(|x| x.as_str()) {
                        reasoning.push_str(t);
                        ctx.emit_reasoning(t);
                    }
                }
                Some(Some("input_json_delta")) => {
                    anthropic_tool_delta(v, d, tool_accs, blocks);
                }
                Some(Some("text_delta")) | None => {
                    if let Some(t) = d.and_then(|d| d.get("text")).and_then(|x| x.as_str()) {
                        let clean = cfilter.push(t);
                        if !clean.is_empty() {
                            content.push_str(&clean);
                            ctx.emit_delta(&clean);
                        }
                    }
                }
                _ => {}
            }
        }
        Some("message_delta") => {
            // Anthropic stop_reason → OpenAI 语义（stream_lane 的
            // wants_tools 检查的是 "tool_calls"）
            if v.pointer("/delta/stop_reason").and_then(|x| x.as_str()) == Some("tool_use") {
                *finish_reason = Some("tool_calls".to_string());
            }
            if let Some(out) = v.pointer("/usage/output_tokens").and_then(|x| x.as_u64()) {
                usage.output = Some(out);
            }
        }
        _ => {}
    }
    None
}

/// content_block_start(type=tool_use)：注册一个调用accumulator，
/// 记录 block index → accumulator 位置（text/tool 块交错时 index 不连续）。
fn anthropic_tool_start(
    v: &Value,
    tool_accs: &mut Vec<ToolCallAcc>,
    blocks: &mut std::collections::HashMap<u64, usize>,
) {
    if v.pointer("/content_block/type").and_then(|x| x.as_str()) != Some("tool_use") {
        return;
    }
    let idx = v.get("index").and_then(|x| x.as_u64()).unwrap_or(0);
    blocks.insert(idx, tool_accs.len());
    tool_accs.push(ToolCallAcc {
        id: v.pointer("/content_block/id").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        name: v.pointer("/content_block/name").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        args: String::new(),
    });
}

/// content_block_delta(input_json_delta)：分片字段是 partial_json —— 此前
/// 这类分片被当 text_delta 处理（读 delta.text 读不到 → 工具参数整体
/// 丢失，tool_use 静默失效）。按 block index 追加到对应 accumulator。
fn anthropic_tool_delta(
    v: &Value,
    d: Option<&Value>,
    tool_accs: &mut [ToolCallAcc],
    blocks: &mut std::collections::HashMap<u64, usize>,
) {
    if let Some(t) = d.and_then(|d| d.get("partial_json")).and_then(|x| x.as_str()) {
        let idx = v.get("index").and_then(|x| x.as_u64()).unwrap_or(0);
        if let Some(pos) = blocks.get(&idx) {
            if let Some(acc) = tool_accs.get_mut(*pos) {
                acc.args.push_str(t);
            }
        }
    }
}

/// One Responses-API SSE event (OpenaiResponses / AzureResponses lanes).
/// Text/reasoning deltas ride the `delta` field of the `*-text.delta`
/// events; function calls arrive whole on `output_item.done` — no fragment
/// assembly needed. The terminal `response.completed` / `.incomplete` /
/// `.failed` event carries the full response including usage (OpenAI-style
/// bucket semantics: cached is a SUBSET of input_tokens). A failed response
/// ends the stream like an empty reply — the caller's empty-content path
/// reports it, matching how empty chat-completions replies surface today.
#[allow(clippy::too_many_arguments)]
fn handle_responses_frame(
    ctx: &SendCtx<'_>,
    v: &Value,
    content: &mut String,
    reasoning: &mut String,
    usage: &mut UsageStat,
    cfilter: &mut crate::confidence::ConfidenceFilter,
    tool_accs: &mut Vec<ToolCallAcc>,
    finish_reason: &mut Option<String>,
) -> Option<String> {
    // response.failed carries the real failure reason in response.error —
    // folded into the terminal arm before, the message was dropped
    if v.get("type").and_then(|x| x.as_str()) == Some("response.failed") {
        return stream_error_message(v);
    }
    let (text_delta, reasoning_delta) = apply_responses_event(
        v, content, reasoning, usage, cfilter, tool_accs, finish_reason,
    );
    if !text_delta.is_empty() {
        ctx.emit_delta(&text_delta);
    }
    if !reasoning_delta.is_empty() {
        ctx.emit_reasoning(&reasoning_delta);
    }
    None
}

/// Extract the message from an in-stream error frame. Covers the three wire
/// shapes this app can receive: Anthropic `{"type":"error","error":{…}}`,
/// OpenAI-compatible gateways inlining `{"error":{…}}` into a data frame,
/// and Responses `response.failed` with `response.error`.
fn stream_error_message(v: &Value) -> Option<String> {
    let err = v
        .get("error")
        .or_else(|| v.pointer("/response/error"))?;
    let msg = err
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or("上游返回了未说明的错误");
    Some(msg.to_string())
}

/// Provider and model-list endpoints carry the conversation transcript (or
/// provider keys' auth headers) in the request — following a cross-host
/// redirect would hand them to a third party. Same-host hops stay allowed
/// (CDN-style); anything else surfaces as an error instead of silently
/// following. reqwest strips auth headers cross-host on its own, but the
/// body with the full transcript would still ride along.
pub fn same_host_redirects() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|att| {
        let same_host = att
            .previous()
            .first()
            .and_then(|u| u.host_str())
            .zip(att.url().host_str())
            .map(|(a, b)| a == b)
            .unwrap_or(false);
        if same_host {
            att.follow()
        } else {
            att.error("上游跨主机重定向已拒绝")
        }
    })
}

/// Route one parsed SSE frame to the provider handler. Returns the upstream
/// error message when the frame is a fatal in-stream error event.
#[allow(clippy::too_many_arguments)]
fn dispatch_sse_frame(    ctx: &SendCtx<'_>,
    v: &Value,
    content: &mut String,
    reasoning: &mut String,
    usage: &mut UsageStat,
    cfilter: &mut crate::confidence::ConfidenceFilter,
    tool_accs: &mut Vec<ToolCallAcc>,
    finish_reason: &mut Option<String>,
    anthropic_blocks: &mut std::collections::HashMap<u64, usize>,
) -> Option<String> {
    match ctx.provider.kind {
        ProviderKind::OpenaiCompatible => {
            handle_openai_frame(ctx, v, content, reasoning, usage, cfilter, tool_accs, finish_reason)
        }
        ProviderKind::OpenaiResponses | ProviderKind::AzureResponses => {
            handle_responses_frame(ctx, v, content, reasoning, usage, cfilter, tool_accs, finish_reason)
        }
        ProviderKind::Anthropic => handle_anthropic_frame(
            ctx, v, content, reasoning, usage, cfilter, tool_accs, finish_reason, anthropic_blocks,
        ),
    }
}

/// Pure event parser (testable without a Tauri channel): applies one
/// Responses SSE event to the lane accumulators and returns the (content,
/// reasoning) deltas to emit upstream.
#[allow(clippy::too_many_arguments)]
fn apply_responses_event(
    v: &Value,
    content: &mut String,
    reasoning: &mut String,
    usage: &mut UsageStat,
    cfilter: &mut crate::confidence::ConfidenceFilter,
    tool_accs: &mut Vec<ToolCallAcc>,
    finish_reason: &mut Option<String>,
) -> (String, String) {
    match v.get("type").and_then(|x| x.as_str()) {
        Some("response.output_text.delta") => {
            let mut out = String::new();
            if let Some(t) = v.get("delta").and_then(|x| x.as_str()) {
                if !t.is_empty() {
                    let clean = cfilter.push(t);
                    if !clean.is_empty() {
                        content.push_str(&clean);
                        out = clean;
                    }
                }
            }
            (out, String::new())
        }
        Some("response.reasoning_summary_text.delta") => {
            let mut out = String::new();
            if let Some(t) = v.get("delta").and_then(|x| x.as_str()) {
                if !t.is_empty() {
                    reasoning.push_str(t);
                    out = t.to_string();
                }
            }
            (String::new(), out)
        }
        Some("response.output_item.done") => {
            // complete item: {type:"function_call", call_id, name, arguments}
            if v.pointer("/item/type").and_then(|x| x.as_str()) == Some("function_call") {
                tool_accs.push(ToolCallAcc {
                    id: v
                        .pointer("/item/call_id")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    name: v
                        .pointer("/item/name")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    args: v
                        .pointer("/item/arguments")
                        .and_then(|x| x.as_str())
                        .unwrap_or("{}")
                        .to_string(),
                });
            }
            (String::new(), String::new())
        }
        Some("response.completed") | Some("response.incomplete") | Some("response.failed") => {
            if let Some(u) = v.pointer("/response/usage") {
                usage.input = u.get("input_tokens").and_then(|x| x.as_u64());
                usage.output = u.get("output_tokens").and_then(|x| x.as_u64());
                if let Some(c) = u
                    .pointer("/input_tokens_details/cached_tokens")
                    .and_then(|x| x.as_u64())
                {
                    usage.cached = Some(c);
                }
            }
            *finish_reason = Some(if tool_accs.is_empty() { "stop" } else { "tool_calls" }.into());
            (String::new(), String::new())
        }
        _ => (String::new(), String::new()),
    }
}

pub enum AuthHeader {
    Bearer(String),
    Anthropic { key: String },
    /// Azure OpenAI: `api-key` request header (no Bearer / version header).
    Azure { key: String },
}

impl AuthHeader {
    pub fn headers(&self) -> reqwest::header::HeaderMap {
        use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
        let mut hm = HeaderMap::new();
        match self {
            AuthHeader::Bearer(k) => {
                if let Ok(v) = HeaderValue::from_str(&format!("Bearer {k}")) {
                    hm.insert(reqwest::header::AUTHORIZATION, v);
                }
            }
            AuthHeader::Anthropic { key } => {
                if let Ok(k) = HeaderValue::from_str(key) {
                    hm.insert(HeaderName::from_static("x-api-key"), k);
                    hm.insert(
                        HeaderName::from_static("anthropic-version"),
                        HeaderValue::from_static("2023-06-01"),
                    );
                }
            }
            AuthHeader::Azure { key } => {
                if let Ok(k) = HeaderValue::from_str(key) {
                    hm.insert(HeaderName::from_static("api-key"), k);
                }
            }
        }
        hm
    }
}

pub fn auth_for(p: &Provider) -> AuthHeader {
    match p.kind {
        ProviderKind::Anthropic => AuthHeader::Anthropic { key: p.api_key.clone() },
        ProviderKind::AzureResponses => AuthHeader::Azure { key: p.api_key.clone() },
        _ => AuthHeader::Bearer(p.api_key.clone()),
    }
}

pub fn endpoint_chat(p: &Provider) -> String {
    match p.kind {
        ProviderKind::Anthropic => format!("{}/v1/messages", p.base_url.trim_end_matches('/')),
        // Responses protocol: OpenAI official uses {base}/responses (base
        // ends in /v1); Azure's v1 data-plane surface uses the same suffix
        // with base https://<resource>.openai.azure.com/openai/v1 and the
        // api-key header instead of Bearer.
        ProviderKind::OpenaiResponses | ProviderKind::AzureResponses => {
            format!("{}/responses", p.base_url.trim_end_matches('/'))
        }
        _ => format!("{}/chat/completions", p.base_url.trim_end_matches('/')),
    }
}

pub fn endpoint_models(p: &Provider) -> String {
    match p.kind {
        ProviderKind::Anthropic => format!("{}/v1/models", p.base_url.trim_end_matches('/')),
        _ => format!("{}/models", p.base_url.trim_end_matches('/')),
    }
}

/// One-shot (non-streaming) OpenAI chat call — used by the post-turn
/// reflection and the benchmark runner. Supports the chat-completions and
/// Responses protocols; Anthropic-kind providers are rejected (their
/// /v1/messages body shape differs).
pub async fn ask_once(
    client: &reqwest::Client,
    provider: &Provider,
    model: &str,
    system: &str,
    user: &str,
    max_tokens: u32,
) -> Result<String, String> {
    if matches!(provider.kind, ProviderKind::Anthropic) {
        return Err("该 Provider 为 Anthropic 类型，暂不支持此功能所需的非流式调用".into());
    }
    let responses = matches!(
        provider.kind,
        ProviderKind::OpenaiResponses | ProviderKind::AzureResponses
    );
    let body = if responses {
        json!({
            "model": model,
            "store": false,
            "instructions": system,
            "input": [{ "type": "message", "role": "user",
                        "content": [{ "type": "input_text", "text": user }] }],
            "max_output_tokens": max_tokens,
            "temperature": 0.2
        })
    } else {
        json!({
            "model": model,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user }
            ],
            "max_tokens": max_tokens,
            "temperature": 0.2
        })
    };
    let mut req = client
        .post(endpoint_chat(provider))
        .header("Content-Type", "application/json")
        .json(&body)
        .timeout(std::time::Duration::from_secs(120));
    if !provider.api_key.trim().is_empty() {
        // auth_for covers Bearer (chat-completions + OpenAI Responses) and
        // the api-key header (Azure Responses)
        req = req.headers(auth_for(provider).headers());
    }
    let resp = req.send().await.map_err(|e| format!("请求失败: {e}"))?;
    let status = resp.status();
    let v: serde_json::Value = resp.json().await.map_err(|e| format!("响应解析失败: {e}"))?;
    if !status.is_success() {
        let msg = v
            .pointer("/error/message")
            .and_then(|m| m.as_str())
            .unwrap_or("未知错误");
        return Err(format!("接口错误（HTTP {status}）: {msg}"));
    }
    let text = if responses {
        responses_output_text(&v)
    } else {
        v.pointer("/choices/0/message/content")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string()
    };
    Ok(text)
}

/// Extract the assistant text from a non-streaming Responses payload:
/// walk `output[]` and join the output_text parts of message items.
fn responses_output_text(v: &Value) -> String {
    let mut out = String::new();
    if let Some(items) = v.get("output").and_then(|o| o.as_array()) {
        for item in items {
            if item.get("type").and_then(|t| t.as_str()) != Some("message") {
                continue;
            }
            if let Some(parts) = item.get("content").and_then(|c| c.as_array()) {
                for p in parts {
                    if p.get("type").and_then(|t| t.as_str()) == Some("output_text") {
                        if let Some(t) = p.get("text").and_then(|x| x.as_str()) {
                            out.push_str(t);
                        }
                    }
                }
            }
        }
    }
    out
}

/// Alt text for the generated-image markdown: strip markdown-breaking
/// brackets and control chars, cap length.
fn image_alt(prompt: &str) -> String {
    prompt
        .replace('[', " ")
        .replace(']', " ")
        .replace('(', " ")
        .replace(')', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(24)
        .collect()
}

/// Guess the mime of a base64 image payload from its magic prefix.
fn image_data_uri(b64: &str) -> String {
    let mime = if b64.starts_with("iVBOR") {
        "image/png"
    } else if b64.starts_with("/9j/") {
        "image/jpeg"
    } else if b64.starts_with("UklGR") {
        "image/webp"
    } else if b64.starts_with("R0lGOD") {
        "image/gif"
    } else {
        "image/png"
    };
    format!("data:{mime};base64,{b64}")
}

/// One image-generation turn on a lane: POST {base}/images/generations —
/// the OpenAI-compatible image endpoint (image-mode sessions never touch
/// chat/completions). Non-streaming: the markdown result is emitted as a
/// single delta when the request completes. `url` responses become a plain
/// markdown image; `b64_json` responses become a data URI (capped) so the
/// renderer shows them without asset-protocol setup.
pub async fn image_generate(ctx: &SendCtx<'_>, prompt: &str) -> Result<LaneOutcome, String> {
    let _ = ctx.channel.send(StreamEvent::Started {
        lane: ctx.lane,
        model: ctx.model.to_string(),
        message_id: ctx.message_id.clone(),
    });
    if ctx.stop.load(Ordering::Relaxed) {
        return Ok(LaneOutcome {
            content: String::new(),
            reasoning: String::new(),
            status: "stopped".into(),
            usage: UsageStat::default(),
            confidence: None,
            tool_calls: vec![],
        });
    }
    if matches!(ctx.provider.kind, ProviderKind::Anthropic) {
        return Err(
            "生图模式需要 OpenAI 兼容接口（POST {base_url}/images/generations）；当前 Provider 为 Anthropic 类型，没有图像生成端点".into(),
        );
    }

    let url = format!(
        "{}/images/generations",
        ctx.provider.base_url.trim_end_matches('/')
    );
    let body = serde_json::json!({
        "model": ctx.model,
        "prompt": prompt,
        "n": 1,
    });

    // image generation is slow (up to a few minutes on high-quality tiers) —
    // give it its own generous timeout instead of the chat default
    let resp = ctx
        .client
        .post(&url)
        .headers(auth_for(ctx.provider).headers())
        .json(&body)
        .timeout(std::time::Duration::from_secs(300))
        .send()
        .await
        .map_err(|e| format!("网络错误: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let snippet: String = text.chars().take(400).collect();
        return Err(format!("上游返回 {status}: {snippet}"));
    }
    let v: Value = resp.json().await.map_err(|e| format!("响应解析失败: {e}"))?;

    let item = v
        .get("data")
        .and_then(|d| d.as_array())
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(Value::Null);
    let alt = image_alt(prompt);
    let content = if let Some(u) = item.get("url").and_then(|x| x.as_str()) {
        format!("![{alt}]({u})")
    } else if let Some(b64) = item.get("b64_json").and_then(|x| x.as_str()) {
        if b64.len() > 12_000_000 {
            "图片已生成，但 base64 体积过大，无法内联展示。".to_string()
        } else {
            format!("![{alt}]({})", image_data_uri(b64))
        }
    } else if let Some(msg) = v.pointer("/error/message").and_then(|x| x.as_str()) {
        return Err(format!("生图失败: {msg}"));
    } else {
        return Err("生图响应中没有图片数据（data[0].url / b64_json 均缺失）".into());
    };

    ctx.emit_delta(&content);
    Ok(LaneOutcome {
        content,
        reasoning: String::new(),
        status: "ok".into(),
        usage: UsageStat::default(),
        confidence: None,
        tool_calls: vec![],
    })
}

pub fn build_body(
    p: &Provider,
    model: &str,
    lp: &LanePrefix,
    new_msgs: &[ChatMessage],
    system: &str,
    tools: Option<&Value>,
) -> String {
    match p.kind {
        ProviderKind::OpenaiCompatible => lp.build_openai_body_multi(model, new_msgs, tools),
        // Responses protocol: same byte-stable head design, protocol-native
        // item shapes (instructions + input items + flat tools)
        ProviderKind::OpenaiResponses | ProviderKind::AzureResponses => {
            lp.build_responses_body(model, new_msgs, tools)
        }
        ProviderKind::Anthropic => {
            // Anthropic path: Zone H is REPLAYED (P0 — this arm used to send
            // ONLY this turn's messages, wiping every earlier round from
            // Claude's context without any error). In-history system
            // messages (RollingMemo / workflow re-injection) demote to user
            // turns inside the body builder; tool-call history maps to
            // native tool_use / tool_result blocks there too.
            let mut msgs = lp.history_messages();
            msgs.extend(new_msgs.iter().cloned());
            let beh = p.behavior.get(model);
            prefix::build_anthropic_body(
                model,
                system,
                &msgs,
                beh.and_then(|b| b.max_output),
                beh.and_then(|b| b.temperature),
                p.cache_tier(),
                tools,
                lp.thinking_level(),
            )
        }
    }
}

/// Rebuild the OpenAI wire shape of a persisted tool-call batch.
pub fn tool_calls_wire_value(v: &[ToolCallWire]) -> Value {
    Value::Array(
        v.iter()
            .map(|tc| {
                serde_json::json!({
                    "id": tc.id,
                    "type": "function",
                    "function": { "name": tc.name, "arguments": tc.arguments }
                })
            })
            .collect(),
    )
}

/// Filter a persisted transcript down to the context for one lane:
/// user messages (shared) + this lane's assistant and tool records; broken
/// turns are excluded so a failed request never poisons the next context.
/// Plan-mode directive, prepended to the user message at model-context
/// assembly time (like skill bodies). Build-constant bytes: injecting here
/// (rather than editing Zone S) keeps restart rebuilds byte-identical with
/// the live Zone H, and switching modes never rewrites history.
pub const PLAN_DIRECTIVE: &str = "当前处于【规划模式】。请严格遵守：\n\
1. 只允许使用只读工具（list_dir / read_file / glob_files / grep_files）调研现状，不要尝试任何写入、修改或执行操作；\n\
2. 调研完成后输出一份实施方案：方案正文必须放在且仅放在一个 ```plan 围栏代码块中，内容包括——目标、将改动的文件清单、分步骤做法、风险与回滚方式、验证方法；\n\
3. 在用户批准方案之前，不会有任何文件被修改；批准后请严格按方案分步执行，每步说明改了什么。\n\n";

/// Sub-agent directive: the delegated task rides on the user record via the
/// workflow field ("subagent"), injected here exactly like plan/goal — so
/// live Zone-H appends and restart rebuilds stay byte-identical.
pub const SUBAGENT_DIRECTIVE: &str = "你是被父任务委派的子智能体，在独立上下文中只负责这一个子任务。请严格遵守：\n\
1. 只使用只读工具（list_dir / read_file / glob_files / grep_files）调研，你没有写入工具；\n\
2. 聚焦子任务本身，不要复述父对话的整体上下文；\n\
3. 完成后输出结构化结论（不超过 400 字）：关键发现/结论、涉及的文件与关键位置、建议父任务采取的后续动作。只输出结论本身。\n\n";

/// Goal-mode directive: only the goal + acceptance criteria are locked; the
/// agent picks its own path and reports a ```goal checklist each round.
/// Build-constant bytes, injected at model-context assembly time.
pub const GOAL_DIRECTIVE: &str = "当前处于【目标模式】。用户只锁定目标与验收标准，路径由你自主决定。请严格遵守：\n\
1. 直接使用可用工具推进目标（写入操作仍按当前权限档位的审批规则执行）；\n\
2. 每轮回复的末尾输出一个 ```goal 围栏代码块：逐条列出验收标准，已满足的标准前标 ✅，未满足的标 ⬜，并附一句当前进展说明；\n\
3. 完成审计：每条 ✅ 必须在该行内附上可核验的证据——用反引号标注具体文件路径、命令及输出摘要或测试名（如 `src/prefix.rs`、`cargo test 12 通过`），或用全角括注（…）说明证据来源；「已完成」「已实现」等空泛表述不构成证据，行内无证据标注的 ✅ 会被审计为未验证声明；测试通过、代码写完、清单打满这类辅助信号本身不能单独作为完成依据；\n\
4. 存疑即未完成：任一验收标准无法给出可核验证据时，保持 ⬜ 并继续验证或继续工作——宁可多跑一轮，不可虚标 ✅；\n\
5. 当且仅当全部验收标准都为 ✅ 时，在该 ```goal 块的最后一行单独输出 GOAL_DONE；\n\
6. 动态重规划：每轮开始时重新评估目标与环境现状——若事实、障碍或外部条件发生变化，允许修订未完成(⬜)验收标准的表述（在该行尾标注「（修订：<原因>）」），但已满足(✅)的标准不得删除或放宽；\n\
7. 若当前路径已明显无法达成目标，输出一句重规划说明后切换路径继续推进，不要在死路上反复尝试；\n\
8. 不要把你的意图、阶段性进度、已耗费的精力或一个看上去合理的最终答案当作完成证明——只有标准全部满足且有证据支撑时才输出 GOAL_DONE。\n\n";

/// Deep-reasoning directive (ToT-style pre-play): the rehearsal block
/// (candidate approaches + judge verdict) is appended to the user message
/// actually SENT this turn by the lane runtime; this directive explains the
/// mechanism at transcript-assembly time so live Zone-H appends and restart
/// rebuilds stay byte-identical (same injection mechanism as plan/goal).
pub const DEEP_DIRECTIVE: &str = "当前处于【深度推理模式】。本轮采用 Tree-of-Thoughts 式方案预演：系统已针对该任务并行生成多个候选方案并完成评审，评审结论以「[深度推理 · 方案预演]」附在本条消息之后。请严格遵守：\n\
1. 优先按选定方案推进；除非执行中发现明显更优路径，可简要说明理由后调整；\n\
2. 关键决策点先给出理由再行动。\n\n";

/// Review directive (better-harness style findings reconciliation): three
/// mutually-exclusive read-only experts pre-review in parallel and their raw
/// findings ride on the user message; the model itself acts as the Lead —
/// the only role allowed to dedupe, grade and publish the final table.
pub const REVIEW_DIRECTIVE: &str = "当前处于【审阅模式】。本轮采用三专家并行预审：正确性、安全边界、可维护性三个只读视角的原始发现以「[三专家审阅预演]」附在本条消息之后。请作为汇合评审（Lead）严格遵守：\n\
1. 你只有只读工具——先用工具核实每条发现的文件、行号与根因，再决定是否采纳，不要修改任何文件；\n\
2. 去重合并三份发现（同一位置同一根因只保留一条），按严重度排序（严重 > 主要 > 次要）；\n\
3. 输出最终发现表（Markdown 表格，列：| # | 严重度 | 后果 | 根因 | 位置 | 修复与验证 |）；位置用 `文件:行号` 反引号格式；每条「修复与验证」给出最小修复动作与验证方式；\n\
4. 无法核实的条目在严重度列标注「待核」，不要删除也不要臆断；专家列表之外你自己用只读工具新发现的同样入表；\n\
5. 没有发现时明确说明「本次审阅无发现」，不要为了凑数输出风格类噪声。\n\n";

/// Text of the deterministic synthetic user message that carries a tool
/// result's images (OpenAI tool-role content must stay a plain string, so
/// screenshots follow the tool result as an image-bearing user turn). Used
/// by BOTH the live turn and transcript rebuilds — identical bytes.
pub const TOOL_IMAGE_NOTE: &str = "（附图：上一条工具结果携带的截图，请结合它继续任务）";

/// Synthetic tool result for calls whose round was stopped/errored before
/// execution — keeps the rebuilt transcript's tool_calls paired (OpenAI-
/// compatible endpoints answer 400 to unpaired calls).
pub const UNPAIRED_TOOL_NOTE: &str = "（回合被中断，该工具调用未执行）";

/// Resolve a declarative-workflow state by definition id + state name.
/// Shared by transcript assembly (directive injection) and the lane runtime
/// (tool-surface clamping + auto-advance).
pub fn resolve_sm<'a>(
    defs: &'a [crate::config::WorkflowDef],
    def_id: &str,
    state_name: &str,
) -> Option<(&'a crate::config::WorkflowDef, &'a crate::config::SmState)> {
    defs.iter()
        .find(|d| d.id == def_id)
        .and_then(|d| d.states.iter().find(|s| s.name == state_name).map(|st| (d, st)))
}

/// Evaluate one conditional-branch predicate against a finished lane-0 turn.
/// Language: "" | "always" always match; "ok"/"error" match the turn status;
/// "contains:<text>" / "not_contains:<text>" match the turn's reply text
/// (case-insensitive); "tool_used:<name>" matches a tool invoked this turn.
/// Unknown predicates never match (fail-safe: fall through to `next`).
pub fn eval_when(when: &str, reply: &str, tools_used: &[String], status: &str) -> bool {
    let w = when.trim();
    if w.is_empty() || w == "always" {
        return true;
    }
    if let Some(rest) = w.strip_prefix("contains:") {
        return !rest.is_empty() && reply.to_lowercase().contains(&rest.to_lowercase());
    }
    if let Some(rest) = w.strip_prefix("not_contains:") {
        return rest.is_empty() || !reply.to_lowercase().contains(&rest.to_lowercase());
    }
    if let Some(rest) = w.strip_prefix("tool_used:") {
        let name = rest.trim();
        return !name.is_empty() && tools_used.iter().any(|t| t == name);
    }
    match w {
        "ok" => status == "ok",
        "error" => status == "error",
        _ => false,
    }
}

/// A boundary compaction replaces everything before `upto_ts` with a
/// summary pair — the persisted transcript itself is never touched.
/// `data_dir` locates the session attachment dir: user/tool image records
/// reference relative filenames there, which load back into wire-ready
/// image parts here (files are immutable once written, so rebuilds stay
/// byte-stable with live Zone-H appends).
pub fn transcript_for_lane(
    sf: &SessionFile,
    lane: u32,
    workflows: &[crate::config::WorkflowDef],
    data_dir: &std::path::Path,
) -> Vec<ChatMessage> {
    let mut msgs: Vec<&MessageRecord> = sf
        .messages
        .iter()
        // role="notice" records are user-facing cache warnings — display
        // only, never model context (they must not perturb Zone H bytes)
        .filter(|m| m.role == "user" || (m.lane == lane && m.role != "notice"))
        .filter(|m| m.status != "error")
        .collect();
    msgs.sort_by_key(|m| m.ts);

    // tool-call pairing inventory: an assistant record whose tool_calls lack
    // matching tool results (round stopped or crashed between the model
    // frame and tool execution — or a session file written by an older
    // build) would make OpenAI-compatible endpoints reject the whole
    // rebuilt context with 400. Missing pairs get placeholder tool
    // messages injected in-memory below; persisted records stay untouched.
    let paired: std::collections::HashSet<&str> = msgs
        .iter()
        .filter(|m| m.role == "tool")
        .filter_map(|m| m.tool_call_id.as_deref())
        .collect();
    // call ids owned by assistant records that survive the filters — a tool
    // result referencing anything else is a dangler (its assistant was
    // excluded as an errored round, while the synthetic "not executed"
    // results persisted next to it carry status ok) that would make strict
    // endpoints 400 the whole rebuilt context
    let assistant_calls: std::collections::HashSet<&str> = msgs
        .iter()
        .filter(|m| m.role == "assistant")
        .filter_map(|m| m.tool_calls.as_ref())
        .flatten()
        .map(|tc| tc.id.as_str())
        .collect();

    // resolve skill bodies once if any user message invokes skills — the
    // visible record stores names only; bodies are injected here so the
    // model still receives the full instruction on every subsequent turn
    let needs_skills = msgs.iter().any(|m| m.skill_calls.as_ref().is_some_and(|s| !s.is_empty()));
    let skill_map: HashMap<String, String> = if needs_skills {
        crate::skills::scan(sf.meta.workspace.as_deref())
            .into_iter()
            .map(|s| (s.name, s.body))
            .collect()
    } else {
        HashMap::new()
    };

    let mut out: Vec<ChatMessage> = Vec::new();
    if let Some(c) = &sf.compaction {
        if msgs.first().is_some_and(|m| m.ts < c.upto_ts) {
            out.push(ChatMessage::plain(
                "user",
                "（系统提示）此前对话已按缓存策略压缩，以下摘要包含其全部要点。请基于摘要继续当前任务。",
            ));
            out.push(ChatMessage::plain("assistant", c.summary.clone()));
        }
    }
    // placeholders for unpaired calls flush lazily: after the assistant's
    // real tool results, right before the next non-tool record (or the end)
    let mut pending_syn: Vec<ChatMessage> = Vec::new();
    for m in msgs {
        if m.role != "tool" {
            out.append(&mut pending_syn);
        }
        if sf.compaction.as_ref().is_some_and(|c| m.ts < c.upto_ts) {
            continue; // folded into the summary
        }
        if m.role == "tool" {
            if let Some(id) = &m.tool_call_id {
                if !assistant_calls.contains(id.as_str()) {
                    continue; // dangling result — its assistant never made it into the context
                }
                out.push(ChatMessage {
                    role: "tool".into(),
                    content: m.content.clone(),
                    tool_calls: None,
                    tool_call_id: Some(id.clone()),
                    images: Vec::new(),
                });
                // Tool-produced images (take_screenshot) ride in a
                // deterministic synthetic user message right after the
                // result — OpenAI tool-role content must stay a plain
                // string, and this shape is accepted by Anthropic too, so
                // live appends and restart rebuilds stay byte-identical.
                if !m.images.is_empty() {
                    let imgs = crate::sessions::load_attachments(data_dir, &sf.meta.id, &m.images);
                    if !imgs.is_empty() {
                        out.push(ChatMessage::with_images("user", TOOL_IMAGE_NOTE, imgs));
                    }
                }
            }
            continue;
        }
        let has_calls = m.tool_calls.as_ref().is_some_and(|t| !t.is_empty());
        if m.role == "assistant" && m.content.trim().is_empty() && !has_calls {
            continue;
        }
        // user message with skill invocations → prepend resolved bodies;
        // plan-mode messages → prepend the fixed plan directive the same way
        let content = match (&m.role[..], &m.skill_calls, &m.workflow) {
            ("user", Some(calls), _) if !calls.is_empty() => {
                let mut text = String::new();
                for name in calls {
                    match skill_map.get(name) {
                        Some(body) => {
                            // market-installed skills are third-party text:
                            // wrap on injection-pattern hits, same as Zone S
                            let body = crate::skills::guarded_body(name, body);
                            text.push_str(&format!("---\n[调用技能 /{name}]\n\n{body}\n---\n\n"));
                        }
                        None => {
                            text.push_str(&format!("[调用技能 /{name}]（该技能已不存在）\n\n"));
                        }
                    }
                }
                text.push_str(&m.content);
                text
            }
            ("user", _, Some(w)) if w == "plan" => format!("{PLAN_DIRECTIVE}{}", m.content),
            ("user", _, Some(w)) if w == "goal" => format!("{GOAL_DIRECTIVE}{}", m.content),
            ("user", _, Some(w)) if w == "deep" => format!("{DEEP_DIRECTIVE}{}", m.content),
            ("user", _, Some(w)) if w == "review" => format!("{REVIEW_DIRECTIVE}{}", m.content),
            ("user", _, Some(w)) if w == "subagent" => format!("{SUBAGENT_DIRECTIVE}{}", m.content),
            // declarative state machine: the gate rides as "sm:<def>:<state>"
            // on the user record; the state's directive is injected verbatim
            // ahead of the message so history and live appends stay
            // byte-identical (state text is immutable per record)
            ("user", _, Some(w)) if w.starts_with("sm:") => {
                match w[3..].split_once(':') {
                    Some((def_id, state_name)) => match resolve_sm(workflows, def_id, state_name) {
                        Some((def, st)) => format!(
                            "---\n[状态机工作流「{}」 · 当前状态：{}]\n{}\n---\n\n{}",
                            def.name, st.name, st.directive, m.content
                        ),
                        // def or state was deleted/redefined — degrade to the
                        // plain message instead of failing the whole rebuild
                        None => m.content.clone(),
                    },
                    None => m.content.clone(),
                }
            }
            _ => m.content.clone(),
        };
        out.push(ChatMessage {
            role: m.role.clone(),
            content,
            tool_calls: m
                .tool_calls
                .as_ref()
                .filter(|t| !t.is_empty())
                .map(|t| tool_calls_wire_value(t)),
            tool_call_id: None,
            // user-attached images load back from the session attachment
            // dir; every other role carries none (tool images were already
            // folded into their synthetic user message above)
            images: if m.role == "user" {
                crate::sessions::load_attachments(data_dir, &sf.meta.id, &m.images)
            } else {
                Vec::new()
            },
        });
        // pairing repair (see the inventory above): queue placeholders for
        // calls that never got a result — flushed after this assistant's
        // real tool results, keeping the natural wire order
        if let Some(calls) = m.tool_calls.as_ref().filter(|t| !t.is_empty()) {
            for tc in calls.iter().filter(|c| !paired.contains(c.id.as_str())) {
                pending_syn.push(ChatMessage {
                    role: "tool".into(),
                    content: UNPAIRED_TOOL_NOTE.to_string(),
                    tool_calls: None,
                    tool_call_id: Some(tc.id.clone()),
                    images: Vec::new(),
                });
            }
        }
    }
    out.append(&mut pending_syn);
    out
}

pub fn cost_of(usage: &UsageStat, p: &Provider, model: &str) -> Option<f64> {
    let pr = p.pricing.get(model)?;
    if pr.input_per_m <= 0.0 && pr.cached_per_m <= 0.0 && pr.output_per_m <= 0.0 {
        return None;
    }
    let input = usage.input.unwrap_or(0) as f64;
    let cached = usage.cached.unwrap_or(0) as f64;
    let out = usage.output.unwrap_or(0) as f64;
    // Bucket semantics differ: OpenAI-style usage reports cached tokens as a
    // SUBSET of prompt_tokens; Anthropic reports three DISJOINT buckets
    // (input tail / cache_write at the 1h premium / cache_read discounted).
    let cost = match p.kind {
        ProviderKind::Anthropic => {
            let write = usage.cache_write.unwrap_or(0) as f64;
            input / 1e6 * pr.input_per_m
                + write / 1e6 * write_rate_per_m(p, pr)
                + cached / 1e6 * pr.cached_per_m
                + out / 1e6 * pr.output_per_m
        }
        _ => {
            let non_cached = (input - cached).max(0.0);
            non_cached / 1e6 * pr.input_per_m
                + cached / 1e6 * pr.cached_per_m
                + out / 1e6 * pr.output_per_m
        }
    };
    Some((cost * 10000.0).round() / 10000.0)
}

/// One-shot non-streaming completion — used by boundary-compaction summaries
/// and AuxMemo whitelist calls (title / prompt enhancement: plain text in,
/// plain text out). Returns the text plus the provider-reported usage so the
/// aux ledger can bill the call.
pub struct OnceOutcome {
    pub text: String,
    pub usage: UsageStat,
}

fn parse_once_usage(kind: &ProviderKind, v: &Value) -> UsageStat {
    match kind {
        ProviderKind::Anthropic => UsageStat {
            input: v.pointer("/usage/input_tokens").and_then(|x| x.as_u64()),
            output: v.pointer("/usage/output_tokens").and_then(|x| x.as_u64()),
            cached: v.pointer("/usage/cache_read_input_tokens").and_then(|x| x.as_u64()),
            cache_write: v.pointer("/usage/cache_creation_input_tokens").and_then(|x| x.as_u64()),
        },
        // Responses usage: cached is a SUBSET of input_tokens (OpenAI style)
        ProviderKind::OpenaiResponses | ProviderKind::AzureResponses => UsageStat {
            input: v.pointer("/usage/input_tokens").and_then(|x| x.as_u64()),
            output: v.pointer("/usage/output_tokens").and_then(|x| x.as_u64()),
            cached: v.pointer("/usage/input_tokens_details/cached_tokens").and_then(|x| x.as_u64()),
            cache_write: None,
        },
        ProviderKind::OpenaiCompatible => UsageStat {
            input: v.pointer("/usage/prompt_tokens").and_then(|x| x.as_u64()),
            output: v.pointer("/usage/completion_tokens").and_then(|x| x.as_u64()),
            cached: v.pointer("/usage/prompt_tokens_details/cached_tokens").and_then(|x| x.as_u64()),
            cache_write: None, // writes are unreported and billed at input rate
        },
    }
}

/// Wire body for one-shot non-streaming calls (boundary-compaction
/// summaries, AuxMemo titles / prompt enhancement). Deliberately carries NO
/// cache_control markers and no prompt_cache_key (pi compaction discipline:
/// "summaries are standalone requests — avoid cache writes that cannot be
/// reused"): the summary prefix is never replayed, so on Anthropic a marker
/// would pay the 1h write premium (2× input) for a cache entry nothing will
/// ever read; on OpenAI-style providers automatic caching stays free either
/// way, so nothing is lost by not pinning routing either.
pub fn build_once_body(kind: &ProviderKind, system: &str, user_text: &str) -> Value {
    match kind {
        ProviderKind::Anthropic => serde_json::json!({
            "model": "",
            "max_tokens": 1024,
            "system": system,
            "messages": [{"role": "user", "content": user_text}],
        }),
        ProviderKind::OpenaiResponses | ProviderKind::AzureResponses => serde_json::json!({
            "model": "",
            "stream": false,
            "store": false,
            "instructions": system,
            "input": [{"type": "message", "role": "user",
                       "content": [{"type": "input_text", "text": user_text}]}],
        }),
        ProviderKind::OpenaiCompatible => serde_json::json!({
            "model": "",
            "stream": false,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user_text}
            ],
        }),
    }
}

pub async fn complete_once(
    client: &reqwest::Client,
    provider: &Provider,
    model: &str,
    system: &str,
    user_text: &str,
) -> Result<OnceOutcome, String> {
    let mut body = build_once_body(&provider.kind, system, user_text);
    body["model"] = serde_json::Value::String(model.to_string());
    let resp = client
        .post(endpoint_chat(provider))
        .header("Content-Type", "application/json")
        .headers(auth_for(provider).headers())
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("网络错误: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let snippet: String = text.chars().take(300).collect();
        return Err(format!("上游返回 {status}: {snippet}"));
    }
    let v: Value = resp.json().await.map_err(|e| format!("响应不是 JSON: {e}"))?;
    let text = match provider.kind {
        ProviderKind::Anthropic => v
            .pointer("/content/0/text")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        ProviderKind::OpenaiResponses | ProviderKind::AzureResponses => responses_output_text(&v),
        ProviderKind::OpenaiCompatible => v
            .pointer("/choices/0/message/content")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
    };
    if text.trim().is_empty() {
        return Err("上游返回了空内容".into());
    }
    let usage = parse_once_usage(&provider.kind, &v);
    Ok(OnceOutcome { text, usage })
}

pub async fn fetch_models_async(client: &reqwest::Client, p: &Provider) -> Result<Vec<String>, String> {
    let url = endpoint_models(p);
    let resp = client
        .get(&url)
        .headers(auth_for(p).headers())
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| format!("网络错误: {e}"))?;
    if !resp.status().is_success() {
        let code = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let snippet: String = text.chars().take(300).collect();
        return Err(format!("上游返回 {code}: {snippet}"));
    }
    let v: Value = resp.json().await.map_err(|e| format!("响应不是 JSON: {e}"))?;
    let mut models = Vec::new();
    if let Some(arr) = v.get("data").and_then(|d| d.as_array()) {
        for m in arr {
            if let Some(id) = m.get("id").and_then(|x| x.as_str()) {
                models.push(id.to_string());
            }
        }
    }
    models.sort();
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_provider(kind: ProviderKind) -> Provider {
        Provider {
            id: "p".into(),
            name: "P".into(),
            kind,
            base_url: "https://example.invalid".into(),
            api_key: "k".into(),
            models: vec![],
            enabled: true,
            allow_local: false,
            context_window: None,
            pricing: {
                let mut m = std::collections::BTreeMap::new();
                m.insert(
                    "m".to_string(),
                    crate::config::Pricing { input_per_m: 3.0, cached_per_m: 0.3, output_per_m: 15.0 },
                );
                m
            },
            behavior: std::collections::BTreeMap::new(),
            cache_tier: None,
            cache_retention_24h: None,
            images_via_files: false,
        }
    }

    fn usage(input: Option<u64>, cached: Option<u64>, cache_write: Option<u64>) -> UsageStat {
        UsageStat { input, output: Some(100), cached, cache_write }
    }

    // --- Anthropic streaming tool_use (input_json_delta / partial_json)

    #[test]
    fn anthropic_tool_stream_assembles_partial_json() {
        let mut accs: Vec<ToolCallAcc> = Vec::new();
        let mut blocks: std::collections::HashMap<u64, usize> = std::collections::HashMap::new();
        let frame = |partial: &str, idx: u64| {
            serde_json::json!({
                "index": idx,
                "delta": { "type": "input_json_delta", "partial_json": partial }
            })
        };
        anthropic_tool_start(
            &serde_json::json!({
                "index": 1,
                "content_block": { "type": "tool_use", "id": "toolu_1", "name": "read_file" }
            }),
            &mut accs,
            &mut blocks,
        );
        // {"path":  +  "a.rs"  +  }  →  分片三段拼出完整参数 JSON
        for part in ["{\"path\":", "\"a.rs\"", "}"] {
            let f = frame(part, 1);
            anthropic_tool_delta(&f, Some(f.get("delta").unwrap()), &mut accs, &mut blocks);
        }
        assert_eq!(accs.len(), 1);
        assert_eq!(accs[0].name, "read_file");
        assert_eq!(accs[0].id, "toolu_1");
        assert_eq!(accs[0].args, r#"{"path":"a.rs"}"#);
    }

    #[test]
    fn anthropic_body_carries_system_updates_and_tools() {
        let p = mk_provider(ProviderKind::Anthropic);
        // in-history system 消息（RollingMemo 注入）必须降级为 user 出现，
        // 而不是被丢弃；tools 必须转换注入
        let lp = LanePrefix::new("sys", "k");
        let new_msgs = vec![
            ChatMessage::plain("system", "滚动记忆：用户偏好深色主题"),
            ChatMessage::plain("user", "继续"),
        ];
        let tools = serde_json::json!([
            { "type": "function", "function": { "name": "read_file", "parameters": { "type": "object" } } }
        ]);
        let body = build_body(
            &p,
            "claude-x",
            &lp,
            &new_msgs,
            "sys",
            Some(&tools),
        );
        assert!(body.contains("[系统更新]"), "system update must survive: {body}");
        assert!(body.contains("滚动记忆"), "{body}");
        assert!(body.contains("\"tools\""), "{body}");
        assert!(body.contains("\"input_schema\""), "{body}");
        assert!(!body.contains("\"role\":\"system\""), "no system role in messages: {body}");
    }

    // --- item: one-shot bodies carry no cache marks (pi compaction discipline)

    #[test]
    fn once_body_has_no_cache_marks() {
        for kind in [
            ProviderKind::Anthropic,
            ProviderKind::OpenaiCompatible,
            ProviderKind::OpenaiResponses,
            ProviderKind::AzureResponses,
        ] {
            let body = build_once_body(&kind, "sys", "usr").to_string();
            assert!(!body.contains("cache_control"), "{kind:?}: {body}");
            assert!(!body.contains("prompt_cache"), "{kind:?}: {body}");
        }
        // Anthropic system stays a plain string (not the marked array form)
        let a = build_once_body(&ProviderKind::Anthropic, "sys", "usr");
        assert_eq!(a["system"], serde_json::json!("sys"));
        // Responses once body uses instructions + typed input items
        let r = build_once_body(&ProviderKind::OpenaiResponses, "sys", "usr");
        assert_eq!(r["instructions"], serde_json::json!("sys"));
        assert_eq!(r["store"], serde_json::json!(false));
        assert_eq!(r["input"][0]["content"][0]["type"], "input_text");
    }

    // --- item: bucket semantics differ per provider (pi cache-stats parity)

    #[test]
    fn analyze_cache_miss_openai_cached_is_subset() {
        let p = mk_provider(ProviderKind::OpenaiCompatible);
        // OpenAI: cached ⊆ prompt_tokens ⇒ uncached = 100k − 5k
        let u = usage(Some(100_000), Some(5_000), None);
        let m = analyze_cache_miss(&u, &p.kind, 400, true, false);
        assert!(m.significant);
        assert_eq!(m.rebilled_tokens, 95_000 - 100);
    }

    #[test]
    fn analyze_cache_miss_anthropic_buckets_are_disjoint() {
        let p = mk_provider(ProviderKind::Anthropic);
        // Anthropic disjoint buckets: input tail is NOT part of cache_read.
        // input=2k, read=98k ⇒ uncached is 2k, NOT 0 (saturating_sub bug).
        let u = usage(Some(2_000), Some(98_000), Some(0));
        let m = analyze_cache_miss(&u, &p.kind, 8_000, true, false);
        assert!(!m.significant, "small tail is a legitimate re-bill: {m:?}");

        // a 12k tail beyond the expected new material is a real miss
        let u = usage(Some(12_000), Some(98_000), Some(0));
        let m = analyze_cache_miss(&u, &p.kind, 400, true, false);
        assert!(m.significant);
        assert_eq!(m.rebilled_tokens, 12_000 - 100);

        // a large cache_write mid-session means the prefix was re-written
        let u = usage(Some(2_000), Some(48_000), Some(50_000));
        let m = analyze_cache_miss(&u, &p.kind, 400, true, false);
        assert!(m.significant);
    }

    // --- item: re-bill cost derives the paid rate from the request's buckets

    #[test]
    fn rebill_cost_anthropic_uses_write_premium() {
        let p = mk_provider(ProviderKind::Anthropic);
        // paid rate = (2k×3.0 + 98k×6.0)/100k = 5.94 $/M ⇒ spread 5.64
        let u = usage(Some(2_000), Some(0), Some(98_000));
        let cost = rebill_cost(100_000, &u, &p, "m").unwrap();
        assert!((cost - 0.564).abs() < 1e-9, "{cost}");
    }

    #[test]
    fn rebill_cost_openai_plain_input_spread() {
        let p = mk_provider(ProviderKind::OpenaiCompatible);
        let u = usage(Some(100_000), Some(5_000), None);
        // spread 2.7 $/M ⇒ 94_900 tokens = 0.25623 → round4 = 0.2562
        let cost = rebill_cost(94_900, &u, &p, "m").unwrap();
        assert!((cost - 0.2562).abs() < 1e-9, "{cost}");
    }

    #[test]
    fn cost_of_anthropic_disjoint_buckets() {
        let p = mk_provider(ProviderKind::Anthropic);
        // 2k×3.0 + 0×6.0 + 98k×0.3 + 100×15.0 per M = 0.0369
        let u = usage(Some(2_000), Some(98_000), Some(0));
        let cost = cost_of(&u, &p, "m").unwrap();
        assert!((cost - 0.0369).abs() < 1e-9, "{cost}");
    }

    #[test]
    fn cost_of_openai_cached_subset() {
        let p = mk_provider(ProviderKind::OpenaiCompatible);
        // (100k−5k)×3.0 + 5k×0.3 + 100×15.0 per M = 0.288
        let u = usage(Some(100_000), Some(5_000), None);
        let cost = cost_of(&u, &p, "m").unwrap();
        assert!((cost - 0.288).abs() < 1e-9, "{cost}");
    }

    // --- item: affinity header rides on every provider kind

    #[test]
    fn affinity_header_map_present_when_set() {
        use reqwest::header::HeaderName;
        let hm = affinity_header_map(Some("ccharness-s1-0"));
        assert_eq!(
            hm.get(HeaderName::from_static("x-session-affinity")).and_then(|v| v.to_str().ok()),
            Some("ccharness-s1-0")
        );
        assert!(affinity_header_map(None).is_empty());
    }

    // --- item: OpenAI Responses / Azure Responses adapter (pi matrix)

    #[test]
    fn responses_auth_and_endpoints() {
        use reqwest::header::HeaderName;
        // OpenAI Responses: Bearer + {base}/responses
        let p = mk_provider(ProviderKind::OpenaiResponses);
        assert_eq!(endpoint_chat(&p), "https://example.invalid/responses");
        assert_eq!(endpoint_models(&p), "https://example.invalid/models");
        let hm = auth_for(&p).headers();
        assert!(
            hm.get(reqwest::header::AUTHORIZATION).is_some(),
            "OpenAI Responses authenticates with Bearer"
        );

        // Azure Responses: api-key header + same endpoint suffix
        let mut az = mk_provider(ProviderKind::AzureResponses);
        az.base_url = "https://res.openai.azure.com/openai/v1".into();
        assert_eq!(endpoint_chat(&az), "https://res.openai.azure.com/openai/v1/responses");
        let hm = auth_for(&az).headers();
        assert_eq!(
            hm.get(HeaderName::from_static("api-key")).and_then(|v| v.to_str().ok()),
            Some("k")
        );
        assert!(hm.get(reqwest::header::AUTHORIZATION).is_none());
    }

    #[test]
    fn responses_output_text_walks_items() {
        let v = serde_json::json!({
            "output": [
                { "type": "reasoning", "summary": [] },
                { "type": "message", "role": "assistant",
                  "content": [
                      { "type": "output_text", "text": "第一段" },
                      { "type": "output_text", "text": "第二段" }
                  ] }
            ]
        });
        assert_eq!(responses_output_text(&v), "第一段第二段");
        // no message items → empty (caller reports 空内容)
        assert_eq!(responses_output_text(&serde_json::json!({"output": []})), "");
    }

    #[test]
    fn responses_frame_parses_deltas_usage_and_tools() {
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut usage = UsageStat::default();
        let mut cfilter = crate::confidence::ConfidenceFilter::new();
        let mut tool_accs: Vec<ToolCallAcc> = Vec::new();
        let mut finish: Option<String> = None;
        let apply = |v: Value,
                         content: &mut String,
                         reasoning: &mut String,
                         usage: &mut UsageStat,
                         cfilter: &mut crate::confidence::ConfidenceFilter,
                         tool_accs: &mut Vec<ToolCallAcc>,
                         finish: &mut Option<String>| {
            apply_responses_event(&v, content, reasoning, usage, cfilter, tool_accs, finish)
        };

        // text + reasoning deltas stream into the accumulators
        let (t, r) = apply(
            serde_json::json!({"type":"response.output_text.delta","delta":"你好"}),
            &mut content, &mut reasoning, &mut usage, &mut cfilter, &mut tool_accs, &mut finish,
        );
        assert_eq!((t, r), ("你好".to_string(), String::new()));
        let (t, r) = apply(
            serde_json::json!({"type":"response.reasoning_summary_text.delta","delta":"思考"}),
            &mut content, &mut reasoning, &mut usage, &mut cfilter, &mut tool_accs, &mut finish,
        );
        assert_eq!((t, r), (String::new(), "思考".to_string()));
        assert_eq!(content, "你好");
        assert_eq!(reasoning, "思考");

        // a whole function_call item lands in the accumulator
        apply(
            serde_json::json!({"type":"response.output_item.done","item":{
                "type":"function_call","call_id":"call_9","name":"read_file",
                "arguments":"{\"path\":\"a.rs\"}"}}),
            &mut content, &mut reasoning, &mut usage, &mut cfilter, &mut tool_accs, &mut finish,
        );
        assert_eq!(tool_accs.len(), 1);
        assert_eq!((tool_accs[0].id.as_str(), tool_accs[0].name.as_str()), ("call_9", "read_file"));

        // terminal event: usage (cached ⊆ input) + finish_reason tool_calls
        apply(
            serde_json::json!({"type":"response.completed","response":{"usage":{
                "input_tokens":100000,"output_tokens":500,
                "input_tokens_details":{"cached_tokens":90000}}}}),
            &mut content, &mut reasoning, &mut usage, &mut cfilter, &mut tool_accs, &mut finish,
        );
        assert_eq!(usage.input, Some(100_000));
        assert_eq!(usage.cached, Some(90_000));
        assert_eq!(usage.output, Some(500));
        assert_eq!(finish.as_deref(), Some("tool_calls"));

        // without tool calls the same terminal event finishes as stop
        let mut tool_accs2: Vec<ToolCallAcc> = Vec::new();
        let mut finish2: Option<String> = None;
        apply(
            serde_json::json!({"type":"response.completed","response":{"usage":{}}}),
            &mut content, &mut reasoning, &mut usage, &mut cfilter, &mut tool_accs2, &mut finish2,
        );
        assert_eq!(finish2.as_deref(), Some("stop"));

        // unknown events are ignored
        let (t, r) = apply(
            serde_json::json!({"type":"response.created","response":{}}),
            &mut content, &mut reasoning, &mut usage, &mut cfilter, &mut tool_accs, &mut finish,
        );
        assert_eq!((t, r), (String::new(), String::new()));
    }

    // --- item: unpaired tool_calls are healed in the rebuilt transcript (C1)

    fn transcript_sf(records: serde_json::Value) -> SessionFile {
        serde_json::from_value(serde_json::json!({
            "meta": { "id": "s", "title": "t", "kind": "chat",
                      "created_at": 0, "updated_at": 0, "bindings": [] },
            "messages": records
        }))
        .unwrap()
    }

    #[test]
    fn transcript_repairs_unpaired_tool_calls() {
        let user = serde_json::json!({
            "id": "u", "lane": 0, "role": "user", "reasoning": null,
            "content": "go", "ts": 1, "model": null, "status": "ok",
            "usage": null, "cost_usd": null, "confidence": null,
            "tool_calls": null, "tool_call_id": null, "skill_calls": null,
            "workflow": null, "images": []
        });
        let asst = serde_json::json!({
            "id": "a", "lane": 0, "role": "assistant", "reasoning": null,
            "content": "calling tools", "ts": 2, "model": "m", "status": "ok",
            "usage": null, "cost_usd": null, "confidence": null,
            "tool_calls": [
                { "id": "call-1", "name": "read_file", "arguments": "{}" },
                { "id": "call-2", "name": "list_dir", "arguments": "{}" }
            ],
            "tool_call_id": null, "skill_calls": null, "workflow": null,
            "images": []
        });
        let tool1 = serde_json::json!({
            "id": "t1", "lane": 0, "role": "tool", "reasoning": null,
            "content": "done", "ts": 3, "model": null, "status": "ok",
            "usage": null, "cost_usd": null, "confidence": null,
            "tool_calls": null, "tool_call_id": "call-1",
            "skill_calls": null, "workflow": null, "images": []
        });

        // call-2 never got a result (round stopped mid-tools) → healed
        let sf = transcript_sf(serde_json::json!([user, asst, tool1]));
        let out = transcript_for_lane(&sf, 0, &[], std::path::Path::new("."));
        assert_eq!(out.len(), 4, "{out:?}");
        assert_eq!(out[2].tool_call_id.as_deref(), Some("call-1"));
        assert_eq!(out[3].role, "tool");
        assert_eq!(out[3].tool_call_id.as_deref(), Some("call-2"));
        assert_eq!(out[3].content, UNPAIRED_TOOL_NOTE);

        // fully paired history stays untouched — no synthetic tail
        let tool2 = serde_json::json!({
            "id": "t2", "lane": 0, "role": "tool", "reasoning": null,
            "content": "listed", "ts": 4, "model": null, "status": "ok",
            "usage": null, "cost_usd": null, "confidence": null,
            "tool_calls": null, "tool_call_id": "call-2",
            "skill_calls": null, "workflow": null, "images": []
        });
        let sf2 = transcript_sf(serde_json::json!([user, asst, tool1, tool2]));
        let out2 = transcript_for_lane(&sf2, 0, &[], std::path::Path::new("."));
        assert_eq!(out2.len(), 4);
        assert!(out2.iter().all(|m| m.content != UNPAIRED_TOOL_NOTE));
    }
}
