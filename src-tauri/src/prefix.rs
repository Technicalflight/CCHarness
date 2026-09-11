// Prefix state machine — the core of the cache-hit design (see
// docs/design/cache-hit-mechanism.md). Three byte zones per request:
//
//   Zone S  frozen system prompt      (stable for the whole epoch)
//   Zone H  append-only message bytes (never rewritten, stored as
//                                     pre-serialized JSON strings so a
//                                     rebuild cannot change one byte)
//   Zone T  tail: the new user turn   (falls into H once answered)
//
// Every request body is assembled by byte concatenation of pre-serialized
// fragments, which guarantees the provider sees an identical byte prefix
// across turns — the precondition for provider-side prefix caching.
//
// An epoch counter marks expected cache rebuilds (model switch, first turn,
// manual reset) so telemetry can separate "expected miss" from "regression".

use crate::config::CacheTier;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest as ShaDigest, Sha256};

/// One image attached to a message: mime type + base64 payload (no data-URI
/// prefix). Wire encoding happens at serialization time (protocol-specific).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatImage {
    pub mime: String,
    pub b64: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    /// Assistant tool-call batch, OpenAI wire shape. serde_json Value uses
    /// sorted keys by default, so serialization stays deterministic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<serde_json::Value>,
    /// Set on role="tool" result messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Images attached to this message (user attachments / tool screenshots).
    /// Custom Serialize below controls the wire shape: user roles emit the
    /// images as OpenAI content parts; every other role strips them (tool
    /// images ride in a deterministic synthetic user message that follows
    /// the tool result — OpenAI tool content must stay a plain string).
    #[serde(default)]
    pub images: Vec<ChatImage>,
}

impl ChatMessage {
    pub fn plain(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
            tool_calls: None,
            tool_call_id: None,
            images: Vec::new(),
        }
    }

    pub fn with_images(role: &str, content: impl Into<String>, images: Vec<ChatImage>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
            tool_calls: None,
            tool_call_id: None,
            images,
        }
    }

    /// OpenAI multimodal content parts: optional text, then one image_url
    /// part per attached image (data URI). Deterministic ordering.
    fn openai_parts(&self) -> Vec<serde_json::Value> {
        let mut parts = Vec::new();
        if !self.content.is_empty() {
            parts.push(json!({ "type": "text", "text": self.content }));
        }
        for img in &self.images {
            parts.push(json!({
                "type": "image_url",
                "image_url": { "url": format!("data:{};base64,{}", img.mime, img.b64) }
            }));
        }
        parts
    }
}

impl Serialize for ChatMessage {
    /// Legacy byte shape when no images are attached (field order = role,
    /// content, optional tool_calls / tool_call_id — exactly what this app
    /// has always sent, so plain messages stay byte-identical); the
    /// multimodal shape swaps the string content for an OpenAI parts array
    /// on user roles only. Images on non-user roles never serialize.
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let multimodal = self.role == "user" && !self.images.is_empty();
        let mut n = 2usize;
        if self.tool_calls.is_some() {
            n += 1;
        }
        if self.tool_call_id.is_some() {
            n += 1;
        }
        let mut st = s.serialize_struct("ChatMessage", n)?;
        st.serialize_field("role", &self.role)?;
        if multimodal {
            st.serialize_field("content", &self.openai_parts())?;
        } else {
            st.serialize_field("content", &self.content)?;
        }
        if let Some(tc) = &self.tool_calls {
            st.serialize_field("tool_calls", tc)?;
        }
        if let Some(id) = &self.tool_call_id {
            st.serialize_field("tool_call_id", id)?;
        }
        st.end()
    }
}

/// Serialize one message to its exact request-bytes. Field order is the
/// struct declaration order — deterministic across process restarts — and
/// absent optional fields emit nothing, so plain messages keep the exact
/// bytes this app has always sent.
pub fn message_json(m: &ChatMessage) -> String {
    serde_json::to_string(m).expect("message serialize cannot fail")
}

#[derive(Clone)]
pub struct LanePrefix {
    /// Zone S bytes: the serialized system message ("" if none configured).
    system_json: String,
    /// Zone H: JSON string per message, appended only.
    history: Vec<String>,
    /// Running digest chain state.
    hasher: Sha256,
    /// Latest chain digest (hex).
    digest: String,
    /// Total Zone S+H byte length.
    prefix_len: usize,
    /// Cache epoch; bumped on rebuild events.
    pub epoch: u32,
    /// Model this prefix is bound to; change ⇒ epoch bump.
    pub model: Option<String>,
    /// Stable per-lane cache routing key (prompt_cache_key). Constant for the
    /// lane's lifetime so load-balanced gateways keep affinity to one backend.
    cache_key: String,
    /// Reasoning effort injected into the head; change ⇒ epoch bump (the
    /// request head bytes change, so the provider cache restarts).
    thinking: Option<String>,
    /// Sampling temperature injected into the head; None = omit. Change ⇒
    /// epoch bump (same head-bytes reasoning as `thinking`).
    temperature: Option<f64>,
    /// Output cap (`max_tokens`) injected into the head; None = omit
    /// (Anthropic callers fall back to their required default). Change ⇒
    /// epoch bump.
    max_output: Option<u32>,
    /// Privacy-mode flag this prefix's Zone H was scrubbed under. A change
    /// means the rebuilt transcript bytes differ ⇒ caller rebuilds (and the
    /// rebuild bumps the epoch).
    pub privacy: bool,
    /// Raw Zone S text (Responses path rides it as top-level `instructions`;
    /// the OpenAI/Anthropic paths use the serialized `system_json` form).
    system_text: String,
    /// Cache TTL tier riding in the byte-stable head: it decides whether
    /// `prompt_cache_key` is emitted (Short/Long) or omitted (None) and
    /// whether `prompt_cache_retention:"24h"` rides along (Long only). A
    /// toggle changes head bytes ⇒ epoch bump (same discipline as
    /// bind_behavior).
    cache_tier: CacheTier,
    /// Hash of the tool-schema loadout sent in the head. A loadout change
    /// (MCP server added/removed mid-session) silently rewrites the head —
    /// tracking it keeps the epoch honest about that rebuild.
    tools_hash: Option<u64>,
}

