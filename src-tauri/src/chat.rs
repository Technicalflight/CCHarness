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

use crate::config::{Provider, ProviderKind};
use crate::prefix::{self, ChatMessage, LanePrefix};
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

    use futures_util::StreamExt;
    loop {
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
                match ctx.provider.kind {
                    ProviderKind::OpenaiCompatible => handle_openai_frame(
                        ctx,
                        &v,
                        &mut content,
                        &mut reasoning,
                        &mut usage,
                        &mut cfilter,
                        &mut tool_accs,
                        &mut finish_reason,
                    ),
                    ProviderKind::Anthropic => {
                        handle_anthropic_frame(ctx, &v, &mut content, &mut reasoning, &mut usage, &mut cfilter)
                    }
                }
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
    let wants_tools =
        finish_reason.as_deref() == Some("tool_calls") && !tool_calls.is_empty();

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
    buf.windows(2).position(|w| w == b"\n\n").map(|p| p + 2)
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
) {
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
        if let Some(t) = d.get("reasoning_content").and_then(|x| x.as_str()) {
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
        // streamed tool-call fragments, indexed assembly
        if let Some(arr) = d.get("tool_calls").and_then(|x| x.as_array()) {
            for tc in arr {
                let idx = tc.get("index").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
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
}

fn handle_anthropic_frame(
    ctx: &SendCtx<'_>,
    v: &Value,
    content: &mut String,
    reasoning: &mut String,
    usage: &mut UsageStat,
    cfilter: &mut crate::confidence::ConfidenceFilter,
) {
    match v.get("type").and_then(|x| x.as_str()) {
        Some("message_start") => {
            if let Some(inp) = v.pointer("/message/usage/input_tokens").and_then(|x| x.as_u64()) {
                usage.input = Some(inp);
            }
            if let Some(c) = v.pointer("/message/usage/cache_read_input_tokens").and_then(|x| x.as_u64()) {
                usage.cached = Some(c);
            }
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
                Some(Some("text_delta")) | Some(Some("input_json_delta")) | None => {
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
            if let Some(out) = v.pointer("/usage/output_tokens").and_then(|x| x.as_u64()) {
                usage.output = Some(out);
            }
        }
        _ => {}
    }
}

pub enum AuthHeader {
    Bearer(String),
    Anthropic { key: String },
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
        }
        hm
    }
}

pub fn auth_for(p: &Provider) -> AuthHeader {
    match p.kind {
        ProviderKind::Anthropic => AuthHeader::Anthropic { key: p.api_key.clone() },
        _ => AuthHeader::Bearer(p.api_key.clone()),
    }
}

pub fn endpoint_chat(p: &Provider) -> String {
    match p.kind {
        ProviderKind::Anthropic => format!("{}/v1/messages", p.base_url.trim_end_matches('/')),
        _ => format!("{}/chat/completions", p.base_url.trim_end_matches('/')),
    }
}

pub fn endpoint_models(p: &Provider) -> String {
    match p.kind {
        ProviderKind::Anthropic => format!("{}/v1/models", p.base_url.trim_end_matches('/')),
        _ => format!("{}/models", p.base_url.trim_end_matches('/')),
    }
}

/// One-shot (non-streaming) OpenAI-compatible chat call — used by the
/// post-turn reflection and the benchmark runner. Anthropic-kind providers
/// are rejected (their /v1/messages body shape differs).
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
    let body = json!({
        "model": model,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": user }
        ],
        "max_tokens": max_tokens,
        "temperature": 0.2
    });
    let mut req = client
        .post(endpoint_chat(provider))
        .json(&body)
        .timeout(std::time::Duration::from_secs(120));
    if !provider.api_key.trim().is_empty() {
        req = req.bearer_auth(provider.api_key.trim());
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
    Ok(v.pointer("/choices/0/message/content")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string())
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

/// Merge consecutive same-role messages (Anthropic requires alternation).
fn merge_alternating(msgs: Vec<ChatMessage>) -> Vec<ChatMessage> {
    let mut out: Vec<ChatMessage> = Vec::new();
    for m in msgs {
        if let Some(last) = out.last_mut() {
            if last.role == m.role && m.role == "user" {
                last.content.push_str("\n\n");
                last.content.push_str(&m.content);
                continue;
            }
        }
        out.push(m);
    }
    out
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
        ProviderKind::Anthropic => {
            // Anthropic path: tool schema not injected in v1; the request keeps
            // its legacy shape (top-level system + merged roles). new_msgs
            // collapse to a single trailing user turn for compat.
            let mut msgs: Vec<ChatMessage> = Vec::new();
            for m in new_msgs {
                if m.role == "user" || m.role == "assistant" {
                    msgs.push(m.clone());
                }
            }
            let merged = merge_alternating(msgs);
            let beh = p.behavior.get(model);
            prefix::build_anthropic_body(
                model,
                system,
                &merged,
                beh.and_then(|b| b.max_output),
                beh.and_then(|b| b.temperature),
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
        .filter(|m| m.role == "user" || m.lane == lane)
        .filter(|m| m.status != "error")
        .collect();
    msgs.sort_by_key(|m| m.ts);

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
    for m in msgs {
        if sf.compaction.as_ref().is_some_and(|c| m.ts < c.upto_ts) {
            continue; // folded into the summary
        }
        if m.role == "tool" {
            if let Some(id) = &m.tool_call_id {
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
    }
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
    let non_cached = (input - cached).max(0.0);
    let cost = non_cached / 1e6 * pr.input_per_m
        + cached / 1e6 * pr.cached_per_m
        + out / 1e6 * pr.output_per_m;
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
        },
        ProviderKind::OpenaiCompatible => UsageStat {
            input: v.pointer("/usage/prompt_tokens").and_then(|x| x.as_u64()),
            output: v.pointer("/usage/completion_tokens").and_then(|x| x.as_u64()),
            cached: v.pointer("/usage/prompt_tokens_details/cached_tokens").and_then(|x| x.as_u64()),
        },
    }
}

pub async fn complete_once(
    client: &reqwest::Client,
    provider: &Provider,
    model: &str,
    system: &str,
    user_text: &str,
) -> Result<OnceOutcome, String> {
    let (url, body) = match provider.kind {
        ProviderKind::Anthropic => (
            format!("{}/v1/messages", provider.base_url.trim_end_matches('/')),
            serde_json::json!({
                "model": model,
                "max_tokens": 1024,
                "system": system,
                "messages": [{"role": "user", "content": user_text}],
            }),
        ),
        ProviderKind::OpenaiCompatible => (
            format!("{}/chat/completions", provider.base_url.trim_end_matches('/')),
            serde_json::json!({
                "model": model,
                "stream": false,
                "messages": [
                    {"role": "system", "content": system},
                    {"role": "user", "content": user_text}
                ],
            }),
        ),
    };
    let resp = client
        .post(&url)
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
