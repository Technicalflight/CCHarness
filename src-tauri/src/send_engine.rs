// Send engine: everything behind the send button — lane orchestration
// for chat / arena / group sends, provider & subagent resolution,
// rehearsal paths, the approval gate, tool rounds with goal/plan/deep
// handling, persistence, telemetry and the auto-title task. Split
// out of commands.rs (P2-13).

use crate::chat::{self, SendCtx};
use crate::commands::{
    backup_snapshot, grant_key, lp_is_empty, next_record_ts, open_approval, prefixes_lock,
    restore_args, save_todos, warn_save, TodoItem, AppState, APPROVAL_TIMEOUT_SECS,
    GOAL_MAX_TOOL_ROUNDS, MAX_DELEGATIONS_PER_TURN, MAX_TOOL_ROUNDS, SendResult, WRITE_LOG_CAP,
};
use crate::compaction::{maybe_auto_compact, rolling_memo_apply, render_memo, tools_hash};
use crate::config::{self, AppConfig, Provider};
use crate::prefix::{message_json, ChatMessage, LanePrefix};
use crate::sessions::{now_ms, SessionStore};
use crate::types_rs::{MessageRecord, RequestStat, SessionBinding, StreamEvent};
use crate::workflow::{
    handle_goal_tool, parse_goal_summary_ext, permission_of, persist_gate, record_workflow_of,
    strip_list_prefix,
    sm_put, workflow_of_in,
};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{Manager, State};
use uuid::Uuid;

/// Cached tool schemas of all enabled, connected servers (spawned-task safe).
fn state_mcp_tools(data_dir: &std::path::Path) -> Vec<Value> {
    let cfg = config::load(data_dir);
    let enabled: Vec<crate::config::McpServerConfig> =
        cfg.mcp_servers.iter().filter(|s| s.enabled).cloned().collect();
    crate::mcp::global().cached_tools(&enabled)
}

/// Connect every enabled server once (best-effort) so the first send does
/// not pay the handshake latency. Called from run_send before lanes start.
async fn ensure_mcp_servers(data_dir: &std::path::Path, stop: &AtomicBool) {
    let cfg = config::load(data_dir);
    for s in cfg.mcp_servers.iter().filter(|s| s.enabled) {
        // each dead server can burn up to ~2×15s of handshake timeout before
        // the lanes spawn — a stop click must cut the walk short
        if stop.load(Ordering::Relaxed) {
            return;
        }
        if let Err(e) = crate::mcp::global().ensure(s).await {
            eprintln!("[mcp] {} 连接失败: {e}", s.name);
        }
    }
}


#[tauri::command]
pub fn stop_generation(state: State<'_, AppState>, session_id: String) {
    if let Some(flag) = state.stops.lock().unwrap_or_else(|p| p.into_inner()).get(&session_id) {
        flag.store(true, Ordering::Relaxed);
    }
}

/// Resolves as soon as the session's stop flag flips. Paired with
/// `tokio::select!` around the pre-stream phases (MCP warm-up, auto
/// compaction, memory recall, deep/review rehearsals) so a stop click takes
/// effect immediately instead of being ignored until the first streamed
/// token — previously those phases left the turn looking frozen with a dead
/// stop button.
async fn wait_stopped(stop: &AtomicBool) {
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    }
}

/// Emit the turn-closing event for a lane aborted before any streamed
/// content (stop during a pre-stream phase). No assistant record exists yet
/// — nothing was generated — so the transcript keeps only the user message.
/// The digest-span marker is dropped (mirroring the error path): the aborted
/// turn's bytes never went upstream, so the next turn's totals legitimately
/// shrink and must not be judged against the stale span.
async fn abort_lane_pre_stream(
    state: &AppState,
    channel: &tauri::ipc::Channel<StreamEvent>,
    session_id: &str,
    lane: u32,
    message_id: &str,
) {
    if let Some(map) = state.last_span.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
        map.remove(&(session_id.to_string(), lane));
    }
    let _ = channel.send(StreamEvent::Done {
        lane,
        message_id: message_id.to_string(),
        status: "stopped".into(),
        confidence: None,
    });
}

#[tauri::command]
pub async fn send_message(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    session_id: String,
    content: String,
    skill_calls: Option<Vec<String>>,
    images: Option<Vec<crate::prefix::ChatImage>>,
    channel: tauri::ipc::Channel<StreamEvent>,
) -> Result<SendResult, String> {
    let bindings = {
        let sf = state.store.load(&session_id)?;
        sf.meta.bindings.clone()
    };
    run_send(
        app,
        &state,
        session_id,
        content,
        skill_calls,
        images.unwrap_or_default(),
        bindings,
        channel,
        false,
    )
    .await?;
    Ok(SendResult { ok: true })
}

#[tauri::command]
pub async fn arena_send(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    session_id: String,
    content: String,
    skill_calls: Option<Vec<String>>,
    images: Option<Vec<crate::prefix::ChatImage>>,
    lanes: Vec<SessionBinding>,
    channel: tauri::ipc::Channel<StreamEvent>,
) -> Result<SendResult, String> {
    // persist lanes first so they survive restarts
    state.store.set_bindings(&session_id, lanes.clone())?;
    run_send(
        app,
        &state,
        session_id,
        content,
        skill_calls,
        images.unwrap_or_default(),
        lanes,
        channel,
        true,
    )
    .await?;
    Ok(SendResult { ok: true })
}

/// Group chat / round-table: the SAME prompt goes to N members SEQUENTIALLY.
/// Default (sequential) order is lane order; with `moderated`, after each
/// member speaks an LLM host picks the next speaker (or ends the table), so
/// the discussion is scheduled by relevance instead of fixed order. Each
/// later member's user message carries the earlier members' replies as a
/// quoted block; every member reuses the arena lane machinery (persistence,
/// StreamEvent, per-lane prefix cache) with lane = member index. A member
/// that fails or is stopped ends the round-table with the replies gathered
/// so far. Attached images ride only on the first member's turn.
#[tauri::command]
pub async fn group_send(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    session_id: String,
    content: String,
    skill_calls: Option<Vec<String>>,
    images: Option<Vec<crate::prefix::ChatImage>>,
    moderated: Option<bool>,
    lanes: Vec<SessionBinding>,
    channel: tauri::ipc::Channel<StreamEvent>,
) -> Result<SendResult, String> {
    if lanes.is_empty() {
        return Err("圆桌成员为空 —— 请先添加要参与的模型".into());
    }
    let moderated = moderated.unwrap_or(false);
    // persist lanes first so they survive restarts (same as arena)
    state.store.set_bindings(&session_id, lanes.clone())?;
    let host_binding = lanes[0].clone();
    // remaining speaker queue; default order = lane order
    let mut queue: Vec<usize> = (0..lanes.len()).collect();
    let mut round_text = String::new();
    let mut pending_images = images.unwrap_or_default();
    while let Some(idx) = queue.first().copied() {
        queue.remove(0);
        let binding = &lanes[idx];
        let member_content = if round_text.is_empty() {
            content.clone()
        } else {
            format!(
                "{content}\n\n---\n【圆桌讨论】其他成员已就上述问题发表看法，请阅读后发表你的观点（可以补充、反驳或修正他人，不要重复他人已说的内容）：\n{round_text}"
            )
        };
        // run_send awaits its (single) lane task internally, so this loop is
        // strictly sequential — each member sees all earlier replies.
        // Snapshot the newest persisted ts BEFORE the turn: run_send sends on
        // lane 0 no matter which member is speaking, so the read-back below
        // keys on "ok assistant after this mark" — keying on lane == idx made
        // member 1+ read nothing and silently ended the table.
        let before_ts = {
            let _guard = state.save_lock.lock().await;
            state
                .store
                .load(&session_id)
                .ok()
                .and_then(|sf| sf.messages.iter().map(|m| m.ts).max())
                .unwrap_or(0)
        };
        run_send(
            app.clone(),
            &state,
            session_id.clone(),
            member_content,
            skill_calls.clone(),
            std::mem::take(&mut pending_images),
            vec![binding.clone()],
            channel.clone(),
            true,
        )
        .await?;
        // read this member's final reply back from the persisted transcript
        let said: String = {
            let _guard = state.save_lock.lock().await;
            state
                .store
                .load(&session_id)
                .ok()
                .and_then(|sf| {
                    sf.messages
                        .iter()
                        .rev()
                        .find(|m| m.ts > before_ts && m.role == "assistant" && m.status == "ok")
                        .map(|m| m.content.clone())
                })
                .unwrap_or_default()
        };
        let said = said.trim().to_string();
        if said.is_empty() {
            break; // member failed or the user stopped it — end the table
        }
        round_text.push_str(&format!("\n【成员 {} · {}】\n{}\n", idx + 1, binding.model, said));
        // ---- LLM-host scheduling: pick the most valuable next speaker ----
        if moderated && !queue.is_empty() {
            let members = queue
                .iter()
                .map(|&j| format!("{}. {}", j + 1, lanes[j].model))
                .collect::<Vec<_>>()
                .join("\n");
            let ask = format!(
                "圆桌讨论进行中。议题：\n{content}\n\n已有发言：\n{round_text}\n\n剩余成员（序号. 模型）：\n{members}\n\n你是主持人。请选出最适合下一个发言的成员（能补充新视角、对抗验证或推进讨论者）。只输出一个成员序号（例如 2）；若认为讨论已充分，只输出：结束"
            );
            let next = match resolve_provider(&config::load(&state.data_dir), &host_binding) {
                Some(p) => chat::ask_once(
                    &state.client,
                    p,
                    &host_binding.model,
                    "你是圆桌讨论的主持人，负责调度发言顺序。只输出序号或「结束」。",
                    &ask,
                    60,
                )
                .await
                .ok(),
                None => None,
            };
            let mut picked: Option<usize> = None;
            if let Some(reply) = next {
                let r = reply.trim();
                if r.contains("结束") || r.eq_ignore_ascii_case("end") {
                    break;
                }
                // first digit in the reply names the member (1-based over
                // the listed remaining set)
                if let Some(d) = r.chars().find(|c| c.is_ascii_digit()).and_then(|c| c.to_digit(10)) {
                    let pos = (d as usize).saturating_sub(1);
                    if pos < queue.len() {
                        picked = Some(queue[pos]);
                    }
                }
                // parse failure ⇒ sequential fallback (queue order stands)
            }
            if let Some(p) = picked {
                if let Some(pos) = queue.iter().position(|&j| j == p) {
                    queue.remove(pos);
                    queue.insert(0, p);
                }
            }
        }
    }
    Ok(SendResult { ok: true })
}

pub(crate) fn resolve_provider<'a>(cfg: &'a AppConfig, binding: &SessionBinding) -> Option<&'a Provider> {
    cfg.providers
        .iter()
        .find(|p| p.id == binding.provider_id && p.enabled && !p.api_key.is_empty())
}

/// A named profile overrides provider+model; without one the sub inherits
/// the parent session's binding.
fn binding_for_sub(
    profile: Option<&crate::config::SubagentProfile>,
    parent: &SessionBinding,
) -> SessionBinding {
    match profile {
        Some(p) => SessionBinding {
            provider_id: p.provider_id.clone(),
            model: p.model.clone(),
        },
        None => parent.clone(),
    }
}

/// Look up an enabled named subagent profile by the delegate call's
/// `agent` argument (settings-managed + file-defined).
fn find_subagent_profile(
    cfg: &AppConfig,
    data_dir: &std::path::Path,
    args: &str,
) -> Option<crate::config::SubagentProfile> {
    let name = serde_json::from_str::<Value>(args)
        .ok()
        .and_then(|a| a.get("agent").and_then(|v| v.as_str()).map(|s| s.trim().to_string()))
        .unwrap_or_default();
    if name.is_empty() {
        return None;
    }
    all_subagents(cfg, data_dir).into_iter().find(|p| p.enabled && p.name == name)
}

/// Parse `<data_dir>/agents/*.md` into extra named-subagent profiles
/// (ZCode-style file definitions): frontmatter carries name / description /
/// model / tools (comma list) / max_turns / enabled; the markdown body is
/// the role system prompt. The model must belong to an enabled provider —
/// otherwise the file is skipped. Resolved on the fly, never persisted.
fn file_agent_profiles(cfg: &AppConfig, data_dir: &std::path::Path) -> Vec<crate::config::SubagentProfile> {
    let dir = data_dir.join("agents");
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else { return out };
    // the profile list rides into request bytes (delegate tool description)
    // — read_dir order is filesystem-defined (exFAT has none), so sort by
    // file name to keep the head deterministic across rebuilds
    let mut paths: Vec<std::path::PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for p in paths {
        if p.extension().and_then(|x| x.to_str()) != Some("md") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&p) else { continue };
        let Some(rest) = raw.strip_prefix("---") else { continue };
        let Some(i) = rest.find("\n---") else { continue };
        let fm = &rest[..i];
        let body = rest[i + 4..].trim_start_matches(['\r', '\n']).to_string();
        let mut name = String::new();
        let mut description = String::new();
        let mut model = String::new();
        let mut tools: Vec<String> = Vec::new();
        let mut max_turns: Option<u32> = None;
        let mut enabled = true;
        for line in fm.lines() {
            let Some((k, v)) = line.split_once(':') else { continue };
            let v = v.trim().trim_matches('"').trim_matches('\'').to_string();
            match k.trim() {
                "name" => name = v,
                "description" => description = v,
                "model" => model = v,
                "enabled" => enabled = v != "false",
                "max_turns" => max_turns = v.parse::<u32>().ok(),
                "tools" => {
                    let v = v.trim_start_matches('[').trim_end_matches(']').replace('，', ",");
                    tools = v
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                }
                _ => {}
            }
        }
        if name.is_empty() {
            name = p.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
        }
        if model.is_empty() {
            continue;
        }
        let Some(provider_id) = cfg
            .providers
            .iter()
            .find(|pr| pr.enabled && pr.models.iter().any(|m| *m == model))
            .map(|pr| pr.id.clone())
        else {
            continue;
        };
        out.push(crate::config::SubagentProfile {
            id: format!("file:{}", p.file_stem().and_then(|s| s.to_str()).unwrap_or(&name)),
            name,
            description,
            provider_id,
            model,
            system_prompt: body,
            enabled,
            tools,
            max_turns,
            source: "file".into(),
        });
    }
    out
}