impl LanePrefix {
    pub fn new(system_prompt: &str, cache_key: &str) -> Self {
        let mut lp = Self {
            system_json: String::new(),
            history: Vec::new(),
            hasher: Sha256::new(),
            digest: String::new(),
            prefix_len: 0,
            epoch: 0,
            model: None,
            cache_key: cache_key.to_string(),
            thinking: None,
            temperature: None,
            max_output: None,
            privacy: false,
            system_text: system_prompt.to_string(),
            cache_tier: CacheTier::Short,
            tools_hash: None,
        };
        if !system_prompt.is_empty() {
            let sys = message_json(&ChatMessage::plain("system", system_prompt));
            lp.system_json = sys;
            // Zone S participates in the digest chain
            lp.hasher.update(lp.system_json.as_bytes());
            lp.prefix_len += lp.system_json.len();
        }
        lp.digest = hex::encode(lp.hasher.clone().finalize());
        lp
    }

    /// Rebind the privacy flag. Returns true when it changed — the caller
    /// must rebuild Zone H from the (re-scrubbed) transcript; the rebuild
    /// bumps the epoch.
    pub fn bind_privacy(&mut self, on: bool) -> bool {
        if self.privacy == on {
            return false;
        }
        self.privacy = on;
        true
    }

    /// Rebind to a model. A model change rebuilds Zone H (re-serializing the
    /// same transcript) and bumps the epoch — an expected, logged rebuild.
    pub fn bind_model(&mut self, model: &str) {
        if self.model.as_deref() == Some(model) {
            return;
        }
        if self.model.is_some() {
            self.epoch += 1;
        }
        self.model = Some(model.to_string());
    }

    /// Append one message's bytes to Zone H. Called *after* a turn completes
    /// so the next request starts with it in the stable prefix.
    pub fn append(&mut self, m: &ChatMessage) {
        let bytes = message_json(m);
        self.hasher.update(bytes.as_bytes());
        self.prefix_len += bytes.len() + 1; // +1 comma separator
        self.history.push(bytes);
        self.digest = hex::encode(self.hasher.clone().finalize());
    }

    /// Rebuild Zone H from a transcript (epoch reset path). Always bumps the
    /// epoch: a rebuilt prefix is a fresh cache identity even when the bytes
    /// are identical.
    pub fn rebuild(&mut self, system_prompt: &str, messages: &[ChatMessage]) {
        let cache_key = self.cache_key.clone();
        let fresh = Self::new(system_prompt, &cache_key);
        *self = fresh;
        for m in messages {
            self.append(m);
        }
        self.epoch += 1;
    }

    /// Current fingerprint-chain head. Part of the prefix API surface;
    /// exercised by tests and reserved for the telemetry panel.
    #[allow(dead_code)]
    pub fn digest_hex(&self) -> String {
        self.digest.clone()
    }

    #[allow(dead_code)]
    pub fn prefix_bytes(&self) -> usize {
        self.prefix_len
    }

    /// Rebind the reasoning effort. A change alters head bytes ⇒ epoch bump.
    pub fn bind_thinking(&mut self, level: Option<&str>) {
        let normalized = level.filter(|l| !l.is_empty() && *l != "default").map(|l| l.to_string());
        if self.thinking == normalized {
            return;
        }
        if self.thinking.is_some() || normalized.is_some() {
            self.epoch += 1;
        }
        self.thinking = normalized;
    }

    /// Rebind per-model sampling parameters (temperature / output cap).
    /// Same epoch discipline as bind_thinking: these ride in the byte-stable
    /// head, so any present↔absent or value change is an expected rebuild.
    pub fn bind_behavior(&mut self, temperature: Option<f64>, max_output: Option<u32>) {
        if self.temperature == temperature && self.max_output == max_output {
            return;
        }
        let had_any = self.temperature.is_some() || self.max_output.is_some();
        let has_any = temperature.is_some() || max_output.is_some();
        if had_any || has_any {
            self.epoch += 1;
        }
        self.temperature = temperature;
        self.max_output = max_output;
    }

    /// Rebind the cache TTL tier. The tier rides in the byte-stable head
    /// (prompt_cache_key presence / retention argument), so any change is an
    /// expected rebuild: epoch bump (same discipline as bind_behavior).
    pub fn bind_cache_tier(&mut self, tier: CacheTier) {
        if self.cache_tier == tier {
            return;
        }
        self.epoch += 1;
        self.cache_tier = tier;
    }

    /// Rebind the tool-schema loadout hash. MCP servers joining/leaving
    /// mid-session rewrite the head bytes (tools serialize before messages)
    /// without touching Zone H — tracking the hash keeps the epoch honest
    /// about that rebuild instead of misattributing it to the upstream.
    pub fn bind_tools_hash(&mut self, hash: Option<u64>) {
        if self.tools_hash == hash {
            return;
        }
        if self.tools_hash.is_some() || hash.is_some() {
            self.epoch += 1;
        }
        self.tools_hash = hash;
    }

    /// Whether Zone S already carries exactly this system text. A mismatch
    /// means settings/workspace/AGENTS.md changed ⇒ caller rebuilds (epoch+1).
    pub fn system_is(&self, system_prompt: &str) -> bool {
        if system_prompt.is_empty() {
            return self.system_json.is_empty();
        }
        self.system_json == message_json(&ChatMessage::plain("system", system_prompt))
    }

    /// Public accessors used by the command layer.
    pub fn prefix_bytes_public(&self) -> usize {
        self.prefix_len
    }

    pub fn history_len_public(&self) -> usize {
        self.history.len()
    }

    /// Assemble the full OpenAI-compatible request body.
    ///
    /// Layout: fixed head (model/cache key/stream options/tools schema) →
    /// messages array → tail (all messages added since the Zone H snapshot).
    /// The Zone S+H byte span inside `messages` is byte-identical across
    /// requests of one epoch; the tool schema in the head is build-stable.
    pub fn build_openai_body_multi(
        &self,
        model: &str,
        new_msgs: &[ChatMessage],
        tools: Option<&serde_json::Value>,
    ) -> String {
        let tail: usize = new_msgs.iter().map(|m| m.content.len() + 96).sum();
        let mut body = String::with_capacity(self.prefix_len + tail + 1024);
        body.push_str("{\"model\":");
        body.push_str(&serde_json::to_string(model).expect("model str"));
        // stable routing key so load-balanced gateways keep cache affinity.
        // Tier None omits it entirely — strict gateways that reject unknown
        // arguments stay safe (nothing is cached through routing then).
        if self.cache_tier != CacheTier::None && !self.cache_key.is_empty() {
            body.push_str(",\"prompt_cache_key\":");
            body.push_str(&serde_json::to_string(&self.cache_key).expect("cache key str"));
        }
        // OpenAI extended retention (Long tier): keeps cached prefixes
        // active for up to 24h instead of the ~5-10min in-memory default.
        // Supported by GPT-5.x/4.1; must stay in the byte-stable head.
        if self.cache_tier == CacheTier::Long {
            body.push_str(",\"prompt_cache_retention\":\"24h\"");
        }
        body.push_str(",\"stream\":true,\"stream_options\":{\"include_usage\":true}");
        // per-model sampling params ride in the byte-stable head too; the
        // bind_behavior epoch discipline keeps them per-epoch constant
        if let Some(t) = self.temperature {
            body.push_str(",\"temperature\":");
            body.push_str(&serde_json::to_string(&t).expect("temperature f64"));
        }
        if let Some(n) = self.max_output {
            body.push_str(",\"max_tokens\":");
            body.push_str(&n.to_string());
        }
        // reasoning effort rides in the byte-stable head; changing it bumps
        // the epoch (see bind_thinking) so this stays per-epoch constant
        if let Some(level) = &self.thinking {
            body.push_str(",\"reasoning_effort\":");
            body.push_str(&serde_json::to_string(level).expect("effort str"));
        }
        // read-only/agent tool surface (schema bytes are constant per build)
        if let Some(t) = tools {
            body.push_str(",\"tools\":");
            body.push_str(&serde_json::to_string(t).expect("tools schema"));
            body.push_str(",\"tool_choice\":\"auto\"");
        }
        body.push_str(",\"messages\":[");
        let mut first = true;
        for part in self.zone_parts() {
            if !first {
                body.push(',');
            }
            body.push_str(&part);
            first = false;
        }
        for m in new_msgs {
            if !first {
                body.push(',');
            }
            body.push_str(&message_json(m));
            first = false;
        }
        body.push_str("]}");
        body
    }

    /// Single-tail convenience kept for the byte-stability tests.
    #[allow(dead_code)]
    pub fn build_openai_body(&self, model: &str, user_msg: &ChatMessage) -> String {
        self.build_openai_body_multi(model, std::slice::from_ref(user_msg), None)
    }

    /// Assemble the OpenAI Responses request body (pi adapter matrix: the
    /// Responses protocol is its own adapter, not a chat-completions
    /// variant). Layout mirrors build_openai_body_multi: epoch-stable head
    /// (model / instructions / sampling / tools / cache routing) → `input`
    /// items = Zone S+H fragments converted 1:1 → live tail items.
    ///
    /// Server-side storage is disabled (`store:false`): context is managed
    /// client-side by the byte-prefix design, and nothing must persist
    /// provider-side. `previous_response_id` chaining is deliberately not
    /// used — every request replays the full stable prefix, which is what
    /// makes provider-side prefix caching possible at all.
    pub fn build_responses_body(
        &self,
        model: &str,
        new_msgs: &[ChatMessage],
        tools: Option<&serde_json::Value>,
    ) -> String {
        let tail: usize = new_msgs.iter().map(|m| m.content.len() + 128).sum();
        let mut body = String::with_capacity(self.prefix_len + tail + 1024);
        body.push_str("{\"model\":");
        body.push_str(&serde_json::to_string(model).expect("model str"));
        // the system prompt rides as top-level `instructions` (Responses
        // shape) — Zone S bytes in their protocol-native form
        body.push_str(",\"instructions\":");
        body.push_str(&serde_json::to_string(&self.system_text).expect("system str"));
        body.push_str(",\"stream\":true,\"store\":false");
        // per-model sampling params ride the byte-stable head (epoch-gated)
        if let Some(t) = self.temperature {
            body.push_str(",\"temperature\":");
            body.push_str(&serde_json::to_string(&t).expect("temperature f64"));
        }
        if let Some(n) = self.max_output {
            body.push_str(",\"max_output_tokens\":");
            body.push_str(&n.to_string());
        }
        // Responses nests the effort under reasoning:{effort} (chat
        // completions uses the flat reasoning_effort field)
        if let Some(level) = &self.thinking {
            body.push_str(",\"reasoning\":{\"effort\":");
            body.push_str(&serde_json::to_string(level).expect("effort str"));
            body.push('}');
        }
        // cache routing: same tier semantics as the chat-completions path
        if self.cache_tier != CacheTier::None && !self.cache_key.is_empty() {
            body.push_str(",\"prompt_cache_key\":");
            body.push_str(&serde_json::to_string(&self.cache_key).expect("cache key str"));
        }
        if self.cache_tier == CacheTier::Long {
            body.push_str(",\"prompt_cache_retention\":\"24h\"");
        }
        if let Some(t) = tools {
            body.push_str(",\"tools\":");
            body.push_str(&serde_json::to_string(&responses_tools(t)).expect("tools schema"));
            body.push_str(",\"tool_choice\":\"auto\"");
        }
        body.push_str(",\"input\":[");
        let mut first = true;
        for part in self.zone_parts() {
            // Zone fragments are stored as chat-completions message bytes;
            // converting them here is deterministic (total mapping + sorted
            // serde_json keys), so the same fragment always yields the same
            // item bytes — the Responses input array stays byte-stable.
            if let Ok(v) = serde_json::from_str::<Value>(&part) {
                for item in responses_items(&v) {
                    if !first {
                        body.push(',');
                    }
                    body.push_str(&serde_json::to_string(&item).expect("input item"));
                    first = false;
                }
            }
        }
        for m in new_msgs {
            let v = serde_json::to_value(m).expect("message value");
            for item in responses_items(&v) {
                if !first {
                    body.push(',');
                }
                body.push_str(&serde_json::to_string(&item).expect("input item"));
                first = false;
            }
        }
        body.push_str("]}");
        body
    }