/// All usable named-subagent profiles: settings-managed ones first, then
/// file-defined ones (<data_dir>/agents/*.md).
fn all_subagents(cfg: &AppConfig, data_dir: &std::path::Path) -> Vec<crate::config::SubagentProfile> {
    let mut v: Vec<crate::config::SubagentProfile> = cfg.subagents.clone();
    v.extend(file_agent_profiles(cfg, data_dir));
    v
}

/// Max tool rounds inside one sub-agent run. Sub-agents are read-only, so
/// the loop is naturally bounded; this guards pathological repetition.
const SUB_MAX_ROUNDS: usize = 12;

/// Result text handed back to the parent model (chars).
const SUB_RESULT_CAP: usize = 4_000;

/// Removes the sub lane's prefix entry when a delegation ends — success,
/// error or early return alike (Drop). The entry is keyed by a fresh
/// per-run sub_id, so once the run is over nothing can reuse it (sub
/// transcripts never go through run_send's rebuild path); without this
/// guard the static map grows one dead LanePrefix per delegation.
struct SubPrefixGuard<'a> {
    state: &'a AppState,
    sub_id: String,
}
impl Drop for SubPrefixGuard<'_> {
    fn drop(&mut self) {
        if let Some(map) = prefixes_lock(self.state).as_mut() {
            map.remove(&(self.sub_id.clone(), 0u32));
        }
    }
}

/// Run a background sub-agent for `delegate_subagent`: its own hidden
/// session (kind "sub"), own prefix state, read-only tools, no MCP, no UI
/// streaming (events go to a discard channel). Returns the final conclusion
/// text for the parent's tool-result message. Sub-sessions are kept on disk
/// (inspectable) but excluded from sidebar lists by the frontend; they carry
/// the parent link and are deleted with it (sessions::subs_of cascade), and
/// the per-run prefix entry dies with the run via SubPrefixGuard.
pub(crate) async fn run_subagent(
    state: &AppState,
    client: &reqwest::Client,
    data_dir: &std::path::Path,
    parent_session: &str,
    task: &str,
    profile: Option<&crate::config::SubagentProfile>,
    parent_binding: &SessionBinding,
    cfg: &AppConfig,
    parent_channel: &tauri::ipc::Channel<StreamEvent>,
    parent_lane: u32,
    call_id: &str,
) -> Result<String, String> {
    let store = SessionStore::new(data_dir);
    let parent_ws = store.load(parent_session).ok().and_then(|sf| sf.meta.workspace.clone());
    let title: String = task.chars().take(20).collect();
    // live progress: the sub lane's deltas are forwarded to the parent UI,
    // tagged with the delegate tool-call id so the card can stream them
    let tap = chat::ProgressTap {
        lane: parent_lane,
        call_id: call_id.to_string(),
        title: title.clone(),
        channel: parent_channel.clone(),
    };
    let sf = store.create("sub", vec![binding_for_sub(profile, parent_binding)], &format!("🤖 子任务 · {title}"))?;
    let sub_id = sf.meta.id.clone();
    // the parent link is the cascade key: deleting the parent session
    // removes this transcript with it (sessions::subs_of / delete_session)
    store.adopt_sub(&sub_id, parent_session, parent_ws.clone())?;

    let mut system_full = crate::sysprompt::assemble(&cfg.settings.system_prompt, parent_ws.as_deref());
    if let Some(p) = profile {
        let sp = p.system_prompt.trim();
        if !sp.is_empty() {
            system_full = format!("{system_full}\n\n# Subagent Role — {}\n{sp}", p.name);
        }
    }
    let binding = binding_for_sub(profile, parent_binding);
    let provider = resolve_provider(cfg, &binding).cloned().ok_or("Provider 未配置或未填 API Key")?;
    let model = binding.model.clone();
    // Tool surface: whitelist from the profile when given (write tools are
    // opt-in per profile), otherwise the safe read-only default.
    let tools = match profile {
        Some(p) if !p.tools.is_empty() => crate::agent_tools::schema_filtered(&p.tools),
        _ => crate::agent_tools::schema_readonly(),
    };
    let max_rounds = profile
        .and_then(|p| p.max_turns)
        .map(|n| (n as usize).clamp(1, 40))
        .unwrap_or(SUB_MAX_ROUNDS);
    // stable per-(parent, role) routing shard: repeated runs of the same
    // profile replay identical Zone S bytes and hit the warm shard instead
    // of cold-starting on a fresh per-run key
    let cache_key =
        crate::prefix::subagent_cache_key(parent_session, profile.map(|p| p.name.as_str()));

    // the task record carries workflow="subagent" so transcript_for_lane
    // injects SUBAGENT_DIRECTIVE — identical bytes live and after restart
    let task_record = MessageRecord {
        id: Uuid::new_v4().to_string(),
        lane: 0,
        role: "user".into(),
        content: task.to_string(),
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
        workflow: Some("subagent".into()),
        images: Vec::new(),
    };
    {
        let mut s = store.load(&sub_id)?;
        s.messages.push(task_record.clone());
        store.save(&s)?;
    }

    // prefix state for the sub lane (same static map, distinct key)
    let owned_prefix = {
        let mut guard = prefixes_lock(&state);
        let map = guard.get_or_insert_with(HashMap::new);
        let lp = map
            .entry((sub_id.clone(), 0u32))
            .or_insert_with(|| LanePrefix::new(&system_full, &cache_key));
        lp.bind_model(&model);
        // sub agents honor the same per-model sampling params (epoch-gated)
        let beh = provider.behavior.get(&model);
        lp.bind_behavior(beh.and_then(|b| b.temperature), beh.and_then(|b| b.max_output));
        // head-byte discipline: cache-tier toggle and MCP loadout changes are
        // expected rebuilds and must move the epoch, not fake an upstream miss
        lp.bind_cache_tier(provider.cache_tier());
        lp.bind_tools_hash(tools_hash(Some(&tools)));
        lp.clone()
    };
    // the entry dies with the run, on every exit path — see SubPrefixGuard
    let _prefix_guard = SubPrefixGuard { state, sub_id: sub_id.clone() };

    let task_msg = ChatMessage::plain("user", format!("{}{task}", chat::SUBAGENT_DIRECTIVE));
    let mut sent_this_turn: Vec<ChatMessage> = vec![task_msg.clone()];
    let channel = tauri::ipc::Channel::<StreamEvent>::new(|_| Ok(()));
    let stop = Arc::new(AtomicBool::new(false));

    let mut final_text: Option<String> = None;
    for _round in 1..=max_rounds {
        let body = chat::build_body(&provider, &model, &owned_prefix, &sent_this_turn, &system_full, Some(&tools));
        let message_id = Uuid::new_v4().to_string();
        let ctx = chat::SendCtx {
            client,
            provider: &provider,
            model: &model,
            lane: 0,
            message_id: message_id.clone(),
            channel: channel.clone(),
            stop: stop.clone(),
            progress_tap: Some(tap.clone()),
            // pi gating: the affinity header only makes sense while the tier
            // still routes via prompt_cache_key (tier None sends no marks)
            affinity: if provider.cache_tier() == crate::config::CacheTier::None {
                None
            } else {
                Some(cache_key.clone())
            },
        };
        let outcome = match chat::stream_lane(&ctx, body, chat::auth_for(&provider)).await {
            Ok(o) => o,
            Err(e) => return Err(e),
        };
        let usage = outcome.usage.clone();
        let cost = chat::cost_of(&usage, &provider, &model);
        let stat = RequestStat {
            seq: state.seq.fetch_add(1, Ordering::Relaxed),
            ts: now_ms(),
            lane: 0,
            model: model.clone(),
            epoch: owned_prefix.epoch,
            prefix_bytes: owned_prefix.prefix_bytes_public(),
            added_bytes: sent_this_turn.iter().map(|m| message_json(m).len() + 1).sum::<usize>(),
            chain_ok: true,
            input_tokens: usage.input,
            cached_tokens: usage.cached,
            output_tokens: usage.output,
            cost_usd: cost,
            significant_miss: false,
            rebilled_tokens: 0,
            rebilled_cost: None,
            miss_cause: None,
        };
        let has_tools = !outcome.tool_calls.is_empty();
        let tool_wire: Option<Vec<crate::types_rs::ToolCallWire>> = if has_tools {
            Some(
                outcome
                    .tool_calls
                    .iter()
                    .map(|tc| crate::types_rs::ToolCallWire {
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        arguments: tc.arguments.clone(),
                    })
                    .collect(),
            )
        } else {
            None
        };
        let asst_msg = ChatMessage {
            role: "assistant".into(),
            content: outcome.content.clone(),
            tool_calls: tool_wire.as_ref().map(|w| chat::tool_calls_wire_value(w)),
            tool_call_id: None,
            images: Vec::new(),
        };
        let pending_call_ids: Vec<String> = tool_wire
            .as_ref()
            .map(|w| w.iter().map(|tc| tc.id.clone()).collect())
            .unwrap_or_default();
        let record = MessageRecord {
            id: message_id,
            lane: 0,
            role: "assistant".into(),
            reasoning: if outcome.reasoning.is_empty() { None } else { Some(outcome.reasoning.clone()) },
            content: outcome.content.clone(),
            ts: next_record_ts(&state),
            model: Some(model.clone()),
            status: outcome.status.clone(),
            usage: Some(usage),
            cost_usd: cost,
            confidence: outcome.confidence,
            tool_calls: tool_wire,
            tool_call_id: None,
            skill_calls: None,
            workflow: None,
            images: Vec::new(),
        };
        sent_this_turn.push(asst_msg.clone());
        {
            let mut s = store.load(&sub_id)?;
            s.messages.push(record);
            // stopped/errored mid-round: same pairing discipline as the
            // main lane — no unpaired tool_calls may survive to a rebuild
            if outcome.status != "ok" {
                for cid in &pending_call_ids {
                    {
                        s.messages.push(MessageRecord {
                            id: Uuid::new_v4().to_string(),
                            lane: 0,
                            role: "tool".into(),
                            content: crate::chat::UNPAIRED_TOOL_NOTE.into(),
                            reasoning: None,
                            ts: next_record_ts(&state),
                            model: None,
                            status: "ok".into(),
                            usage: None,
                            cost_usd: None,
                            confidence: None,
                            tool_calls: None,
                            tool_call_id: Some(cid.clone()),
                            skill_calls: None,
                            workflow: None,
                            images: Vec::new(),
                        });
                    }
                }
            }
            s.telemetry.push(stat);
            s.meta.updated_at = now_ms();
            store.save(&s)?;
        }
        if outcome.status != "ok" {
            break;
        }
        if !has_tools {
            final_text = Some(outcome.content);
            break;
        }
        // execute read tools, feed results back
        for tc in &outcome.tool_calls {
            let result = match serde_json::from_str::<Value>(&tc.arguments) {
                Err(e) => format!("ERROR: 参数不是合法 JSON: {e}"),
                Ok(args) => {
                    // 同步工具（run_command 轮询最长 120 秒等）必须离开
                    // async 执行器线程，否则整条 runtime 被卡住
                    let ws = parent_ws.as_deref().unwrap_or("").to_string();
                    let name = tc.name.clone();
                    tokio::task::spawn_blocking(move || {
                        crate::agent_tools::execute(&ws, &name, &args)
                    })
                    .await
                    .unwrap_or_else(|e| format!("ERROR: 工具任务失败: {e}"))
                }
            };
            // guardrails: the sub lane reads untrusted external content too
            // (web_fetch over unknown pages, grep over unknown workspaces) —
            // the main lane fences its tool results, and this background
            // path runs without a user watching the stream. Same shape as
            // the main lane: wrap first, then spill.
            let result = if cfg.settings.guardrails
                && !result.starts_with("ERROR:")
                && !result.starts_with("DENIED:")
            {
                crate::guard::wrap_untrusted(&tc.name, &result, &cfg.settings.guardrails_extra)
            } else {
                result
            };
            // context-volume: same trim as the main lane, so a delegation
            // whose result is later promoted into the parent replay keeps
            // identical bytes
            let result = crate::spill::maybe_spill(
                &sub_id,
                &tc.name,
                &result,
                cfg.settings.spill_max_chars,
            );
            let tool_record = MessageRecord {
                id: Uuid::new_v4().to_string(),
                lane: 0,
                role: "tool".into(),
                content: result.clone(),
                reasoning: None,
                ts: next_record_ts(&state),
                model: None,
                status: "ok".into(),
                usage: None,
                cost_usd: None,
                confidence: None,
                tool_calls: None,
                tool_call_id: Some(tc.id.clone()),
                skill_calls: None,
                workflow: None,
                images: Vec::new(),
            };
            sent_this_turn.push(ChatMessage {
                role: "tool".into(),
                content: result,
                tool_calls: None,
                tool_call_id: Some(tc.id.clone()),
                images: Vec::new(),
            });
            let mut s = store.load(&sub_id)?;
            s.messages.push(tool_record);
            s.meta.updated_at = now_ms();
            store.save(&s)?;
        }
    }

    // The sub-turn is NOT folded into a Zone H here: unlike a main lane,
    // this entry is keyed by a per-run sub_id that nothing re-opens after
    // the run — _prefix_guard removes it on return, so folding would only
    // polish bytes that are about to be dropped.

    // flip the parent's delegate card out of its streaming state
    tap.forward("", true);

    let text = final_text
        .ok_or("子任务在轮数上限内未产出结论")?
        .trim()
        .to_string();
    if text.is_empty() {
        return Err("子任务返回了空结论".into());
    }
    Ok(text.chars().take(SUB_RESULT_CAP).collect())
}

/// Garnish cap per ToT candidate (chars) — keeps the rehearsal block small
/// even when a model is verbose.
const TOT_CANDIDATE_CAP: usize = 1_500;

/// Deep mode (ToT-style rehearsal): three candidate approaches are drafted
/// in parallel (safe / rigorous / creative angles), then a judge ask_once
/// picks the most promising one. Returns a text block to append to the user
/// message actually SENT this turn — same garnish mechanism as vector-memory
/// recall (persisted record stays clean; restarts rebuild a new epoch
/// without it). Any failure returns None: deep mode must never block a
/// normal send.
async fn run_tot_rehearsal(
    client: &reqwest::Client,
    provider: &crate::config::Provider,
    model: &str,
    task: &str,
) -> Option<String> {
    let clip = |s: &str, n: usize| s.chars().take(n).collect::<String>();
    let angles = [
        ("A", "最直接稳妥的方案：步骤最少、依赖最少、最快给出可用结果"),
        ("B", "最周全严谨的方案：覆盖边界情况、风险与备选路径"),
        ("C", "最有创意的方案：换一个不寻常但可能更优的切入点"),
    ];
    let prompts: Vec<String> = angles
        .iter()
        .map(|(tag, angle)| {
            format!(
                "你是方案规划专家。针对用户任务，只输出「方案{tag}」：{angle}。用简洁的编号步骤（最多 5 步），不要寒暄，不要输出方案 {tag} 以外的内容。"
            )
        })
        .collect();
    let mut futs = Vec::new();
    for p in &prompts {
        futs.push(chat::ask_once(client, provider, model, p, task, 700));
    }
    let results = futures_util::future::join_all(futs).await;
    let mut cands: Vec<(char, String)> = Vec::new();
    for ((tag, _), r) in angles.iter().zip(results) {
        if let Ok(text) = r {
            let text = text.trim();
            if !text.is_empty() {
                let tag_ch = tag.chars().next().unwrap_or('A');
                cands.push((tag_ch, clip(text, TOT_CANDIDATE_CAP)));
            }
        }
    }
    if cands.is_empty() {
        return None;
    }
    // judge: pick the best candidate; parse failure falls back to A
    let listing = cands
        .iter()
        .map(|(tag, text)| format!("方案{tag}：{text}"))
        .collect::<Vec<_>>()
        .join("\n\n");
    let judge_prompt = format!(
        "任务：{}\n\n三个候选方案：\n{listing}\n\n你是评审专家。选出最有希望成功的方案，只输出 JSON：{{\"best\":\"A\"|\"B\"|\"C\",\"reason\":\"一句话理由\"}}",
        clip(task, 1_200)
    );
    let verdict = chat::ask_once(
        client,
        provider,
        model,
        "你是评审专家，只输出 JSON，不要输出其他内容。",
        &judge_prompt,
        200,
    )
    .await
    .ok();
    let (best, reason) = match verdict.as_deref().map(str::trim) {
        Some(v) => {
            // tolerate markdown fences / surrounding prose: take {..}
            let json_txt = match (v.find('{'), v.rfind('}')) {
                (Some(s), Some(e)) if e > s => &v[s..=e],
                _ => v,
            };
            match serde_json::from_str::<serde_json::Value>(json_txt) {
                Ok(j) => {
                    let b = j
                        .get("best")
                        .and_then(|x| x.as_str())
                        .and_then(|s| s.chars().next())
                        .map(|c| c.to_ascii_uppercase())
                        .unwrap_or('A');
                    let b = if cands.iter().any(|(tag, _)| *tag == b) { b } else { 'A' };
                    let r = j.get("reason").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
                    (b, r)
                }
                Err(_) => ('A', String::new()),
            }
        }
        None => ('A', String::new()),
    };
    let mut block = String::from("\n\n[深度推理预演（ToT）— 三方案并行生成，评审选优]\n");
    for (tag, text) in &cands {
        block.push_str(&format!("方案{tag}：{text}\n\n"));
    }
    if reason.is_empty() {
        block.push_str(&format!("评审结论：采用方案{best}。\n\n请按选中的方案作答（可吸收其他方案的优点）。"));
    } else {
        block.push_str(&format!("评审结论：采用方案{best} —— {reason}\n\n请按选中的方案作答（可吸收其他方案的优点）。"));
    }
    Some(block)
}

/// Garnish cap per review expert (chars).
const REVIEW_EXPERT_CAP: usize = 1_500;

/// Review mode (better-harness style findings pre-review): three
/// mutually-exclusive read-only experts review in parallel — correctness,
/// security boundaries, maintainability/test gaps — each emitting one-line
/// findings (severity / consequence / root cause / location / verifier).
/// The model later acts as the Lead (dedupe + grade + publish), so there is
/// deliberately NO judge call here. Returns a text block to append to the
/// user message actually SENT this turn (same garnish mechanism as ToT).
/// Any failure returns None: review mode must never block a normal send.
async fn run_review_rehearsal(
    client: &reqwest::Client,
    provider: &crate::config::Provider,
    model: &str,
    task: &str,
    diff: Option<&str>,
    data_dir: &std::path::Path,
) -> Option<String> {
    let clip = |s: &str, n: usize| s.chars().take(n).collect::<String>();
    // Expert lenses can be overridden by <data_dir>/agents/review-*.md
    // (frontmatter optional; body = role instructions). Missing/empty file
    // falls back to the built-in lens.
    let lens_or_file = |stem: &str, fallback: &str| -> String {
        std::fs::read_to_string(data_dir.join("agents").join(format!("{stem}.md")))
            .ok()
            .map(|raw| {
                let body = match raw.strip_prefix("---") {
                    Some(rest) => match rest.find("\n---") {
                        Some(i) => rest[i + 4..].to_string(),
                        None => raw.clone(),
                    },
                    None => raw.clone(),
                };
                let b = body.trim().to_string();
                if b.is_empty() {
                    fallback.to_string()
                } else {
                    b
                }
            })
            .unwrap_or_else(|| fallback.to_string())
    };
    let experts = [
        ("A", "正确性", lens_or_file("review-correctness", "正确性与逻辑缺陷：边界条件、错误处理、并发/时序、空值与溢出")),
        ("B", "安全边界", lens_or_file("review-security", "安全边界：注入、SSRF、密钥泄露、越权访问、不可信输入未校验")),
        ("C", "可维护性", lens_or_file("review-maintainability", "可维护性与测试缺口：重复逻辑、复杂度失控、命名误导、缺失测试")),
    ];
    let subject = match diff {
        Some(d) => format!(
            "{}\n\n--- 待审阅的未提交变更（git diff HEAD）---\n{}",
            clip(task, 1_200),
            d
        ),
        None => clip(task, 6_000),
    };
    let prompts: Vec<String> = experts
        .iter()
        .map(|(_tag, _label, prompt)| {
            format!(
                "你是代码审阅专家（只读视角，只负责：{prompt}）。针对待审阅内容逐条输出发现，每条一行，格式严格为：\n- [严重度] 后果 ← 根因 @ 位置 → 验证方式\n其中严重度 ∈ 严重/主要/次要；位置写 文件:行号（无文件上下文时写 任务级）；验证方式一句话说明如何证实或复现。只输出本视角的发现，不要涉及其他视角，不要寒暄；没有发现则只输出「无发现」。"
            )
        })
        .collect();
    let mut futs = Vec::new();
    for p in &prompts {
        futs.push(chat::ask_once(client, provider, model, p, &subject, 800));
    }
    let results = futures_util::future::join_all(futs).await;
    let mut block = String::from("\n\n[三专家审阅预演 — 并行只读评审，待汇合定级]\n");
    let mut any = false;
    for ((tag, label, _), r) in experts.iter().zip(results) {
        let text = r.ok().map(|s| s.trim().to_string()).unwrap_or_default();
        if text.is_empty() {
            continue;
        }
        any = true;
        block.push_str(&format!("专家{tag}（{label}）：{}\n\n", clip(&text, REVIEW_EXPERT_CAP)));
    }
    if !any {
        return None;
    }
    block.push_str(
        "请按审阅模式规则汇合：先用只读工具核实，再输出去重定级后的最终发现表；无法核实的条目标注「待核」。",
    );
    Some(block)
}