    fn zone_parts(&self) -> Vec<String> {
        let mut parts = Vec::with_capacity(self.history.len() + 1);
        if !self.system_json.is_empty() {
            parts.push(self.system_json.clone());
        }
        parts.extend(self.history.iter().cloned());
        parts
    }

    /// Byte length of Zone S+H exactly as it will appear in the next request
    /// (including separators) — recorded into telemetry before the send.
    #[allow(dead_code)]
    pub fn next_request_prefix_len(&self, user_len: usize) -> (usize, usize) {
        // stable span = current prefix_len; added = user message + separators
        let added = if self.prefix_len > 0 { user_len + 1 } else { user_len };
        (self.prefix_len, added)
    }
}

/// Convert one chat-completions message value (the Zone fragment wire
/// shape) into its Responses-API input item(s). The mapping is total and
/// deterministic, so identical fragments always produce identical item
/// bytes — the precondition for the Responses `input` array staying
/// byte-stable across turns.
///
///   system    → dropped (rides as top-level `instructions`)
///   user      → {type:"message", content:[input_text | input_image …]}
///   assistant → {type:"message", content:[output_text]} + one
///               {type:"function_call", call_id, name, arguments} per call
///   tool      → {type:"function_call_output", call_id, output}
fn responses_items(m: &Value) -> Vec<Value> {
    let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
    match role {
        "system" => Vec::new(),
        "tool" => vec![json!({
            "type": "function_call_output",
            "call_id": m.get("tool_call_id").cloned().unwrap_or(Value::Null),
            "output": m.get("content").cloned().unwrap_or(Value::Null),
        })],
        "assistant" => {
            let mut items = Vec::new();
            if let Some(text) = m.get("content").and_then(|c| c.as_str()) {
                if !text.is_empty() {
                    items.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": text }],
                    }));
                }
            }
            if let Some(calls) = m.get("tool_calls").and_then(|t| t.as_array()) {
                for tc in calls {
                    items.push(json!({
                        "type": "function_call",
                        "call_id": tc.pointer("/id").cloned().unwrap_or(Value::Null),
                        "name": tc.pointer("/function/name").cloned().unwrap_or(Value::Null),
                        "arguments": tc.pointer("/function/arguments").cloned().unwrap_or(Value::Null),
                    }));
                }
            }
            items
        }
        // user: string content → single input_text; multimodal parts →
        // input_text / input_image (the data-URI image_url form is accepted
        // by the Responses API as-is)
        _ => {
            let content = match m.get("content") {
                Some(Value::String(s)) => json!([{ "type": "input_text", "text": s }]),
                Some(Value::Array(parts)) => Value::Array(
                    parts
                        .iter()
                        .map(|p| match p.get("type").and_then(|t| t.as_str()) {
                            Some("image_url") => json!({
                                "type": "input_image",
                                "image_url": p.pointer("/image_url/url").cloned().unwrap_or(Value::Null),
                            }),
                            _ => json!({
                                "type": "input_text",
                                "text": p.get("text").cloned().unwrap_or(Value::Null),
                            }),
                        })
                        .collect(),
                ),
                _ => json!([]),
            };
            vec![json!({ "type": "message", "role": "user", "content": content })]
        }
    }
}

/// Chat-completions tool schema → Responses flat tool schema
/// ({type:"function",function:{name,…}} → {type:"function",name,…}).
/// Order-preserving; entries without a `function` object are dropped.
fn responses_tools(tools: &Value) -> Value {
    Value::Array(
        tools
            .as_array()
            .map(|a| a.as_slice())
            .unwrap_or(&[])
            .iter()
            .filter_map(|t| {
                let f = t.get("function")?;
                Some(json!({
                    "type": "function",
                    "name": f.get("name").cloned().unwrap_or(Value::Null),
                    "description": f.get("description").cloned().unwrap_or(Value::Null),
                    "parameters": f.get("parameters").cloned().unwrap_or(Value::Null),
                }))
            })
            .collect(),
    )
}

/// Anthropic bodies are structurally different (top-level system, required
/// max_tokens), so Zone stability is maintained per-role fragments but the
/// exact byte-layout guarantee belongs to the OpenAI-compat path.
/// `messages` must already be role-merged (user/assistant alternating).
/// `max_output` overrides the built-in 8192 cap; `temperature` is omitted
/// when None (provider default).
///
/// Prompt caching follows the CacheTier of the provider (pi retention
/// alignment). Long = Claude Code's `getCacheControl` time setting,
/// `cache_control:{"type":"ephemeral","ttl":"1h"}` — a 60-minute cache
/// window (the API default when the marker is absent is only 5 minutes).
/// Short = `{"type":"ephemeral"}` — the 5-minute default window at the
/// cheaper 1.25× write rate. None = no markers at all (no write premium,
/// but nothing is cached). Markers sit on (a) the stable system block
/// (Zone S) and (b) the newest message (incremental breakpoint: the next
/// request's Zone H prefix hits).
///
/// Deliberately NO third breakpoint on the tool array (pi marks its last
/// tool): Anthropic's cache prefix order is tools → system → messages, so a
/// tools checkpoint only pays off when the system prompt changes while the
/// tool set stays identical — a case byte-stable Zone S makes impossible;
/// mid-session tool changes are already an honest epoch bump.
pub fn build_anthropic_body(
    model: &str,
    system_prompt: &str,
    messages: &[ChatMessage],
    max_output: Option<u32>,
    temperature: Option<f64>,
    tier: CacheTier,
) -> String {
    let cache_mark = || match tier {
        CacheTier::Long => Some(json!({ "type": "ephemeral", "ttl": "1h" })),
        CacheTier::Short => Some(json!({ "type": "ephemeral" })),
        CacheTier::None => None,
    };
    #[derive(Serialize)]
    struct Msg {
        role: String,
        content: serde_json::Value,
    }
    let mut msgs: Vec<Msg> = messages
        .iter()
        .filter(|m| m.role == "assistant" || m.role == "user")
        .map(|m| {
            // user messages with attached images become content blocks
            // (text + base64 image sources); plain messages stay strings
            let content = if !m.images.is_empty() {
                let mut blocks = Vec::new();
                if !m.content.is_empty() {
                    blocks.push(json!({ "type": "text", "text": m.content }));
                }
                for img in &m.images {
                    blocks.push(json!({
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": img.mime,
                            "data": img.b64
                        }
                    }));
                }
                serde_json::Value::Array(blocks)
            } else {
                serde_json::Value::String(m.content.clone())
            };
            Msg { role: m.role.clone(), content }
        })
        .collect();
    // incremental cache breakpoint: everything up to (and including) the
    // newest message becomes the cached prefix for the NEXT request
    if let (Some(mark), Some(last)) = (cache_mark(), msgs.last_mut()) {
        last.content = match std::mem::take(&mut last.content) {
            serde_json::Value::String(s) => {
                json!([{ "type": "text", "text": s, "cache_control": mark }])
            }
            serde_json::Value::Array(mut blocks) => {
                if let Some(b) = blocks.last_mut() {
                    b["cache_control"] = mark;
                }
                serde_json::Value::Array(blocks)
            }
            other => other,
        };
    }
    let mut body = serde_json::json!({
        "model": model,
        "max_tokens": max_output.unwrap_or(8192),
        "stream": true,
        "messages": msgs,
    });
    if let Some(t) = temperature {
        body["temperature"] = serde_json::Value::Number(
            serde_json::Number::from_f64(t).expect("temperature f64"),
        );
    }
    if !system_prompt.is_empty() {
        if let Some(mark) = cache_mark() {
            body["system"] = json!([
                {
                    "type": "text",
                    "text": system_prompt,
                    "cache_control": mark,
                }
            ]);
        } else {
            // tier None: plain top-level string — no marker, no write premium
            body["system"] = json!(system_prompt);
        }
    }
    serde_json::to_string(&body).expect("anthropic body")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sys() -> LanePrefix {
        LanePrefix::new("你是严谨的编程助手", "ccharness-test-0")
    }

    #[test]
    fn stable_prefix_across_turns() {
        let mut lp = sys();
        lp.bind_model("deepseek-chat");
        let u1 = ChatMessage::plain("user", "第一轮");
        let body1 = lp.build_openai_body("deepseek-chat", &u1);

        // after the turn, user+assistant fall into Zone H
        lp.append(&u1);
        lp.append(&ChatMessage::plain("assistant", "回答一"));
        let u2 = ChatMessage::plain("user", "第二轮");
        let body2 = lp.build_openai_body("deepseek-chat", &u2);

        // The Zone S+H span (messages array content up to the newest turn)
        // must be byte-identical in both requests. Extract sys,u1 from body1
        // (between the opening "[" and the closing "]}") and require the same
        // bytes at the same offset in body2.
        let start = body1.find("\"messages\":[").unwrap() + "\"messages\":[".len();
        let span1 = &body1[start..body1.len() - 2]; // strip "]}"
        let span2 = &body2[start..start + span1.len()];
        assert_eq!(span1, span2, "zone S+H bytes must be identical across turns");
        assert!(body2.len() > body1.len());
    }

    #[test]
    fn digest_advances_on_append() {
        let mut lp = sys();
        let d0 = lp.digest_hex();
        lp.append(&ChatMessage::plain("user", "x"));
        assert_ne!(d0, lp.digest_hex());
    }

    #[test]
    fn model_switch_bumps_epoch() {
        let mut lp = sys();
        lp.bind_model("m1");
        assert_eq!(lp.epoch, 0);
        lp.bind_model("m2");
        assert_eq!(lp.epoch, 1);
        lp.bind_model("m2");
        assert_eq!(lp.epoch, 1, "same model must not bump epoch");
    }

    #[test]
    fn cache_key_is_stable_in_head() {
        let lp = sys();
        let u = ChatMessage::plain("user", "hi");
        let b1 = lp.build_openai_body("m", &u);
        let b2 = lp.build_openai_body("m", &u);
        assert_eq!(b1, b2);
        assert!(b1.contains("\"prompt_cache_key\":\"ccharness-test-0\""));
        // key must sit in the byte-stable head, before messages
        let key_at = b1.find("\"prompt_cache_key\"").unwrap();
        let msgs_at = b1.find("\"messages\":").unwrap();
        assert!(key_at < msgs_at);
    }

    #[test]
    fn rebuild_preserves_bytes_and_digest_marks_epoch() {
        let mut lp = sys();
        lp.append(&ChatMessage::plain("user", "a"));
        let bytes_before = lp.prefix_bytes();
        let digest_before = lp.digest_hex();
        let epoch_before = lp.epoch;
        lp.rebuild("你是严谨的编程助手", &[ChatMessage::plain("user", "a")]);
        // identical byte content ⇒ identical fingerprint…
        assert_eq!(bytes_before, lp.prefix_bytes());
        assert_eq!(digest_before, lp.digest_hex());
        // …while the epoch counter is what marks the rebuild.
        assert_eq!(lp.epoch, epoch_before + 1);
    }

    #[test]
    fn behavior_binds_with_epoch_discipline() {
        let mut lp = sys();
        lp.bind_model("m1");
        // absent → present: epoch bump, params land in the head
        lp.bind_behavior(Some(0.3), Some(4096));
        assert_eq!(lp.epoch, 1);
        let body = lp.build_openai_body_multi("m1", &[ChatMessage::plain("user", "hi")], None);
        assert!(body.contains("\"temperature\":0.3"), "{body}");
        assert!(body.contains("\"max_tokens\":4096"), "{body}");
        // params must sit in the head, before messages
        let temp_at = body.find("\"temperature\"").unwrap();
        let msgs_at = body.find("\"messages\":").unwrap();
        assert!(temp_at < msgs_at);
        // same values: no bump
        lp.bind_behavior(Some(0.3), Some(4096));
        assert_eq!(lp.epoch, 1);
        // value change: bump
        lp.bind_behavior(Some(0.7), Some(4096));
        assert_eq!(lp.epoch, 2);
        // present → absent (provider default): bump again
        lp.bind_behavior(None, None);
        assert_eq!(lp.epoch, 3);
        let body = lp.build_openai_body_multi("m1", &[ChatMessage::plain("user", "hi")], None);
        assert!(!body.contains("\"temperature\""), "{body}");
        assert!(!body.contains("\"max_tokens\""), "{body}");
        // absent → absent: no-op
        lp.bind_behavior(None, None);
        assert_eq!(lp.epoch, 3);
    }

    #[test]
    fn tier_and_tools_bind_with_epoch_discipline() {
        let mut lp = sys();
        lp.bind_model("m1");
        // default tier Short: cache key present, no retention argument
        let body = lp.build_openai_body_multi("m1", &[ChatMessage::plain("user", "hi")], None);
        assert!(body.contains("\"prompt_cache_key\":\"ccharness-test-0\""), "{body}");
        assert!(!body.contains("prompt_cache_retention"), "{body}");
        // short → long: epoch bump + retention rides the head before messages
        lp.bind_cache_tier(CacheTier::Long);
        assert_eq!(lp.epoch, 1);
        let body = lp.build_openai_body_multi("m1", &[ChatMessage::plain("user", "hi")], None);
        assert!(body.contains("\"prompt_cache_retention\":\"24h\""), "{body}");
        let ret_at = body.find("\"prompt_cache_retention\"").unwrap();
        let msgs_at = body.find("\"messages\":").unwrap();
        assert!(ret_at < msgs_at, "retention must ride the byte-stable head");
        // same value: no bump
        lp.bind_cache_tier(CacheTier::Long);
        assert_eq!(lp.epoch, 1);
        // long → none: bump again; cache key AND retention both gone
        lp.bind_cache_tier(CacheTier::None);
        assert_eq!(lp.epoch, 2);
        let body = lp.build_openai_body_multi("m1", &[ChatMessage::plain("user", "hi")], None);
        assert!(!body.contains("prompt_cache_key"), "{body}");
        assert!(!body.contains("prompt_cache_retention"), "{body}");
        // none → none: no-op
        lp.bind_cache_tier(CacheTier::None);
        assert_eq!(lp.epoch, 2);
        // none → short: bump back, key returns
        lp.bind_cache_tier(CacheTier::Short);
        assert_eq!(lp.epoch, 3);
        let body = lp.build_openai_body_multi("m1", &[ChatMessage::plain("user", "hi")], None);
        assert!(body.contains("\"prompt_cache_key\""), "{body}");

        // tools loadout: None → Some is an expected rebuild
        lp.bind_tools_hash(None);
        assert_eq!(lp.epoch, 3, "no bump while no tools were ever bound");
        lp.bind_tools_hash(Some(0xDEAD));
        assert_eq!(lp.epoch, 4);
        lp.bind_tools_hash(Some(0xDEAD));
        assert_eq!(lp.epoch, 4, "same loadout must not bump epoch");
        lp.bind_tools_hash(Some(0xBEEF));
        assert_eq!(lp.epoch, 5, "loadout change (MCP join/leave) bumps epoch");
    }

    #[test]
    fn anthropic_body_honors_behavior_opts() {
        let msgs = [ChatMessage::plain("user", "hi")];
        // no overrides: built-in 8192 cap, no temperature
        let b = build_anthropic_body("claude-x", "sys", &msgs, None, None, CacheTier::Long);
        assert!(b.contains("\"max_tokens\":8192"), "{b}");
        assert!(!b.contains("temperature"), "{b}");
        // overrides applied
        let b = build_anthropic_body("claude-x", "sys", &msgs, Some(2048), Some(0.5), CacheTier::Long);
        assert!(b.contains("\"max_tokens\":2048"), "{b}");
        assert!(b.contains("\"temperature\":0.5"), "{b}");
    }

    #[test]
    fn anthropic_body_marks_1h_cache() {
        // Long tier = Claude-Code time setting: cache_control ephemeral +
        // ttl 1h (60min). Note: serde_json sorts object keys, so assertions
        // parse the body back instead of matching raw strings in order.
        let msgs = [
            ChatMessage::plain("user", "第一轮"),
            ChatMessage::plain("assistant", "回答一"),
            ChatMessage::plain("user", "第二轮"),
        ];
        let b = build_anthropic_body("claude-x", "系统提示", &msgs, None, None, CacheTier::Long);
        // exactly two markers: system + newest-message breakpoint
        assert_eq!(b.matches("\"ttl\":\"1h\"").count(), 2, "{b}");
        let v: serde_json::Value = serde_json::from_str(&b).expect("valid json");
        // system block is a marked content-block array
        let sys = v["system"].as_array().expect("system array");
        assert_eq!(sys.len(), 1, "{b}");
        assert_eq!(sys[0]["type"], "text");
        assert_eq!(sys[0]["text"], "系统提示");
        assert_eq!(sys[0]["cache_control"]["ttl"], "1h");
        assert_eq!(sys[0]["cache_control"]["type"], "ephemeral");
        // newest message was rewritten to a single text block carrying the
        // marker (incremental breakpoint); earlier messages stay unmarked
        let mv = v["messages"].as_array().expect("messages array");
        let last = mv.last().unwrap();
        let blocks = last["content"].as_array().expect("rewritten to blocks");
        assert_eq!(blocks.len(), 1, "{b}");
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "第二轮");
        assert_eq!(blocks[0]["cache_control"]["ttl"], "1h");
        assert!(mv[1]["content"].get("cache_control").is_none(), "{b}");
        // image-block user message: marker lands on the last block
        let msgs = [ChatMessage::with_images(
            "user",
            "看图",
            vec![ChatImage { mime: "image/png".into(), b64: "aGk=".into() }],
        )];
        let b = build_anthropic_body("claude-x", "", &msgs, None, None, CacheTier::Long);
        let v: serde_json::Value = serde_json::from_str(&b).expect("valid json");
        let blocks = v["messages"][0]["content"].as_array().expect("blocks");
        assert_eq!(blocks.last().unwrap()["type"], "image");
        assert_eq!(blocks.last().unwrap()["cache_control"]["ttl"], "1h");
    }

    #[test]
    fn anthropic_body_short_and_none_tiers() {
        let msgs = [ChatMessage::plain("user", "hi")];
        // Short: ephemeral markers WITHOUT ttl — the API-default 5-minute
        // window at the cheaper write rate (serde_json sorts object keys,
        // so match the marker substring without assuming key order)
        let b = build_anthropic_body("claude-x", "sys", &msgs, None, None, CacheTier::Short);
        assert!(b.contains("\"cache_control\":{\"type\":\"ephemeral\"}"), "{b}");
        assert!(!b.contains("ttl"), "{b}");
        assert_eq!(b.matches("\"cache_control\"").count(), 2, "{b}");
        let v: serde_json::Value = serde_json::from_str(&b).expect("valid json");
        assert_eq!(v["system"][0]["cache_control"]["type"], "ephemeral");
        assert!(v["system"][0]["cache_control"].get("ttl").is_none());

        // None: no markers anywhere; system degrades to the plain string
        let b = build_anthropic_body("claude-x", "sys", &msgs, None, None, CacheTier::None);
        assert!(!b.contains("cache_control"), "{b}");
        let v: serde_json::Value = serde_json::from_str(&b).expect("valid json");
        assert_eq!(v["system"], serde_json::json!("sys"));
        assert_eq!(v["messages"][0]["content"], serde_json::json!("hi"));
    }

    #[test]
    fn plain_message_bytes_unchanged() {
        assert_eq!(
            message_json(&ChatMessage::plain("user", "hi")),
            "{\"role\":\"user\",\"content\":\"hi\"}"
        );
    }

    #[test]
    fn image_user_message_wires_parts() {
        let m = ChatMessage::with_images(
            "user",
            "看这张图",
            vec![ChatImage { mime: "image/png".into(), b64: "aGk=".into() }],
        );
        let j = message_json(&m);
        assert!(j.starts_with("{\"role\":\"user\",\"content\":["), "{j}");
        assert!(j.contains("\"type\":\"text\""), "{j}");
        assert!(j.contains("\"type\":\"image_url\""), "{j}");
        assert!(j.contains("data:image/png;base64,aGk="), "{j}");
        // images never serialize on non-user roles (tool screenshots ride in
        // a deterministic synthetic user message built by the caller)
        let tool = ChatMessage {
            role: "tool".into(),
            content: "result".into(),
            tool_calls: None,
            tool_call_id: Some("t1".into()),
            images: vec![ChatImage { mime: "image/png".into(), b64: "aGk=".into() }],
        };
        let jt = message_json(&tool);
        assert!(!jt.contains("image"), "{jt}");
        assert!(jt.contains("\"tool_call_id\":\"t1\""), "{jt}");
    }

    #[test]
    fn anthropic_body_wires_image_blocks() {
        let msgs = [ChatMessage::with_images(
            "user",
            "看图",
            vec![ChatImage { mime: "image/png".into(), b64: "aGk=".into() }],
        )];
        let b = build_anthropic_body("claude-x", "sys", &msgs, None, None, CacheTier::Long);
        assert!(b.contains("\"type\":\"image\""), "{b}");
        assert!(b.contains("\"media_type\":\"image/png\""), "{b}");
        assert!(b.contains("\"data\":\"aGk=\""), "{b}");
    }

    // ---- OpenAI Responses adapter ----

    fn tools_schema() -> Value {
        serde_json::json!([
            {
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": "Read a file",
                    "parameters": { "type": "object", "properties": { "path": { "type": "string" } } }
                }
            }
        ])
    }

    #[test]
    fn responses_body_shape_and_stability() {
        let mut lp = LanePrefix::new("系统提示", "ccharness-rs-0");
        lp.bind_model("gpt-5.1");
        let u1 = ChatMessage::plain("user", "第一轮");
        let body1 = lp.build_responses_body("gpt-5.1", &[u1.clone()], Some(&tools_schema()));

        // protocol shape: instructions + input items + flat tools
        assert!(body1.contains("\"instructions\":\"系统提示\""), "{body1}");
        assert!(body1.contains("\"store\":false"), "{body1}");
        assert!(body1.contains("\"stream\":true"), "{body1}");
        assert!(body1.contains("\"prompt_cache_key\":\"ccharness-rs-0\""), "{body1}");
        assert!(!body1.contains("prompt_cache_retention"), "{body1}");
        assert!(body1.contains("\"tool_choice\":\"auto\""), "{body1}");
        // tool schema flattened: {type,name,…} not nested under a function
        // object (key order is sorted by serde_json)
        assert!(body1.contains("\"name\":\"read_file\""), "{body1}");
        assert!(!body1.contains("\"function\":{"), "{body1}");
        // user message converted to a message item with input_text
        let v: Value = serde_json::from_str(&body1).expect("valid json");
        assert_eq!(v["input"][0]["type"], "message");
        assert_eq!(v["input"][0]["role"], "user");
        assert_eq!(v["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(v["input"][0]["content"][0]["text"], "第一轮");

        // Zone S+H byte stability across turns (same invariant as the
        // chat-completions path)
        lp.append(&u1);
        lp.append(&ChatMessage::plain("assistant", "回答一"));
        let body2 = lp.build_responses_body("gpt-5.1", &[ChatMessage::plain("user", "第二轮")], Some(&tools_schema()));
        let start = body1.find("\"input\":[").unwrap() + "\"input\":[".len();
        let span1 = &body1[start..body1.len() - 2];
        let span2 = &body2[start..start + span1.len()];
        assert_eq!(span1, span2, "Responses input prefix bytes must be stable");
        assert!(body2.len() > body1.len());

        // deterministic: same state → same bytes
        let again = lp.build_responses_body("gpt-5.1", &[ChatMessage::plain("user", "第二轮")], Some(&tools_schema()));
        assert_eq!(body2, again);
    }

    #[test]
    fn responses_body_converts_tool_loops() {
        let mut lp = LanePrefix::new("", "k0");
        lp.bind_model("gpt-5.1");
        let mut call = ChatMessage::plain("assistant", "");
        call.tool_calls = Some(serde_json::json!([
            { "id": "call_1", "type": "function",
              "function": { "name": "read_file", "arguments": "{\"path\":\"a.rs\"}" } }
        ]));
        let result = ChatMessage {
            role: "tool".into(),
            content: "file body".into(),
            tool_calls: None,
            tool_call_id: Some("call_1".into()),
            images: Vec::new(),
        };
        let body = lp.build_responses_body(
            "gpt-5.1",
            &[call, result, ChatMessage::plain("user", "继续")],
            Some(&tools_schema()),
        );
        let v: Value = serde_json::from_str(&body).expect("valid json");
        let items = v["input"].as_array().expect("items");
        // assistant(empty text) → one function_call; tool → function_call_output
        assert_eq!(items[0]["type"], "function_call");
        assert_eq!(items[0]["call_id"], "call_1");
        assert_eq!(items[0]["name"], "read_file");
        assert_eq!(items[0]["arguments"], "{\"path\":\"a.rs\"}");
        assert_eq!(items[1]["type"], "function_call_output");
        assert_eq!(items[1]["call_id"], "call_1");
        assert_eq!(items[1]["output"], "file body");
        assert_eq!(items[2]["type"], "message");
        assert_eq!(items[2]["role"], "user");
        // empty-text assistant must NOT emit an empty message item
        assert!(items.iter().all(|i| i["type"] != "message" || i["role"] != "assistant"));
    }

    #[test]
    fn responses_body_tier_and_head_params() {
        let mut lp = LanePrefix::new("sys", "k1");
        lp.bind_model("gpt-5.1");
        lp.bind_cache_tier(CacheTier::None);
        lp.bind_thinking(Some("high"));
        lp.bind_behavior(Some(0.3), Some(4096));
        let body = lp.build_responses_body("gpt-5.1", &[ChatMessage::plain("user", "hi")], None);
        // tier None: no cache routing at all
        assert!(!body.contains("prompt_cache_key"), "{body}");
        // nested reasoning effort + sampling params in the head
        assert!(body.contains("\"reasoning\":{\"effort\":\"high\"}"), "{body}");
        assert!(body.contains("\"temperature\":0.3"), "{body}");
        assert!(body.contains("\"max_output_tokens\":4096"), "{body}");
        let reasoning_at = body.find("\"reasoning\"").unwrap();
        let input_at = body.find("\"input\":").unwrap();
        assert!(reasoning_at < input_at, "head must precede input");

        // long tier adds the retention argument
        let mut lp = LanePrefix::new("sys", "k1");
        lp.bind_model("gpt-5.1");
        lp.bind_cache_tier(CacheTier::Long);
        let body = lp.build_responses_body("gpt-5.1", &[ChatMessage::plain("user", "hi")], None);
        assert!(body.contains("\"prompt_cache_key\":\"k1\""), "{body}");
        assert!(body.contains("\"prompt_cache_retention\":\"24h\""), "{body}");
    }

    #[test]
    fn responses_body_multimodal_user_parts() {
        let lp = LanePrefix::new("", "k2");
        let m = ChatMessage::with_images(
            "user",
            "看图",
            vec![ChatImage { mime: "image/png".into(), b64: "aGk=".into() }],
        );
        let body = lp.build_responses_body("gpt-5.1", &[m], None);
        let v: Value = serde_json::from_str(&body).expect("valid json");
        let parts = v["input"][0]["content"].as_array().expect("parts");
        assert_eq!(parts[0]["type"], "input_text");
        assert_eq!(parts[0]["text"], "看图");
        assert_eq!(parts[1]["type"], "input_image");
        assert_eq!(parts[1]["image_url"], "data:image/png;base64,aGk=");
    }
}