async fn run_send(
    app: tauri::AppHandle,
    state: &State<'_, AppState>,
    session_id: String,
    content: String,
    skill_calls: Option<Vec<String>>,
    images: Vec<crate::prefix::ChatImage>,
    lanes: Vec<SessionBinding>,
    channel: tauri::ipc::Channel<StreamEvent>,
    arena: bool,
) -> Result<(), String> {
    if content.trim().is_empty() {
        return Err("空消息".into());
    }

    // cancellation flag for this session
    let stop = Arc::new(AtomicBool::new(false));
    state.stops.lock().unwrap_or_else(|p| p.into_inner()).insert(session_id.clone(), stop.clone());

    // persist the user message once (lane 0); skill names are validated
    // against what is actually installed so stale chips cannot persist
    let cfg = config::load(&state.data_dir);
    let system = cfg.settings.system_prompt.clone();
    let ws = state.store.load(&session_id).ok().and_then(|sf| sf.meta.workspace.clone());
    let valid_skills: std::collections::HashSet<String> = crate::skills::scan(ws.as_deref())
        .into_iter()
        .map(|s| s.name)
        .collect();
    let skill_calls: Vec<String> = skill_calls
        .unwrap_or_default()
        .into_iter()
        .filter(|n| valid_skills.contains(n))
        .collect();
    // attached images land in the session's attachment dir BEFORE the record
    // is written — the record only carries immutable relative filenames, so
    // rebuilds re-reading the files produce byte-identical requests
    let image_names = if images.is_empty() {
        Vec::new()
    } else {
        crate::sessions::save_attachments(&state.data_dir, &session_id, &images)?
    };
    let user_record = MessageRecord {
        id: Uuid::new_v4().to_string(),
        lane: 0,
        role: "user".into(),
        content: content.clone(),
        reasoning: None,
        ts: next_record_ts(&state),
        model: None,
        status: "ok".into(),
        usage: None,
        cost_usd: None,
        confidence: None,
        tool_calls: None,
        tool_call_id: None,
        skill_calls: if skill_calls.is_empty() { None } else { Some(skill_calls) },
        workflow: record_workflow_of(&workflow_of_in(&state, &session_id, &state.data_dir)),
        images: image_names,
    };
    {
        let _guard = state.save_lock.lock().await;
        let mut sf = state.store.load(&session_id)?;
        sf.messages.push(user_record.clone());
        sf.meta.updated_at = now_ms();
        state.store.save(&sf)?;
    }

    // boundary compaction fires here — only at the user boundary, only for
    // chat sessions, only when the last request approached the window
    if !arena {
        maybe_auto_compact(state, &session_id, &cfg, &stop).await;
    }
    // MCP handshakes are lazy — warm them before lanes spawn so the first
    // model turn sees the full tool surface
    if cfg.mcp_servers.iter().any(|s| s.enabled) {
        ensure_mcp_servers(&state.data_dir, &stop).await;
    }

    // one task per lane
    let mut handles = Vec::new();
    for (lane_idx, binding) in lanes.iter().enumerate() {
        let lane = lane_idx as u32;
        let binding = binding.clone();
        let session_id = session_id.clone();
        let channel = channel.clone();
        let stop = stop.clone();
        let save_lock = state.save_lock.clone();
        let data_dir = state.data_dir.clone();
        // 聊天流式走 read_idle 客户端：没有总时长上限，上游 90s 无字节
        // 才判死（image_generate 自带 per-request 300s，不受影响）
        let client = state.stream_client.clone();
        let system = system.clone();
        let app = app.clone();

        let handle = tauri::async_runtime::spawn(async move {
            // the lane task outlives the borrow of `state` — re-resolve the
            // managed state from the (moved, 'static) app handle instead
            let state = app.state::<AppState>();
            let cfg = config::load(&data_dir);
            let provider = match resolve_provider(&cfg, &binding) {
                Some(p) => p.clone(),
                None => {
                    let _ = channel.send(StreamEvent::Error {
                        lane,
                        message: "Provider 未配置或未填 API Key".into(),
                    });
                    return;
                }
            };
            let model = binding.model.clone();
            // a new turn cancels any pending keepalive probe on this lane —
            // it must never fire between our pre-stream phases and requests
            crate::warmer::cancel(&session_id, lane);
            let store = SessionStore::new(&data_dir);

            // workspace + layered Zone S (identity → user global → AGENTS.md → env).
            // Worktree isolation: while active, every tool — read, write,
            // approval preview, write-log snapshot — operates inside the
            // session's worktree; the main checkout is untouched until the
            // user merges (or discards) from the composer capsule.
            let sf_loaded = store.load(&session_id).ok();
            let mut workspace = sf_loaded.as_ref().and_then(|sf| sf.meta.workspace.clone());
            if let Some(wt) = sf_loaded.as_ref().and_then(|sf| sf.meta.wt.clone()) {
                if std::path::Path::new(&wt.path).is_dir() {
                    workspace = Some(wt.path);
                }
            }
            let perm_base = permission_of(&state, &session_id);
            // sandbox: the risky "auto" write tier degrades to per-action
            // approval — fail-safe rather than convenient. 文件白名单的可信
            // 路径在自动模式下保持免审批（见写路径的 trusted 判定）。
            let sb = crate::agent_tools::sandbox_policy();
            let perm_mode = if sb.on && perm_base == "auto" { "approve" } else { perm_base };
            // plan gate: read-only tool surface, no MCP, no writes — the
            // directive itself rides on the user message (see transcript_for_lane).
            // goal gate: full surface, but more tool rounds per turn.
            // image gate: handled below via chat::image_generate — the turn
            // never enters the chat pipeline at all.
            let wf = workflow_of_in(&state, &session_id, &data_dir);
            let plan_mode = wf == "plan";
            let goal_mode = wf == "goal";
            let deep_mode = wf == "deep";
            let review_mode = wf == "review";
            let image_mode = wf == "image";
            // declarative state machine: resolve the current state (if the
            // def was deleted or redefined since the gate was set, degrade
            // gracefully to an unrestricted agent turn)
            let sm_state = match wf.strip_prefix("sm:").and_then(|r| r.split_once(':')) {
                Some((def_id, state_name)) => {
                    chat::resolve_sm(&cfg.workflows, def_id, state_name)
                }
                None => None,
            };
            let sm_tools: Option<&str> = sm_state.as_ref().map(|(_, st)| st.tools.as_str());
            let sm_no_tools = sm_tools == Some("none");
            let sm_ro_tools = sm_tools == Some("readonly");
            let system_full = crate::sysprompt::assemble(&system, workspace.as_deref());
            let tools_on = cfg.settings.agent_tools && workspace.is_some() && !sm_no_tools;
            let mcp_enabled = cfg.settings.agent_tools
                && !plan_mode
                && !review_mode
                && !sm_no_tools
                && !sm_ro_tools
                && cfg.mcp_servers.iter().any(|s| s.enabled);
            let mut schema_all: Vec<Value> = Vec::new();
            if tools_on {
                schema_all.extend(
                    match perm_mode == "readonly" || plan_mode || review_mode || sm_ro_tools {
                        true => crate::agent_tools::schema_readonly().as_array().cloned().unwrap_or_default(),
                        false => crate::agent_tools::schema().as_array().cloned().unwrap_or_default(),
                    },
                );
            }
            if mcp_enabled {
                schema_all.extend(state_mcp_tools(&data_dir));
            }
            // advertise enabled named subagents in the delegate tool so the
            // model can pick `agent` meaningfully (settings + file-defined)
            if tools_on && !plan_mode {
                let names: Vec<String> = all_subagents(&cfg, &data_dir)
                    .iter()
                    .filter(|p| p.enabled)
                    .map(|p| {
                        if p.description.trim().is_empty() {
                            p.name.clone()
                        } else {
                            format!("{}（{}）", p.name, p.description.trim())
                        }
                    })
                    .collect();
                if !names.is_empty() {
                    if let Some(tool) = schema_all.iter_mut().find(|t| {
                        t.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str())
                            == Some("delegate_subagent")
                    }) {
                        tool["function"]["description"] = Value::String(format!(
                            "把一个相对独立的调研/分析子任务委派给后台子智能体（独立上下文、只读工具），完成后以结论回报。task 需自包含。当前可用的具名子智能体（agent 参数填名字）：{}。不指定 agent 时使用主会话的模型配置。",
                            names.join("；")
                        ));
                    }
                }
            }
            // goal lifecycle tools (Codex /goal parity): the model can read
            // the goal, create one (flips the gate), and declare achieved /
            // unmet. Session-scoped — they work even without a workspace,
            // so they ride regardless of tools_on.
            if goal_mode {
                schema_all.push(serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": "get_goal",
                        "description": "查看当前会话的目标状态：目标内容、生命周期状态（active/paused/achieved/unmet/budget_limited）与验收清单进度（x/y、是否已全部满足）。",
                        "parameters": {"type": "object", "properties": {}, "required": []}
                    }
                }));
                schema_all.push(serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": "create_goal",
                        "description": "创建（或替换）当前会话的目标并进入目标模式。objective 需自包含且可映射为可验证的验收清单：一句话目标 + 范围（Scope）+ 硬性约束（Constraints）+ 验收标准（Done when，每条可验证）+ 停止条件（Stop if）。避免「全部/所有/彻底/improve」这类无法映射成清单的虚词。",
                        "parameters": {
                            "type": "object",
                            "properties": {
                                "objective": {
                                    "type": "string",
                                    "description": "目标全文（含范围/约束/验收标准/停止条件）"
                                }
                            },
                            "required": ["objective"]
                        }
                    }
                }));
                schema_all.push(serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": "update_goal",
                        "description": "声明目标达成（status=achieved）或判定无法达成（status=unmet）。调用前必须完成审计：逐条核对验收标准并确认每条 ✅ 都有可核验证据（文件路径/命令输出/测试名）；代理信号（测试通过、代码写完）不能单独作为依据；不确定视作未达成。暂停/恢复/清除不可通过此工具操作。",
                        "parameters": {
                            "type": "object",
                            "properties": {
                                "status": {"type": "string", "enum": ["achieved", "unmet"], "description": "achieved=目标达成；unmet=判定无法达成（需说明原因）"}
                            },
                            "required": ["status"]
                        }
                    }
                }));
            }
            let tools_schema = if schema_all.is_empty() {
                None
            } else {
                Some(Value::Array(schema_all))
            };

            // ---- prefix state (synchronous, brief lock) ----
            let cache_key = format!("ccharness-{session_id}-{lane}");
            // privacy scrub context: outbound messages get type-consistent
            // surrogates; records keep the originals (see privacy.rs)
            let privacy_on = cfg.settings.privacy_mode;
            let pseed = crate::privacy::session_seed(&data_dir, &session_id);
            let scrub_outbound = |msgs: &mut [crate::prefix::ChatMessage]| {
                if privacy_on {
                    for m in msgs.iter_mut() {
                        m.content = crate::privacy::outbound(&session_id, &pseed, &m.content);
                    }
                }
            };
            let sf_snapshot = match store.load(&session_id) {
                Ok(sf) => sf,
                Err(e) => {
                    let _ = channel.send(StreamEvent::Error { lane, message: e });
                    return;
                }
            };

            // ---- image gate: this turn goes to POST {base}/images/generations,
            // not the chat pipeline — no prefix state, no tools, no telemetry
            // ledger entry (the image API has no chat usage to record) ----
            if image_mode {
                let prompt = sf_snapshot
                    .messages
                    .iter()
                    .rev()
                    .find(|m| m.role == "user")
                    .map(|m| m.content.clone())
                    .unwrap_or_default();
                let message_id = Uuid::new_v4().to_string();
                let ctx = SendCtx {
                    client: &client,
                    provider: &provider,
                    model: &model,
                    lane,
                    message_id: message_id.clone(),
                    channel: channel.clone(),
                    stop: stop.clone(),
                    progress_tap: None,
                    // image generation has no prefix cache — no affinity
                    affinity: None,
                };
                let outcome = match chat::image_generate(&ctx, &prompt).await {
                    Ok(o) => o,
                    Err(e) => {
                        let _ = channel.send(StreamEvent::Error { lane, message: e });
                        let _ = channel.send(StreamEvent::Done {
                            lane,
                            message_id,
                            status: "error".into(),
                            confidence: None,
                        });
                        return;
                    }
                };
                let record = MessageRecord {
                    id: message_id.clone(),
                    lane,
                    role: "assistant".into(),
                    reasoning: None,
                    content: outcome.content.clone(),
                    ts: next_record_ts(&state),
                    model: Some(model.clone()),
                    status: outcome.status.clone(),
                    usage: None,
                    cost_usd: None,
                    confidence: None,
                    tool_calls: None,
                    tool_call_id: None,
                    skill_calls: None,
                    workflow: None,
                    images: Vec::new(),
                };
                {
                    let _guard = save_lock.lock().await;
                    if let Ok(mut sf) = store.load(&session_id) {
                        sf.messages.push(record);
                        sf.meta.updated_at = now_ms();
                        warn_save(&session_id, "assistant record", store.save(&sf));
                    }
                }
                let _ = channel.send(StreamEvent::Done {
                    lane,
                    message_id,
                    status: outcome.status,
                    confidence: None,
                });
                return;
            }

            let (owned_prefix, system_injection, memo_injection) = {
                let mut guard = prefixes_lock(&state);
                let map = guard.get_or_insert_with(HashMap::new);
                let key = (session_id.clone(), lane);
                let lp = map
                    .entry(key)
                    .or_insert_with(|| LanePrefix::new(&system_full, &cache_key));
                lp.bind_model(&model);
                // per-model behavior overrides (temperature / max_tokens /
                // reasoning) with global-thinking fallback, all epoch-gated
                let beh = provider.behavior.get(&model);
                lp.bind_behavior(beh.and_then(|b| b.temperature), beh.and_then(|b| b.max_output));
                let reasoning = beh
                    .and_then(|b| b.reasoning.as_deref())
                    .filter(|r| !r.is_empty() && *r != "default")
                    .or(if cfg.settings.thinking_level == "default" {
                        None
                    } else {
                        Some(cfg.settings.thinking_level.as_str())
                    });
                lp.bind_thinking(reasoning);
                // head-byte discipline: cache-tier toggle and MCP loadout
                // changes rewrite the head without touching Zone H — they
                // must bump the epoch (expected rebuild) rather than masquer
                // -ade as an upstream cache miss in telemetry
                lp.bind_cache_tier(provider.cache_tier());
                lp.bind_tools_hash(tools_hash(tools_schema.as_ref()));
                // system change (settings/workspace/AGENTS.md) or privacy-mode
                // toggle or restart recovery ⇒ rebuild Zone H from the
                // persisted transcript (re-scrubbed under the new flag) —
                // UNLESS in-history updates are on: then a system change
                // rides as an injected system message (below) and the head
                // keeps its cache identity
                let privacy_changed = lp.bind_privacy(privacy_on);
                let system_changed = !lp.system_is(&system_full);
                let in_history_ok = cfg.settings.system_update_mode == "in-history"
                    && provider.kind == crate::config::ProviderKind::OpenaiCompatible
                    && !privacy_on;
                let needs_rebuild = privacy_changed
                    || lp_is_empty(lp)
                    || (system_changed && !in_history_ok);
                // adopt only when NOT rebuilding: a fresh/rebuilt prefix
                // carries the current system text in Zone S already —
                // injecting it again would duplicate the prompt in the
                // same request (and then in Zone H at turn end)
                let mut system_injection: Option<String> = None;
                if !needs_rebuild
                    && system_changed
                    && in_history_ok
                    && lp.system_needs_in_history(&system_full)
                {
                    // NOTE: the adoption is NOT booked here — same discipline
                    // as the memo watermark below. Booking at prep time let a
                    // failed/stopped turn claim the update was injected when
                    // its message never entered Zone H, silently losing the
                    // system change until a rebuild. Booking happens right
                    // after the Zone H append at turn end.
                    system_injection = Some(system_full.clone());
                }
                if needs_rebuild {
                    let mut hist = chat::transcript_for_lane(&sf_snapshot, lane, &cfg.workflows, &data_dir);
                    scrub_outbound(&mut hist);
                    if hist.len() > 1 {
                        lp.rebuild(&system_full, &hist[..hist.len() - 1]);
                    }
                }
                // RollingMemo injection (L6 §4.3): after a rebuild the
                // re-created Zone H carries no memo messages, so re-inject
                // once; otherwise inject when the memo changed since the
                // last injection (rev watermark). The message is not
                // persisted — it falls into Zone H at turn end, and a
                // restart re-injects via the rebuild branch above. Privacy
                // mode scrubs the rendered block like any outbound text.
                let memo_injection = if lane == 0 {
                    let rev = sf_snapshot.meta.rolling_memo_rev;
                    let injected = sf_snapshot.meta.rolling_memo_injected_rev;
                    match sf_snapshot.meta.rolling_memo.as_ref().and_then(render_memo) {
                        Some(text) if needs_rebuild || rev > injected => Some(if privacy_on {
                            crate::privacy::outbound(&session_id, &pseed, &text)
                        } else {
                            text
                        }),
                        _ => None,
                    }
                } else {
                    None
                };
                (lp.clone(), system_injection, memo_injection)
            };
            // NOTE: the injection watermark does NOT advance here — it used
            // to, but the memo only enters Zone H when the turn actually
            // lands; a failed/stopped turn never wrote the memo into Zone H,
            // yet the watermark already claimed it was injected, so the
            // memory silently vanished until the next rebuild. The watermark
            // advances right after the Zone H append at turn end instead.

            let mut transcript = chat::transcript_for_lane(&sf_snapshot, lane, &cfg.workflows, &data_dir);
            scrub_outbound(&mut transcript);
            if transcript.is_empty() {
                let _ = channel.send(StreamEvent::Error { lane, message: "会话内容为空".into() });
                return;
            }

            // Everything sent beyond Zone H this turn, in order. The next
            // request replays it verbatim; at turn end it falls into Zone H.
            let mut sent_this_turn: Vec<ChatMessage> = vec![transcript.last().unwrap().clone()];
            // RollingMemo rides ahead of the user turn (L6 §4.3). Inserted
            // BEFORE the system update below so the fixed wire order is
            // [system update, memo, user] — deterministic across replays.
            // Not persisted; falls into Zone H at turn end via the append.
            if let Some(text) = &memo_injection {
                sent_this_turn.insert(0, ChatMessage::plain("system", text.clone()));
            }
            // in-history system update: the new system text rides ahead of
            // the user turn. The message is NOT persisted — restarts rebuild
            // Zone S from the current settings instead — and it falls into
            // Zone H at turn end via the append below, keeping the head
            // byte-stable for the whole run.
            if let Some(sys) = &system_injection {
                sent_this_turn.insert(0, ChatMessage::plain("system", sys.clone()));
            }
            // Files API image reuse (opt-in per provider): resolve every
            // attached image to a server-side file id once, then reference
            // it by id — the base64 payload never re-enters the request.
            // Upload failures fall back to inline data URIs.
            if provider.images_via_files
                && provider.kind == crate::config::ProviderKind::OpenaiCompatible
                && sent_this_turn.iter().any(|m| !m.images.is_empty())
            {
                crate::deepfiles::ensure_file_refs(
                    &client,
                    &data_dir,
                    &provider.id,
                    &provider.base_url,
                    &provider.api_key,
                    &mut sent_this_turn,
                )
                .await;
            }
            let mut last_confidence: Option<u32> = None;
            // The assistant bubble is created BEFORE the slow pre-stream
            // phases below (memory recall / deep rehearsal / review experts)
            // so those turns show a message card immediately instead of
            // looking frozen. The first stream round reuses this id and the
            // frontend dedupes Started events per lane, so exactly one
            // bubble appears per lane per turn.
            let first_message_id = Uuid::new_v4().to_string();
            let mut last_message_id = first_message_id.clone();
            let has_pre_phase = (cfg.settings.vector_memory
                && !cfg.settings.embeddings_url.trim().is_empty())
                || deep_mode
                || review_mode;
            if has_pre_phase {
                let _ = channel.send(StreamEvent::Started {
                    lane,
                    model: model.clone(),
                    message_id: first_message_id.clone(),
                });
            }
            let mut turn_status = "error".to_string();
            // per-turn delegation budget for delegate_subagent
            let mut delegations = 0usize;
            // SM conditional-branch + parallel-fanout bookkeeping: tools
            // invoked this turn, accumulated reply text (for contains:
            // predicates), and the one-shot fan-out guard
            let mut turn_tools: Vec<String> = Vec::new();
            let mut turn_text = String::new();
            let mut fanout_done = false;

            // Vector memory recall: attach the top-k semantically relevant
            // memories as a suffix on the user message actually SENT this
            // turn. The persisted record stays clean; within the turn every
            // replay uses the same suffixed bytes, and at turn end the
            // suffixed message falls into Zone H — restarts simply rebuild a
            // new epoch without the garnish.
            if cfg.settings.vector_memory && !cfg.settings.embeddings_url.trim().is_empty() {
                // find the user message by role: with an in-history system
                // injection, sent_this_turn[0] may be the injected system
                // message instead
                let user_text = sent_this_turn
                    .iter()
                    .find(|m| m.role == "user")
                    .map(|m| m.content.clone())
                    .unwrap_or_default();
                let ws = sf_snapshot.meta.workspace.clone().unwrap_or_default();
                if !user_text.is_empty() && !ws.is_empty() {
                    let _ = channel.send(StreamEvent::Reasoning {
                        lane,
                        message_id: first_message_id.clone(),
                        text: "正在检索相关的长期记忆…\n\n".to_string(),
                    });
                    let recall =
                        crate::memvector::recall(&client, &cfg, &data_dir, &ws, &user_text, 3);
                    // stop-aware: a click during the recall ends the turn
                    // instead of being ignored until the first token
                    tokio::select! {
                        _ = wait_stopped(&stop) => {
                            abort_lane_pre_stream(&state, &channel, &session_id, lane, &first_message_id).await;
                            return;
                        }
                        mems = recall => {
                            if !mems.is_empty() {
                                let block = format!(
                                    "\n\n[长期记忆参考 — 与本条消息语义相关的既往记忆]\n{}",
                                    mems.iter().map(|m| format!("- {m}")).collect::<Vec<_>>().join("\n")
                                );
                                if let Some(user_msg) = sent_this_turn.iter_mut().find(|m| m.role == "user") {
                                    user_msg.content.push_str(&block);
                                }
                            }
                        }
                    }
                }
            }

            // Deep reasoning (ToT-style rehearsal): three candidate
            // approaches are generated in parallel, an LLM judge picks one,
            // and the whole rehearsal is appended to the user message
            // actually SENT this turn — same garnish mechanism as the memory
            // recall above (persisted record stays clean; restarts rebuild
            // a new epoch without it). Any rehearsal failure just proceeds
            // without it.
            if deep_mode && !sent_this_turn.is_empty() {
                let task_text = sent_this_turn
                    .iter()
                    .find(|m| m.role == "user")
                    .map(|m| m.content.clone())
                    .unwrap_or_default();
                let _ = channel.send(StreamEvent::Reasoning {
                    lane,
                    message_id: first_message_id.clone(),
                    text: "正在并行预演三个候选方案（深度推理两阶段：三方案生成 + 评审选优）…\n\n".to_string(),
                });
                tokio::select! {
                    _ = wait_stopped(&stop) => {
                        abort_lane_pre_stream(&state, &channel, &session_id, lane, &first_message_id).await;
                        return;
                    }
                    block = run_tot_rehearsal(&client, &provider, &model, &task_text) => {
                        if let Some(block) = block {
                            if let Some(user_msg) = sent_this_turn.iter_mut().find(|m| m.role == "user") {
                                user_msg.content.push_str(&block);
                            }
                        }
                    }
                }
            }

            // Review mode (better-harness style): three mutually-exclusive
            // read-only experts pre-review in parallel (correctness / security
            // / maintainability); the model itself reconciles their findings
            // into the final graded table. Same garnish mechanism as ToT.
            if review_mode && !sent_this_turn.is_empty() {
                let task_text = sent_this_turn
                    .iter()
                    .find(|m| m.role == "user")
                    .map(|m| m.content.clone())
                    .unwrap_or_default();
                let diff = workspace.as_deref().and_then(|ws| {
                    crate::worktree::git(std::path::Path::new(ws), &["diff", "HEAD"])
                        .ok()
                        .map(|d| d.chars().take(6_000).collect::<String>())
                        .filter(|d| !d.trim().is_empty())
                });
                let _ = channel.send(StreamEvent::Reasoning {
                    lane,
                    message_id: first_message_id.clone(),
                    text: "正在并行预审三个只读专家（正确性 / 安全性 / 可维护性）…\n\n".to_string(),
                });
                // stop-aware: a click during the pre-review ends the turn
                // instead of being ignored until the first token
                tokio::select! {
                    _ = wait_stopped(&stop) => {
                        abort_lane_pre_stream(&state, &channel, &session_id, lane, &first_message_id).await;
                        return;
                    }
                    block = run_review_rehearsal(&client, &provider, &model, &task_text, diff.as_deref(), &data_dir) => {
                        if let Some(block) = block {
                            if let Some(user_msg) = sent_this_turn.iter_mut().find(|m| m.role == "user") {
                                user_msg.content.push_str(&block);
                            }
                        }
                    }
                }
            }

            // Stop clicked during (or between) the pre-stream phases: end the
            // turn before any request goes upstream. Covers the race where a
            // phase future resolves before the 120ms stop poll notices.
            if stop.load(Ordering::Relaxed) {
                abort_lane_pre_stream(&state, &channel, &session_id, lane, &first_message_id).await;
                return;
            }

            let max_rounds = if goal_mode { GOAL_MAX_TOOL_ROUNDS } else { MAX_TOOL_ROUNDS };
            for _round in 1..=max_rounds {
                let body = chat::build_body(
                    &provider,
                    &model,
                    &owned_prefix,
                    &sent_this_turn,
                    &system_full,
                    tools_schema.as_ref(),
                );
                let prefix_bytes = owned_prefix.prefix_bytes_public();
                // tail bytes measured exactly like the request: serialized
                // message JSON + separator — same units as prefix_bytes
                let added_bytes = sent_this_turn.iter().map(|m| message_json(m).len() + 1).sum::<usize>();
                let total_bytes = prefix_bytes + added_bytes;
                // digest-chain continuity: total bytes sent must grow
                // monotonically (append-only) within an epoch; a new epoch is
                // an expected rebuild, not a break
                let chain_ok = {
                    let mut guard = state.last_span.lock().unwrap_or_else(|p| p.into_inner());
                    let map = guard.get_or_insert_with(HashMap::new);
                    let prev = map.insert((session_id.clone(), lane), (owned_prefix.epoch, total_bytes));
                    match prev {
                        None => true,
                        Some((prev_epoch, prev_total)) => {
                            prev_epoch != owned_prefix.epoch || total_bytes >= prev_total
                        }
                    }
                };

                let message_id = Uuid::new_v4().to_string();
                last_message_id = message_id.clone();
                let ctx = SendCtx {
                    client: &client,
                    provider: &provider,
                    model: &model,
                    lane,
                    message_id: message_id.clone(),
                    channel: channel.clone(),
                    stop: stop.clone(),
                    progress_tap: None,
                    // pi gating: the affinity header only makes sense while
                    // the tier still routes via prompt_cache_key (tier None
                    // sends no cache marks at all)
                    affinity: if provider.cache_tier() == crate::config::CacheTier::None {
                        None
                    } else {
                        Some(cache_key.clone())
                    },
                };
                let outcome = match chat::stream_lane(&ctx, body, chat::auth_for(&provider)).await {
                    Ok(o) => o,
                    Err(e) => {
                        let _ = channel.send(StreamEvent::Error { lane, message: e });
                        turn_status = "error".into();
                        break;
                    }
                };

                // per-round telemetry + significant-miss analysis (pi-runtime
                // parity): the request legitimately re-bills only its new
                // tail (added_bytes); everything beyond that was supposed to
                // be a cache read. Needs the provider to report cached tokens.
                let usage = outcome.usage.clone();
                let cost = chat::cost_of(&usage, &provider, &model);
                let mut stat = RequestStat {
                    seq: state.seq.fetch_add(1, Ordering::Relaxed),
                    ts: now_ms(),
                    lane,
                    model: model.clone(),
                    epoch: owned_prefix.epoch,
                    prefix_bytes,
                    added_bytes,
                    chain_ok,
                    input_tokens: usage.input,
                    cached_tokens: usage.cached,
                    output_tokens: usage.output,
                    cost_usd: cost,
                    significant_miss: false,
                    rebilled_tokens: 0,
                    rebilled_cost: None,
                    miss_cause: None,
                };
                let epoch_bumped = store
                    .load(&session_id)
                    .ok()
                    .and_then(|sf| sf.telemetry.iter().rev().find(|r| r.lane == lane).map(|r| r.epoch != owned_prefix.epoch))
                    .unwrap_or(false);
                let miss =
                    chat::analyze_cache_miss(&usage, &provider.kind, added_bytes, chain_ok, epoch_bumped);
                stat.miss_cause = Some(miss.cause.to_string());
                if miss.significant {
                    stat.significant_miss = true;
                    stat.rebilled_tokens = miss.rebilled_tokens;
                    stat.rebilled_cost = chat::rebill_cost(miss.rebilled_tokens, &usage, &provider, &model);
                }
                let _ = channel.send(StreamEvent::Usage {
                    lane,
                    message_id: message_id.clone(),
                    usage: usage.clone(),
                    request: stat.clone(),
                });

                // accumulate the turn's reply text for SM branch predicates
                if !turn_text.is_empty() {
                    turn_text.push_str("\n\n");
                }
                turn_text.push_str(&outcome.content);

                // persist this round's assistant record
                let has_tools = !outcome.tool_calls.is_empty();
                // SM parallel fan-out decision: on the clean lane-0 answer of
                // a workflow state whose `parallel` list is configured (and
                // not yet fanned out this turn), synthesize parallel_branch
                // tool calls so the assistant record/transcript stay coherent
                // with the subagent runs that follow — the same wire shape as
                // delegate_subagent, preserving prefix-cache alignment.
                let mut fan_calls: Vec<crate::types_rs::ToolCallWire> = Vec::new();
                if !has_tools
                    && lane == 0
                    && !fanout_done
                    && outcome.status == "ok"
                    && wf.starts_with("sm:")
                {
                    if let Some((def_id, state_name)) = wf[3..].split_once(':') {
                        if let Some((def, st)) = chat::resolve_sm(&cfg.workflows, def_id, state_name) {
                            if !st.terminal {
                                for (i, tgt) in st.parallel.iter().enumerate() {
                                    if def.states.iter().any(|s| &s.name == tgt) {
                                        fan_calls.push(crate::types_rs::ToolCallWire {
                                            id: format!("par-{message_id}-{i}"),
                                            name: "parallel_branch".into(),
                                            arguments: serde_json::json!({ "state": tgt }).to_string(),
                                        });
                                    }
                                }
                                fanout_done = !fan_calls.is_empty();
                            }
                        }
                    }
                }
                let tool_wire: Option<Vec<crate::types_rs::ToolCallWire>> = if has_tools {
                    Some(
                        outcome
                            .tool_calls
                            .iter()
                            .map(|tc| crate::types_rs::ToolCallWire {
                                id: tc.id.clone(),
                                name: tc.name.clone(),
                                arguments: tc.arguments.clone(),
                            })
                            .collect(),
                    )
                } else if !fan_calls.is_empty() {
                    Some(fan_calls.clone())
                } else {
                    None
                };
                let pending_call_ids: Vec<String> = tool_wire
                    .as_ref()
                    .map(|w| w.iter().map(|tc| tc.id.clone()).collect())
                    .unwrap_or_default();
                let asst_msg = ChatMessage {
                    role: "assistant".into(),
                    content: outcome.content.clone(),
                    tool_calls: tool_wire.as_ref().map(|w| chat::tool_calls_wire_value(w)),
                    tool_call_id: None,
                    images: Vec::new(),
                };
                // privacy: the record keeps the RESTORED text (user reads
                // real values); the live wire message above keeps the model's
                // own bytes (surrogates) so Zone H stays byte-stable — a
                // restart rebuild re-scrubs the restored text to the very
                // same surrogates (deterministic mapping).
                let (restored_content, restored_reasoning) = if privacy_on {
                    (
                        crate::privacy::restore(&session_id, &outcome.content),
                        if outcome.reasoning.is_empty() {
                            None
                        } else {
                            Some(crate::privacy::restore(&session_id, &outcome.reasoning))
                        },
                    )
                } else {
                    (outcome.content.clone(), if outcome.reasoning.is_empty() { None } else { Some(outcome.reasoning.clone()) })
                };
                let record = MessageRecord {
                    id: message_id,
                    lane,
                    role: "assistant".into(),
                    reasoning: restored_reasoning,
                    content: restored_content,
                    ts: next_record_ts(&state),
                    model: Some(model.clone()),
                    status: outcome.status.clone(),
                    usage: Some(usage),
                    cost_usd: cost,
                    confidence: outcome.confidence,
                    tool_calls: tool_wire,
                    tool_call_id: None,
                    skill_calls: None,
                    workflow: None,
                    images: Vec::new(),
                };
                last_confidence = outcome.confidence;
                {
                    let _guard = save_lock.lock().await;
                    if let Ok(mut sf) = store.load(&session_id) {
                        sf.messages.push(record);
                        // significant-miss notice: a user-facing record that
                        // transcript_for_lane excludes from model context —
                        // the model never sees it, so Zone H stays stable
                        if stat.significant_miss && sf.meta.kind == "chat" {
                            let cost_part = stat
                                .rebilled_cost
                                .map(|c| format!("，冤枉钱 ≈ ${:.4}", c))
                                .unwrap_or_default();
                            let hint = if stat.miss_cause.as_deref() == Some("client") {
                                "本地前缀链断裂——有组件改写了历史请求字节，请检查本轮的模式/上下文变更。"
                            } else {
                                "可能原因：闲置超过缓存 TTL（命中会续期）、上游逐出或网关路由变化。持续对话命中率会回升。"
                            };
                            sf.messages.push(MessageRecord {
                                id: Uuid::new_v4().to_string(),
                                lane,
                                role: "notice".into(),
                                content: format!(
                                    "缓存显著未命中：本轮输入 {} tokens 仅命中 {}，重计费 ≈ {} tokens{}。{}",
                                    stat.input_tokens.unwrap_or(0),
                                    stat.cached_tokens.unwrap_or(0),
                                    stat.rebilled_tokens,
                                    cost_part,
                                    hint
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
                        }
                        // stopped/errored mid-round: pair every emitted tool
                        // call with a synthetic "not executed" result so a
                        // restart rebuild never carries an unpaired tool_calls
                        // assistant record (upstream answers 400 to that shape
                        // and the session stays poisoned until compaction)
                        if outcome.status != "ok" {
                            for cid in &pending_call_ids {
                                {
                                    sf.messages.push(MessageRecord {
                                        id: Uuid::new_v4().to_string(),
                                        lane,
                                        role: "tool".into(),
                                        content: crate::chat::UNPAIRED_TOOL_NOTE.into(),
                                        reasoning: None,
                                        ts: next_record_ts(&state),
                                        model: None,
                                        status: "ok".into(),
                                        usage: None,
                                        cost_usd: None,
                                        confidence: None,
                                        tool_calls: None,
                                        tool_call_id: Some(cid.clone()),
                                        skill_calls: None,
                                        workflow: None,
                                        images: Vec::new(),
                                    });
                                }
                            }
                        }
                        sf.telemetry.push(stat);
                        sf.meta.updated_at = now_ms();
                        warn_save(&session_id, "usage record", store.save(&sf));
                    }
                }

                if outcome.status != "ok" {
                    // byte-parity with the restart rebuild: the persisted
                    // transcript now carries the partial assistant (with its
                    // tool_calls) plus the synthetic "not executed" tool
                    // results above — Zone H must carry exactly the same
                    // records, or the next live request misses the
                    // guarantee-cache while a restart shifts the context
                    if outcome.status == "stopped" {
                        sent_this_turn.push(asst_msg);
                        for cid in &pending_call_ids {
                            sent_this_turn.push(ChatMessage {
                                role: "tool".into(),
                                content: crate::chat::UNPAIRED_TOOL_NOTE.into(),
                                tool_calls: None,
                                tool_call_id: Some(cid.clone()),
                                images: Vec::new(),
                            });
                        }
                    }
                    turn_status = outcome.status;
                    break;
                }
                if !has_tools {
                    sent_this_turn.push(asst_msg);
                    if fan_calls.is_empty() {
                        turn_status = "ok".into();
                        break;
                    }
                    // ---- SM parallel fan-out: surface the synthesized
                    // parallel_branch calls, run every configured parallel
                    // state concurrently as a subagent, report the results as
                    // tool messages, then loop for a synthesis round (the
                    // model sees its "calls" answered).
                    let def_id_fan = wf[3..].split_once(':').map(|(d, _)| d.to_string()).unwrap_or_default();
                    for fc in &fan_calls {
                        let st_name = serde_json::from_str::<Value>(&fc.arguments)
                            .ok()
                            .and_then(|a| a.get("state").and_then(|t| t.as_str()).map(|s| s.to_string()))
                            .unwrap_or_default();
                        let _ = channel.send(StreamEvent::ToolCall {
                            lane,
                            call_id: fc.id.clone(),
                            name: fc.name.clone(),
                            args: format!("state={st_name}"),
                        });
                    }
                    let mut fan_tasks: Vec<(String, String)> = Vec::new(); // (call_id, task)
                    for fc in &fan_calls {
                        let st_name = serde_json::from_str::<Value>(&fc.arguments)
                            .ok()
                            .and_then(|a| a.get("state").and_then(|t| t.as_str()).map(|s| s.to_string()))
                            .unwrap_or_default();
                        let directive = chat::resolve_sm(&cfg.workflows, &def_id_fan, &st_name)
                            .and_then(|(_, st2)| {
                                let d = st2.directive.trim();
                                if d.is_empty() { None } else { Some(d.to_string()) }
                            })
                            .unwrap_or_default();
                        let task = if directive.is_empty() {
                            format!("【工作流并行分支 · {st_name}】请执行该分支状态的工作流任务，完成后输出结果摘要。")
                        } else {
                            format!("【工作流并行分支 · {st_name}】{directive}\n\n完成后输出结果摘要。")
                        };
                        fan_tasks.push((fc.id.clone(), task));
                    }
                    let mut futs = Vec::new();
                    for (cid, task) in &fan_tasks {
                        futs.push(run_subagent(
                            &state,
                            &client,
                            &data_dir,
                            &session_id,
                            task,
                            None,
                            &binding,
                            &cfg,
                            &channel,
                            lane,
                            cid,
                        ));
                    }
                    let results = futures_util::future::join_all(futs).await;
                    for (fc, res) in fan_calls.iter().zip(results.into_iter()) {
                        let st_name = serde_json::from_str::<Value>(&fc.arguments)
                            .ok()
                            .and_then(|a| a.get("state").and_then(|t| t.as_str()).map(|s| s.to_string()))
                            .unwrap_or_default();
                        let mut result = match res {
                            Ok(summary) => format!("【并行分支 {st_name} 完成】\n\n{summary}"),
                            Err(e) => format!("ERROR: 并行分支 {st_name} 失败: {e}"),
                        };
                        // context-volume: spill before the privacy scrub so
                        // the file keeps real values while the record keeps
                        // the trimmed form the model actually sees
                        result = crate::spill::maybe_spill(
                            &session_id,
                            &fc.name,
                            &result,
                            cfg.settings.spill_max_chars,
                        );
                        if privacy_on {
                            result = crate::privacy::outbound(&session_id, &pseed, &result);
                        }
                        let preview: String =
                            result.lines().next().unwrap_or("").chars().take(120).collect();
                        let _ = channel.send(StreamEvent::ToolResult {
                            lane,
                            call_id: fc.id.clone(),
                            name: fc.name.clone(),
                            result: preview,
                        });
                        let tool_record = MessageRecord {
                            id: Uuid::new_v4().to_string(),
                            lane,
                            role: "tool".into(),
                            content: result.clone(),
                            reasoning: None,
                            ts: next_record_ts(&state),
                            model: None,
                            status: "ok".into(),
                            usage: None,
                            cost_usd: None,
                            confidence: None,
                            tool_calls: None,
                            tool_call_id: Some(fc.id.clone()),
                            skill_calls: None,
                            workflow: None,
                            images: Vec::new(),
                        };
                        {
                            let _guard = save_lock.lock().await;
                            if let Ok(mut sf) = store.load(&session_id) {
                                sf.messages.push(tool_record);
                                sf.meta.updated_at = now_ms();
                                warn_save(&session_id, "tool record", store.save(&sf));
                            }
                        }
                        sent_this_turn.push(ChatMessage {
                            role: "tool".into(),
                            content: result,
                            tool_calls: None,
                            tool_call_id: Some(fc.id.clone()),
                            images: Vec::new(),
                        });
                    }
                    continue; // synthesis round: the model summarizes branches
                }

                // ---- execute tools, feed results back ----
                sent_this_turn.push(asst_msg);
                let mut tool_msgs: Vec<ChatMessage> = Vec::new();

                // ---- pre-scan: this round's delegate_subagent calls run
                // concurrently. Futures are lazy: they start when joined at
                // the first delegation's wire position, and results fill back
                // in that same order. Budget is consumed here so an overflow
                // call still reports the cap error at its inline position.
                let mut sub_calls: Vec<(String, String)> = Vec::new(); // (call_id, task)
                let mut sub_profiles: HashMap<String, Option<crate::config::SubagentProfile>> =
                    HashMap::new();
                if tools_on && !plan_mode {
                    for tc in &outcome.tool_calls {
                        if tc.name != "delegate_subagent" || delegations >= MAX_DELEGATIONS_PER_TURN {
                            continue;
                        }
                        let task = serde_json::from_str::<Value>(&tc.arguments)
                            .ok()
                            .and_then(|a| a.get("task").and_then(|t| t.as_str()).map(|s| s.trim().to_string()))
                            .unwrap_or_default();
                        if task.is_empty() {
                            continue; // reported inline below
                        }
                        delegations += 1;
                        sub_profiles.insert(
                            tc.id.clone(),
                            find_subagent_profile(&cfg, &data_dir, &tc.arguments),
                        );
                        sub_calls.push((tc.id.clone(), task));
                    }
                }
                let mut sub_slots: HashMap<String, usize> = HashMap::new();
                let mut sub_futs: Option<Vec<_>> = None;
                if !sub_calls.is_empty() {
                    let mut futs: Vec<_> = Vec::new();
                    for (cid, task) in &sub_calls {
                        sub_slots.insert(cid.clone(), futs.len());
                        futs.push(run_subagent(
                            &state,
                            &client,
                            &data_dir,
                            &session_id,
                            task,
                            sub_profiles.get(cid).and_then(|p| p.as_ref()),
                            &binding,
                            &cfg,
                            &channel,
                            lane,
                            cid,
                        ));
                    }
                    sub_futs = Some(futs);
                }
                let mut sub_results: Option<Vec<Result<String, String>>> = None;

                for tc in &outcome.tool_calls {
                    turn_tools.push(tc.name.clone());
                    // images produced by THIS tool call (take_screenshot):
                    // filenames for the persisted record + the deterministic
                    // synthetic user message that carries the payload
                    let mut rec_images: Vec<String> = Vec::new();
                    let mut img_user_msgs: Vec<ChatMessage> = Vec::new();
                    let args_preview: String = tc.arguments.chars().take(160).collect();
                    let _ = channel.send(StreamEvent::ToolCall {
                        lane,
                        call_id: tc.id.clone(),
                        name: tc.name.clone(),
                        args: args_preview,
                    });
                    let result = if !tools_on {
                        "ERROR: 工具未启用（会话未绑定工作区，或在设置中关闭了 Agent 工具）".to_string()
                    } else if plan_mode {
                        "DENIED: 规划模式下不执行任何工具写入或外部调用——请先输出 ```plan 方案等待用户批准".to_string()
                    } else if let Some((mcp_sid, mcp_tool)) = crate::mcp::split_tool_name(&tc.name) {
                        // ---- MCP tool route: trust flag OR per-call approval ----
                        let cfg2 = config::load(&data_dir);
                        let decision: Result<bool, String> =
                            match cfg2.mcp_servers.iter().find(|s| s.id == mcp_sid) {
                                None => Err("MCP 服务不存在或已移除".into()),
                                Some(server) => {
                                    if server.trusted {
                                        Ok(true)
                                    } else {
                                        match serde_json::from_str::<Value>(&tc.arguments) {
                                            Err(e) => Err(format!("参数不是合法 JSON: {e}")),
                                            Ok(args) => {
                                                let approval_id = Uuid::new_v4().to_string();
                                                let rx = open_approval(&state, &approval_id);
                                                let _ = channel.send(StreamEvent::ApprovalRequest {
                                                    lane,
                                                    approval_id,
                                                    tool: tc.name.clone(),
                                                    path: server.name.clone(),
                                                    preview: format!(
                                                        "调用 MCP 服务「{}」的工具 {mcp_tool}\n参数：{}",
                                                        server.name,
                                                        serde_json::to_string_pretty(&args)
                                                            .unwrap_or_default()
                                                    ),
                                                });
                                                Ok(matches!(
                                                    tokio::time::timeout(
                                                        std::time::Duration::from_secs(APPROVAL_TIMEOUT_SECS),
                                                        rx
                                                    )
                                                    .await,
                                                    Ok(Ok(true))
                                                ))
                                            }
                                        }
                                    }
                                },
                            };
                        match decision {
                            Err(e) => format!("ERROR: {e}"),
                            Ok(false) => "DENIED: 用户拒绝或审批超时——MCP 工具未执行".to_string(),
                            Ok(true) => {
                                let cfg3 = config::load(&data_dir);
                                match cfg3.mcp_servers.iter().find(|s| s.id == mcp_sid) {
                                    None => "ERROR: MCP 服务不存在或已移除".to_string(),
                                    Some(server) => {
                                        match serde_json::from_str::<Value>(&tc.arguments) {
                                            Ok(args) => crate::mcp::global()
                                                .call_tool(server, &mcp_tool, &args)
                                                .await,
                                            Err(e) => format!("ERROR: 参数不是合法 JSON: {e}"),
                                        }
                                    }
                                }
                            }
                        }
                    } else {
                        match serde_json::from_str::<Value>(&tc.arguments) {
                            Err(e) => format!("ERROR: 参数不是合法 JSON: {e}"),
                            Ok(args) => {
                                if tc.name == "delegate_subagent" {
                                    if let Some(&slot) = sub_slots.get(&tc.id) {
                                        // pre-spawned concurrent delegation:
                                        // join once, then read this call's slot
                                        if sub_results.is_none() {
                                            let futs = sub_futs.take().expect("delegation futures");
                                            sub_results = Some(futures_util::future::join_all(futs).await);
                                        }
                                        match &sub_results.as_ref().expect("joined results")[slot] {
                                            Ok(summary) => format!("子任务完成，回报如下：\n\n{summary}"),
                                            Err(e) => format!("ERROR: 子任务失败: {e}"),
                                        }
                                    } else if delegations >= MAX_DELEGATIONS_PER_TURN {
                                        format!("ERROR: 本轮委派已达上限（{MAX_DELEGATIONS_PER_TURN} 个子任务）")
                                    } else {
                                        // pre-scan skipped it ⇒ empty task
                                        "ERROR: task 不能为空".to_string()
                                    }
                                } else if tc.name == "memory_save" || tc.name == "memory_search" {
                                    // vector long-term memory (async: embeddings
                                    // API) — workspace-scoped store in data_dir
                                    let ws = workspace.clone().unwrap_or_default();
                                    if !cfg.settings.vector_memory {
                                        "ERROR: 向量长期记忆未启用（设置 → 向量长期记忆）".to_string()
                                    } else if ws.is_empty() {
                                        "ERROR: 会话未绑定工作区，无法定位记忆存储".to_string()
                                    } else if tc.name == "memory_save" {
                                        let text = args
                                            .get("text")
                                            .and_then(|t| t.as_str())
                                            .unwrap_or("")
                                            .trim()
                                            .to_string();
                                        if text.is_empty() {
                                            "ERROR: text 不能为空".to_string()
                                        } else {
                                            match crate::memvector::remember(&client, &cfg, &data_dir, &ws, &text).await {
                                                Ok(()) => "OK: 已写入长期记忆".to_string(),
                                                Err(e) => format!("ERROR: {e}"),
                                            }
                                        }
                                    } else {
                                        let query = args
                                            .get("query")
                                            .and_then(|t| t.as_str())
                                            .unwrap_or("")
                                            .trim()
                                            .to_string();
                                        let k = args
                                            .get("k")
                                            .and_then(|v| v.as_u64())
                                            .map(|v| (v as usize).clamp(1, 20))
                                            .unwrap_or(5);
                                        if query.is_empty() {
                                            "ERROR: query 不能为空".to_string()
                                        } else {
                                            match crate::memvector::embed(
                                                &client,
                                                &cfg.settings.embeddings_url.as_str(),
                                                &cfg.settings.embeddings_key.as_str(),
                                                &cfg.settings.embeddings_model.as_str(),
                                                &query,
                                            )
                                            .await
                                            {
                                                Err(e) => format!("ERROR: {e}"),
                                                Ok(qv) => {
                                                    let hits = crate::memvector::search(&data_dir, &ws, &qv, k);
                                                    if hits.is_empty() {
                                                        "OK: 记忆库中没有匹配的条目".to_string()
                                                    } else {
                                                        let body = hits
                                                            .iter()
                                                            .map(|(s, t)| format!("- [{s:.3}] {t}"))
                                                            .collect::<Vec<_>>()
                                                            .join("\n");
                                                        format!("OK:\n{body}")
                                                    }
                                                }
                                            }
                                        }
                                    }
                                } else if tc.name == "todo_write" {
                                    // session-scoped task list for the preview
                                    // panel — no workspace access, no approval
                                    match serde_json::from_value::<Vec<TodoItem>>(
                                        args.get("todos").cloned().unwrap_or(Value::Null),
                                    ) {
                                        Err(e) => format!("ERROR: todos 参数不合法: {e}"),
                                        Ok(raw) => {
                                            if raw.is_empty() {
                                                save_todos(&data_dir, &session_id, &[]);
                                                "OK: 任务清单已清空".to_string()
                                            } else if raw.len() > 20 {
                                                "ERROR: 任务数超过 20 项上限".to_string()
                                            } else {
                                                let todos: Vec<TodoItem> = raw
                                                    .into_iter()
                                                    .take(20)
                                                    .map(|mut t| {
                                                        t.text = t.text.trim().chars().take(200).collect();
                                                        if t.text.is_empty() {
                                                            t.text = "（未命名任务）".into();
                                                        }
                                                        match t.status.as_str() {
                                                            "in_progress" | "done" => {}
                                                            _ => t.status = "pending".into(),
                                                        }
                                                        t
                                                    })
                                                    .collect();
                                                let done = todos.iter().filter(|t| t.status == "done").count();
                                                save_todos(&data_dir, &session_id, &todos);
                                                format!("OK: 任务清单已更新（共 {} 项，已完成 {done}）", todos.len())
                                            }
                                        }
                                    }
                                } else if tc.name == "take_screenshot" {
                                    // Computer Use "see" primitive: capture
                                    // the screen, save it into the session's
                                    // attachment dir and attach the image to
                                    // the tool result. Privacy: same
                                    // fail-closed approval gate as write
                                    // tools (auto/grant bypasses the card).
                                    let granted = perm_mode == "auto"
                                        || state
                                            .grants
                                            .lock()
                                            .unwrap()
                                            .as_ref()
                                            .is_some_and(|s| {
                                                s.contains(&grant_key(&session_id, "take_screenshot"))
                                            });
                                    let approved = if granted {
                                        true
                                    } else {
                                        let approval_id = Uuid::new_v4().to_string();
                                        let rx = open_approval(&state, &approval_id);
                                        let _ = channel.send(StreamEvent::ApprovalRequest {
                                            lane,
                                            approval_id: approval_id.clone(),
                                            tool: "take_screenshot".into(),
                                            path: "（整个屏幕）".into(),
                                            preview: crate::agent_tools::approval_preview(
                                                "",
                                                "take_screenshot",
                                                &args,
                                            ),
                                        });
                                        match tokio::time::timeout(
                                            std::time::Duration::from_secs(APPROVAL_TIMEOUT_SECS),
                                            rx,
                                        )
                                        .await
                                        {
                                            Ok(Ok(true)) => true,
                                            _ => false,
                                        }
                                    };
                                    if approved {
                                        let dd = data_dir.to_path_buf();
                                        let sid = session_id.to_string();
                                        match tokio::task::spawn_blocking(move || {
                                            crate::agent_tools::take_screenshot(&dd, &sid)
                                        })
                                        .await
                                        {
                                            Ok(Ok((text, shots))) => {
                                                for s in &shots {
                                                    rec_images.push(s.filename.clone());
                                                }
                                                img_user_msgs.push(ChatMessage::with_images(
                                                    "user",
                                                    chat::TOOL_IMAGE_NOTE,
                                                    shots
                                                        .into_iter()
                                                        .map(|s| crate::prefix::ChatImage {
                                                            mime: s.mime,
                                                            b64: s.b64,
                                                            file_ref: None,
                                                        })
                                                        .collect(),
                                                ));
                                                text
                                            }
                                            Ok(Err(e)) => format!("ERROR: {e}"),
                                            Err(e) => format!("ERROR: 工具任务失败: {e}"),
                                        }
                                    } else {
                                        "DENIED: 用户拒绝或审批超时（120 秒）——未截取屏幕".to_string()
                                    }
                                } else if goal_mode
                                    && matches!(
                                        tc.name.as_str(),
                                        "get_goal" | "create_goal" | "update_goal"
                                    )
                                {
                                    handle_goal_tool(&state, &data_dir, &session_id, &tc.name, &args).await
                                } else if !crate::agent_tools::is_write_tool(&tc.name) {
                                    // sandbox policy: read tools may be refused
                                    // (delete-class rules don't apply here, but
                                    // deny-listed paths / network lists do)
                                    if let crate::agent_tools::SandboxVerdict::Block(reason) =
                                        crate::agent_tools::sandbox_check(&tc.name, &args)
                                    {
                                        reason
                                    } else {
                                        let ws = workspace.as_deref().unwrap_or("").to_string();
                                        let name = tc.name.clone();
                                        let args2 = args.clone();
                                        tokio::task::spawn_blocking(move || {
                                            crate::agent_tools::execute(&ws, &name, &args2)
                                        })
                                        .await
                                        .unwrap_or_else(|e| format!("ERROR: 工具任务失败: {e}"))
                                    }
                                } else if perm_mode == "readonly" || plan_mode {
                                    "DENIED: 当前为只读或规划模式，写入工具不可用".to_string()
                                } else {
                                    // sandbox guard: refuse before the approval
                                    // card is ever raised (delete / deny-listed
                                    // paths / denied programs / blocked network)
                                    let verdict = crate::agent_tools::sandbox_check(&tc.name, &args);
                                    if let crate::agent_tools::SandboxVerdict::Block(reason) = &verdict {
                                        reason.clone()
                                    } else {
                                    // privacy: tool arguments may carry
                                    // surrogates the model echoed back — the
                                    // tool must operate on REAL values
                                    let mut exec_args = args.clone();
                                    if privacy_on {
                                        restore_args(&session_id, &pseed, &mut exec_args);
                                    }
                                    // ---- approval gate (fail-closed) ----
                                    // sandbox ForceAsk（需逐次确认名单）强制弹出
                                    // 审批卡，即使自动模式 / 已记住授权；
                                    // 文件白名单的可信路径 keep the auto tier.
                                    let trusted = sb.on
                                        && perm_base == "auto"
                                        && verdict == crate::agent_tools::SandboxVerdict::Allow
                                        && crate::agent_tools::file_paths_trusted(&tc.name, &args);
                                    let granted = trusted
                                        || (verdict != crate::agent_tools::SandboxVerdict::ForceAsk
                                            && (perm_mode == "auto"
                                                || state
                                                    .grants
                                                    .lock()
                                                    .unwrap()
                                                    .as_ref()
                                                    .is_some_and(|s| {
                                                        s.contains(&grant_key(&session_id, &tc.name))
                                                    })));
                                    let approved = if granted {
                                        true
                                    } else {
                                        let approval_id = Uuid::new_v4().to_string();
                                        let path = exec_args
                                            .get("path")
                                            .and_then(|p| p.as_str())
                                            .or_else(|| exec_args.get("command").and_then(|c| c.as_str()))
                                            .or_else(|| exec_args.get("from").and_then(|f| f.as_str()))
                                            .unwrap_or("?")
                                            .to_string();
                                        let rx = open_approval(&state, &approval_id);
                                        let _ = channel.send(StreamEvent::ApprovalRequest {
                                            lane,
                                            approval_id: approval_id.clone(),
                                            tool: tc.name.clone(),
                                            path,
                                            preview: crate::agent_tools::approval_preview(
                                                workspace.as_deref().unwrap_or(""),
                                                &tc.name,
                                                &exec_args,
                                            ),
                                        });
                                        // 120s deny — a dropped channel denies too
                                        match tokio::time::timeout(
                                            std::time::Duration::from_secs(APPROVAL_TIMEOUT_SECS),
                                            rx,
                                        )
                                        .await
                                        {
                                            Ok(Ok(true)) => true,
                                            _ => false,
                                        }
                                    };
                                    if approved {
                                        // review-panel capture: before/after
                                        // snapshots of the target file (only
                                        // for file tools — run_command has none)
                                        let rel =
                                            exec_args.get("path").and_then(|p| p.as_str()).unwrap_or("").to_string();
                                        let abs = if rel.is_empty() {
                                            None
                                        } else {
                                            crate::agent_tools::resolve_in_workspace(
                                                workspace.as_deref().unwrap_or(""),
                                                &rel,
                                            )
                                            .ok()
                                        };
                                        // sandbox auto-backup: snapshot the
                                        // existing file BEFORE it is modified
                                        if cfg.settings.sandbox_backup {
                                            if let Some(p) = abs.as_ref().filter(|p| p.is_file()) {
                                                if let Err(e) = backup_snapshot(
                                                    &data_dir,
                                                    &session_id,
                                                    p,
                                                    cfg.settings.sandbox_backup_cap_mb,
                                                ) {
                                                    eprintln!("[sandbox-backup] {e}");
                                                }
                                            }
                                        }
                                        let snap = |p: &std::path::Path| -> Option<String> {
                                            std::fs::read_to_string(p).ok().map(|s| {
                                                s.chars().take(WRITE_LOG_CAP).collect::<String>()
                                            })
                                        };
                                        let before = abs.as_ref().and_then(|p| snap(p));
                                        let ws_s = workspace.as_deref().unwrap_or("").to_string();
                                        let name_s = tc.name.clone();
                                        let exec_args_s = exec_args.clone();
                                        let mut exec = tokio::task::spawn_blocking(move || {
                                            crate::agent_tools::execute_write(&ws_s, &name_s, &exec_args_s)
                                        })
                                        .await
                                        .unwrap_or_else(|e| format!("ERROR: 工具任务失败: {e}"));
                                        if exec.starts_with("OK") && !rel.is_empty() {
                                            let after = abs.as_ref().and_then(|p| snap(p));
                                            let log = crate::types_rs::WriteLog {
                                                ts: next_record_ts(&state),
                                                tool: tc.name.clone(),
                                                path: rel,
                                                before,
                                                after,
                                            };
                                            let _guard = save_lock.lock().await;
                                            if let Ok(mut sf) = store.load(&session_id) {
                                                sf.writes.push(log);
                                                if sf.writes.len() > 200 {
                                                    sf.writes.drain(..sf.writes.len() - 200);
                                                }
                                                sf.meta.updated_at = now_ms();
                                                warn_save(&session_id, "write log", store.save(&sf));
                                            }
                                        }
                                        // post-write verification hook: run the
                                        // configured command and append its
                                        // report so the model sees verification
                                        // output in this very tool result
                                        if exec.starts_with("OK")
                                            && matches!(
                                                tc.name.as_str(),
                                                "write_file"
                                                    | "edit_file"
                                                    | "apply_patch"
                                                    | "delete_file"
                                                    | "move_path"
                                            )
                                        {
                                            if let (Some(ws), Some(cmd)) = (
                                                workspace.as_deref(),
                                                cfg.settings.post_write_command.as_deref(),
                                            ) {
                                                let ws_v = ws.to_string();
                                                let cmd_v = cmd.to_string();
                                                if let Some(report) = tokio::task::spawn_blocking(move || {
                                                    crate::agent_tools::post_write_verify(&ws_v, &cmd_v)
                                                })
                                                .await
                                                .ok()
                                                .flatten()
                                                {
                                                    exec.push_str(&report);
                                                }
                                            }
                                        }
                                        exec
                                    } else {
                                        "DENIED: 用户拒绝或审批超时（120 秒）——本次写入未执行".into()
                                    }
                                    } // sandbox_check passed
                                }
                            }
                        }
                    };
                    // guardrails: fence untrusted external content (fetched
                    // pages, MCP tool results) as data — opt-in via settings
                    let result = if cfg.settings.guardrails
                        && (tc.name == "web_fetch" || crate::mcp::split_tool_name(&tc.name).is_some())
                        && !result.starts_with("ERROR:")
                        && !result.starts_with("DENIED:")
                    {
                        let source = match crate::mcp::split_tool_name(&tc.name) {
                            Some((sid, tool)) => {
                                let sname = config::load(&data_dir)
                                    .mcp_servers
                                    .iter()
                                    .find(|s| s.id == sid)
                                    .map(|s| s.name.clone())
                                    .unwrap_or_else(|| sid.clone());
                                format!("MCP 服务「{sname}」的 {tool} 结果")
                            }
                            None => format!(
                                "网页 {}",
                                serde_json::from_str::<Value>(&tc.arguments)
                                    .ok()
                                    .and_then(|a| {
                                        a.get("url").and_then(|u| u.as_str()).map(|s| s.to_string())
                                    })
                                    .unwrap_or_default()
                            ),
                        };
                        crate::guard::wrap_untrusted(&source, &result, &cfg.settings.guardrails_extra)
                    } else {
                        result
                    };
                    // context-volume: oversized results spill to disk — the
                    // record (and from there Zone H replay) keeps the trimmed
                    // form; the full output stays readable via read_file at
                    // the locator path
                    let result = crate::spill::maybe_spill(
                        &session_id,
                        &tc.name,
                        &result,
                        cfg.settings.spill_max_chars,
                    );
                    let result_preview: String = result.lines().next().unwrap_or("").chars().take(120).collect();
                    let _ = channel.send(StreamEvent::ToolResult {
                        lane,
                        call_id: tc.id.clone(),
                        name: tc.name.clone(),
                        result: result_preview,
                    });
                    let tool_record = MessageRecord {
                        id: Uuid::new_v4().to_string(),
                        lane,
                        role: "tool".into(),
                        content: result.clone(),
                        reasoning: None,
                        ts: next_record_ts(&state),
                        model: None,
                        status: "ok".into(),
                        usage: None,
                        cost_usd: None,
                        confidence: None,
                        tool_calls: None,
                        tool_call_id: Some(tc.id.clone()),
                        skill_calls: None,
                        workflow: None,
                        images: rec_images,
                    };
                    {
                        let _guard = save_lock.lock().await;
                        if let Ok(mut sf) = store.load(&session_id) {
                            sf.messages.push(tool_record);
                            sf.meta.updated_at = now_ms();
                            warn_save(&session_id, "tool record", store.save(&sf));
                        }
                    }
                    // live wire: the record above stores the REAL tool
                    // output (user-readable); the outbound copy is scrubbed
                    // so the model keeps seeing surrogates. Restart rebuilds
                    // produce the same bytes (deterministic mapping).
                    let mut result = result;
                    if privacy_on {
                        result = crate::privacy::outbound(&session_id, &pseed, &result);
                    }
                    tool_msgs.push(ChatMessage {
                        role: "tool".into(),
                        content: result,
                        tool_calls: None,
                        tool_call_id: Some(tc.id.clone()),
                        images: Vec::new(),
                    });
                    tool_msgs.extend(img_user_msgs);
                }
                sent_this_turn.extend(tool_msgs);
                // continue to the next round: the model sees tool results
            }

            // Zone T falls into Zone H for the next user turn
            if turn_status != "error" {
                // did a RollingMemo block ship this turn? The watermark only
                // advances AFTER the Zone H append below (see the block at
                // the end) — advancing it before the request let a
                // failed/stopped turn silently lose the memory until the
                // next rebuild.
                let memo_landed = memo_injection.is_some();
                let mut warm_slot: Option<crate::warmer::WarmSlot> = None;
                {
                    let mut guard = prefixes_lock(&state);
                    if let Some(map) = guard.as_mut() {
                        if let Some(lp) = map.get_mut(&(session_id.clone(), lane)) {
                            for m in &sent_this_turn {
                                lp.append(m);
                            }
                            // the in-history system update rode the request
                            // and has now entered Zone H — only now book it,
                            // so a failed turn re-injects (see prep comment)
                            if let Some(sys_text) = &system_injection {
                                lp.mark_system_in_history(sys_text);
                            }
                            // cache warmer (opt-in): snapshot the post-append
                            // prefix — exactly the span the next real request
                            // wants cached — and schedule one keepalive probe
                            if turn_status == "ok"
                                && crate::warmer::eligible(
                                    cfg.settings.cache_warmup,
                                    provider.kind.clone(),
                                    provider.cache_tier(),
                                    lp.is_reasoning_bound(),
                                )
                            {
                                warm_slot = Some(crate::warmer::WarmSlot {
                                    prefix: lp.clone(),
                                    model: model.clone(),
                                    tools: tools_schema.clone(),
                                });
                            }
                        }
                    }
                }
                // the memo block has now genuinely entered Zone H (append
                // above) — only now advance the injection watermark, so a
                // failed/stopped turn re-injects instead of losing the
                // memory. The prefixes guard is already dropped here, so
                // taking save_lock keeps the lock order acyclic.
                if memo_landed {
                    let _guard = save_lock.lock().await;
                    if let Ok(mut s) = store.load(&session_id) {
                        s.meta.rolling_memo_injected_rev = s.meta.rolling_memo_rev;
                        s.meta.updated_at = now_ms();
                        warn_save(&session_id, "memo watermark", store.save(&s));
                    }
                }
                if let Some(slot) = warm_slot {
                    crate::warmer::schedule(client.clone(), provider.clone(), session_id.clone(), lane, slot);
                }
            } else {
                // an errored turn's tail never enters Zone H — drop the
                // span marker so the next request isn't judged against bytes
                // we deliberately discarded
                if let Some(map) = state.last_span.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
                    map.remove(&(session_id.clone(), lane));
                }
            }

            // RollingMemo (L6 §4.2): rule-based durable-fact extraction from
            // this finished turn — file targets (last action wins), user
            // corrections, error→fix pairs. Pure functions, zero model
            // calls; the memo reaches the context via the injection path on
            // a later turn (rev watermark + rebuild branch).
            if lane == 0 && turn_status == "ok" {
                let _guard = save_lock.lock().await;
                if let Ok(mut s) = store.load(&session_id) {
                    let mut memo = s.meta.rolling_memo.clone().unwrap_or_default();
                    if rolling_memo_apply(&mut memo, &sent_this_turn) {
                        s.meta.rolling_memo = Some(memo);
                        s.meta.rolling_memo_rev += 1;
                        s.meta.updated_at = now_ms();
                        warn_save(&session_id, "rolling memo", store.save(&s));
                    }
                }
            }

            // State-machine auto-advance: only lane 0 (the shared user lane —
            // arena side-lanes must not move the machine), only on a fully ok
            // turn, and only from the state this turn actually ran under.
            // Terminal states stop; a state without `next` holds position
            // (the user can jump manually from the progress bar).
            if lane == 0 && turn_status == "ok" && wf.starts_with("sm:") {
                if let Some((def_id, state_name)) = wf[3..].split_once(':') {
                    if let Some((def, st)) = chat::resolve_sm(&cfg.workflows, def_id, state_name) {
                        if !st.terminal {
                            // conditional branches first: the FIRST matching
                            // rule wins (evaluated against this turn's reply
                            // text, tools used, and status); fall back to
                            // `next` when no rule matches.
                            let hit = st
                                .branches
                                .iter()
                                .find(|b| {
                                    chat::eval_when(&b.when, &turn_text, &turn_tools, &turn_status)
                                })
                                .map(|b| b.goto.clone());
                            let target = hit.or_else(|| st.next.clone());
                            if let Some(nx) = target {
                                if def.states.iter().any(|s| s.name == nx) {
                                    let gate = format!("sm:{def_id}:{nx}");
                                    sm_put(&state, &session_id, &gate);
                                    // checkpoint so a restart resumes the
                                    // machine at the auto-advanced state
                                    persist_gate(&state, &session_id, Some(&gate)).await;
                                }
                            }
                        }
                    }
                }
            }

            // Auto-reflection gate decided BEFORE Done moves turn_status.
            let do_reflect = lane == 0
                && turn_status == "ok"
                && cfg.settings.vector_memory
                && cfg.settings.auto_reflect
                && !cfg.settings.embeddings_url.trim().is_empty();

            // Goal iteration timeline (ZCode parity): one snapshot per
            // completed turn while the goal gate is active. Title = first
            // pending criterion, so the round list reads as the task's
            // progression story. Capped at the last 60 entries.
            if goal_mode && lane == 0 && turn_status == "ok" && sf_snapshot.meta.goal.is_some() {
                let (ok, total, done, claimed) = parse_goal_summary_ext(&turn_text);
                let pending: String = turn_text
                    .lines()
                    .find(|l| strip_list_prefix(l).starts_with('⬜'))
                    .map(|l| {
                        strip_list_prefix(l)
                            .trim_start_matches('⬜')
                            .trim()
                            .chars()
                            .take(48)
                            .collect::<String>()
                    })
                    .unwrap_or_default();
                let title = if !pending.is_empty() {
                    pending
                } else if done {
                    "已申报 GOAL_DONE，等待校验收尾".to_string()
                } else {
                    "本轮清单全部达成".to_string()
                };
                let _guard = save_lock.lock().await;
                if let Ok(mut sfr) = store.load(&session_id) {
                    let round = sfr.meta.goal_rounds.len() as u32 + 1;
                    sfr.meta.goal_rounds.push(crate::types_rs::GoalRound {
                        round,
                        title,
                        done: ok,
                        total,
                        claimed,
                        ts: now_ms(),
                    });
                    if sfr.meta.goal_rounds.len() > 60 {
                        let overflow = sfr.meta.goal_rounds.len() - 60;
                        sfr.meta.goal_rounds.drain(..overflow);
                    }
                    sfr.meta.updated_at = now_ms();
                    warn_save(&session_id, "goal rounds", store.save(&sfr));
                }
            }

            let _ = channel.send(StreamEvent::Done {
                lane,
                message_id: last_message_id,
                status: turn_status,
                confidence: last_confidence,
            });

            // Auto-reflection (#settings.auto_reflect): on a successful main
            // lane turn, distill durable facts in the background and write
            // them into vector memory. Fully async + silent: never blocks or
            // errors the finished turn.
            if do_reflect {
                let ws = sf_snapshot.meta.workspace.clone().unwrap_or_default();
                let user_text = sf_snapshot
                    .messages
                    .iter()
                    .rev()
                    .find(|m| m.role == "user")
                    .map(|m| m.content.clone())
                    .unwrap_or_default();
                if !ws.is_empty() && !user_text.is_empty() && !turn_text.trim().is_empty() {
                    let client = client.clone();
                    let cfg2 = cfg.clone();
                    let binding2 = binding.clone();
                    let data_dir2 = data_dir.clone();
                    let reply_text = turn_text.clone();
                    tauri::async_runtime::spawn(async move {
                        if let Some(provider) = resolve_provider(&cfg2, &binding2) {
                            crate::memvector::reflect_and_remember(
                                &client,
                                &cfg2,
                                provider,
                                &binding2.model,
                                &user_text,
                                &reply_text,
                                &data_dir2,
                                &ws,
                            )
                            .await;
                        }
                    });
                }
            }
        });
        handles.push(handle);
    }

    for h in handles {
        let _ = h.await;
    }
    // concurrent-send guard: a second send on the same session replaces
    // this entry in the map — only remove the flag if it is still OURS,
    // otherwise the first finisher deletes the second send's stop switch
    // and that send could never be stopped
    {
        let mut stops = state.stops.lock().unwrap_or_else(|p| p.into_inner());
        if stops
            .get(&session_id)
            .is_some_and(|cur| Arc::ptr_eq(cur, &stop))
        {
            stops.remove(&session_id);
        }
    }

    // AuxMemo application: auto-title (whitelist kind "title"). Fires once
    // per session when the first exchange finished; served from the exact
    // cache when the same first exchange was titled before.
    if !arena {
        let cfg = config::load(&state.data_dir);
        let client = state.client.clone();
        let data_dir = state.data_dir.clone();
        let sid = session_id.clone();
        let save_lock = state.save_lock.clone();
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            let state = app.state::<AppState>();
            let store = SessionStore::new(&data_dir);
            let Ok(sf) = store.load(&sid) else { return };
            if sf.meta.title != "新会话" || sf.messages.len() < 2 {
                return;
            }
            // image sessions: the binding is an image model — chat title-gen
            // would call chat/completions with it; derive from the prompt
            if workflow_of_in(&state, &sid, &data_dir) == "image" {
                let t: String = sf
                    .messages
                    .iter()
                    .find(|m| m.role == "user")
                    .map(|m| m.content.clone())
                    .unwrap_or_default()
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(16)
                    .collect();
                if !t.is_empty() {
                    // rename is a load-modify-save on the session file —
                    // hold save_lock or a concurrent lane save is rolled back
                    let _guard = save_lock.lock().await;
                    let _ = store.rename(&sid, &t);
                }
                return;
            }
            let Some(binding) = sf.meta.bindings.first().cloned() else { return };
            let Some(provider) = resolve_provider(&cfg, &binding) else { return };
            let provider = provider.clone();
            let ns = crate::auxmemo::namespace(sf.meta.workspace.as_deref());
            let u = sf.messages.iter().find(|m| m.role == "user").map(|m| m.content.clone()).unwrap_or_default();
            let a = sf.messages.iter().find(|m| m.role == "assistant").map(|m| m.content.clone()).unwrap_or_default();
            let clip = |s: String, n: usize| s.chars().take(n).collect::<String>();
            let input = format!("用户: {}\n\n助手: {}", clip(u, 600), clip(a, 600));
            let key = crate::auxmemo::compute_key("title", &binding.model, &provider.base_url, &input, &ns);
            let title = match crate::auxmemo::get(&data_dir, &key, &ns) {
                Some((e, origin)) => {
                    crate::auxmemo::record_hit(&data_dir, "title", &e, origin);
                    e.text
                }
                None => {
                    let Ok(outcome) = chat::complete_once(
                        &client,
                        &provider,
                        &binding.model,
                        "你是标题生成器。根据对话节选生成一个不超过 12 个字的中文标题，概括主题。只输出标题本身，不要引号、句号或任何前后缀。",
                        &input,
                    )
                    .await
                    else {
                        return; // silent: titles are best-effort
                    };
                    let cost = chat::cost_of(&outcome.usage, &provider, &binding.model);
                    let t: String = outcome
                        .text
                        .trim()
                        .chars()
                        .filter(|c| !c.is_whitespace() && *c != '"' && *c != '“' && *c != '”')
                        .take(24)
                        .collect();
                    if t.is_empty() {
                        return;
                    }
                    crate::auxmemo::put(
                        &data_dir,
                        &key,
                        &ns,
                        &crate::auxmemo::EntryMeta {
                            text: t.clone(),
                            model: binding.model.clone(),
                            in_tok: outcome.usage.input,
                            out_tok: outcome.usage.output,
                            cost_usd: cost,
                        },
                    );
                    crate::auxmemo::record_miss(
                        &data_dir,
                        "title",
                        &binding.model,
                        outcome.usage.input,
                        outcome.usage.output,
                        cost,
                    );
                    t
                }
            };
            let _guard = save_lock.lock().await;
            let _ = store.rename(&sid, &title);
        });
    }
    Ok(())
}


