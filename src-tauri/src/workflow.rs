// Workflow & goal tracking: mode gates (agent / plan / goal / deep /
// review and sm:<def>:<state>), the goal lifecycle (set / status /
// clear plus the in-chat goal tool), state-machine positions and
// permission-mode resolution. Session state itself lives in AppState
// (commands.rs); this module was split out of commands.rs (P2-13).

use crate::commands::{AppState, warn_save};
use crate::config;
use crate::sessions::{now_ms, SessionStore};
use crate::types_rs::{GoalInfo, GoalState};
use serde_json::Value;
use std::collections::HashMap;
use tauri::State;

const VALID_MODES: &[&str] = &["readonly", "approve", "auto"];

/// Store an SM position and mirror it into the workflow gate.
pub(crate) fn sm_put(state: &AppState, session_id: &str, gate: &str) {
    state
        .workflow
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(session_id.to_string(), gate.to_string());
    state
        .sm_state
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(session_id.to_string(), gate.to_string());
}

/// Checkpoint the workflow gate onto the session meta (best-effort — a
/// failed save leaves the in-memory gate in charge; it only matters across
/// restarts). The meta write is a session-file read-modify-write: it goes
/// through the save lock like every other session mutation, or a lane save
/// landing mid-rewrite would be rolled back.
pub(crate) async fn persist_gate(state: &AppState, session_id: &str, gate: Option<&str>) {
    let _guard = state.save_lock.lock().await;
    if let Ok(mut sf) = state.store.load(session_id) {
        sf.meta.wf_gate = gate.map(|g| g.to_string());
        warn_save(session_id, "workflow gate", state.store.save(&sf));
    }
}

/// Workflow tags that ride on persisted message records so every later
/// rebuild re-injects the mode directive. chat.rs's transcript rebuild is
/// the read side and matches the same set — keep both in lockstep (see the
/// `record_workflow_covers_every_rebuild_mode` test): `plan` / `goal` /
/// `deep` / `review` are stamped by run_send's user record, `subagent` by
/// the subagent task record, `sm:<def>:<state>` by both.
pub fn record_workflow_of(wf: &str) -> Option<String> {
    match wf {
        "plan" | "goal" | "deep" | "review" => Some(wf.into()),
        // the full gate "sm:<def>:<state>" rides on the record so every
        // later rebuild injects the directive of the state the message
        // was actually sent under (byte-stable per record)
        w if w.starts_with("sm:") => Some(w.into()),
        _ => None,
    }
}

/// Resolve the active workflow gate. In-memory first; on a miss (fresh
/// process) backfill from the persisted checkpoint on SessionMeta so a
/// restart resumes the previous mode (断点续传). Falls back to "agent".
pub(crate) fn workflow_of_in(state: &AppState, session_id: &str, data_dir: &std::path::Path) -> String {
    let cached = state
        .workflow
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .and_then(|m| m.get(session_id))
        .cloned();
    let gate = match cached {
        Some(g) => Some(g),
        None => match SessionStore::new(data_dir)
            .load(session_id)
            .ok()
            .and_then(|sf| sf.meta.wf_gate.clone())
        {
            Some(g) => {
                state
                    .workflow
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .get_or_insert_with(HashMap::new)
                    .insert(session_id.to_string(), g.clone());
                Some(g)
            }
            None => None,
        },
    };
    match gate.as_deref() {
        Some("plan") => "plan".to_string(),
        Some("goal") => "goal".to_string(),
        Some("deep") => "deep".to_string(),
        Some("review") => "review".to_string(),
        Some("image") => "image".to_string(),
        Some(w) if w.starts_with("sm:") => w.to_string(),
        _ => "agent".to_string(),
    }
}

#[tauri::command]
pub async fn set_workflow_mode(
    state: State<'_, AppState>,
    session_id: String,
    mode: String,
) -> Result<(), String> {
    if matches!(mode.as_str(), "agent" | "plan" | "goal" | "deep" | "review" | "image") {
        // leaving (or never entering) a state machine — clear the SM position
        if let Some(m) = state.sm_state.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
            m.remove(&session_id);
        }
        state
            .workflow
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get_or_insert_with(HashMap::new)
            .insert(session_id.clone(), mode.clone());
        // checkpoint: "agent" clears the persisted gate, everything else
        // persists so a restart resumes the same mode
        if mode == "agent" {
            persist_gate(&state, &session_id, None).await;
        } else {
            persist_gate(&state, &session_id, Some(&mode)).await;
        }
        return Ok(());
    }
    if let Some(def_id) = mode.strip_prefix("sm:") {
        // declarative workflow: validate the def is real and enabled, then
        // initialize the session at its entry state (states[0])
        let cfg = config::load(&state.data_dir);
        let def = cfg
            .workflows
            .iter()
            .find(|d| d.id == def_id && d.enabled)
            .ok_or_else(|| format!("工作流不存在或未启用: sm:{def_id}"))?;
        let first = def
            .states
            .first()
            .ok_or_else(|| format!("工作流「{}」没有任何状态", def.name))?;
        let gate = format!("sm:{}:{}", def.id, first.name);
        sm_put(&state, &session_id, &gate);
        persist_gate(&state, &session_id, Some(&gate)).await;
        return Ok(());
    }
    Err(format!("未知工作流模式: {mode}"))
}

#[tauri::command]
pub fn get_workflow_mode(state: State<'_, AppState>, session_id: String) -> String {
    workflow_of_in(&state, &session_id, &state.data_dir)
}

/// Goal statuses the command surface may set. The model can only reach
/// achieved/unmet — and only through the update_goal tool (see
/// handle_goal_tool); pause/resume/clear are user-only.
const GOAL_STATUSES: &[&str] = &["active", "paused", "achieved", "unmet", "budget_limited"];

/// Checklist lines tolerate Markdown list syntax: models frequently emit
/// "- ✅ …" / "1. ✅ …" even though the directive shows bare markers, and a
/// line the parser misses silently deflates the progress counters. Only the
/// common, unambiguous forms are stripped — a missed line is the status quo,
/// a wrongly counted one is worse.
pub(crate) fn strip_list_prefix(line: &str) -> &str {
    let s = line.trim_start();
    if let Some(rest) = s
        .strip_prefix("- ")
        .or_else(|| s.strip_prefix("* "))
        .or_else(|| s.strip_prefix("+ "))
    {
        return rest.trim_start();
    }
    // ordered list: "12. " or "12、"
    let digits_end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    if digits_end > 0 {
        let rest = &s[digits_end..];
        if let Some(after) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix("、")) {
            return after.trim_start();
        }
    }
    s
}

/// Parse the LAST ```goal checklist block in an assistant reply — extended
/// (better-harness "claimed vs exercised" grading): counts ✅ criteria whose
/// line carries NO inline evidence — no backtick
/// span (`path` / `cmd` / test name) and no （…） bracket note. A ✅ without
/// evidence is a *claim*, not a verified completion; surfaces use this to
/// warn instead of trusting the checkmark.
/// Returns (✅ count, total criteria (✅+⬜ lines), GOAL_DONE seen, claimed).
/// Lines without a ✅/⬜ marker (progress notes, replan remarks) are not
/// counted as criteria.
pub fn parse_goal_summary_ext(content: &str) -> (usize, usize, bool, usize) {
    let mut block: Option<&str> = None;
    let mut rest = content;
    while let Some(pos) = rest.find("```goal") {
        let after = &rest[pos + 7..];
        match after.find("```") {
            Some(end) => {
                block = Some(&after[..end]);
                rest = &after[end + 3..];
            }
            None => {
                block = Some(after);
                break;
            }
        }
    }
    let Some(body) = block else {
        return (0, 0, false, 0);
    };
    let done = body.contains("GOAL_DONE");
    let mut ok = 0usize;
    let mut total = 0usize;
    let mut claimed = 0usize;
    for line in body.lines() {
        let t = strip_list_prefix(line);
        if t.starts_with("✅") {
            ok += 1;
            total += 1;
            let has_backtick = t.contains('`');
            let has_bracket = match t.find('（') {
                Some(i) => t[i + '（'.len_utf8()..].contains('）'),
                None => false,
            };
            if !has_backtick && !has_bracket {
                claimed += 1;
            }
        } else if t.starts_with("⬜") {
            total += 1;
        }
    }
    (ok, total, done, claimed)
}

/// Create/replace the session goal and activate the goal gate. Shared by
/// the /goal command surface and the model's create_goal tool.
/// The goal write is a session-file read-modify-write — it takes the save
/// lock like every other session mutation.
async fn set_goal_inner(
    state: &AppState,
    session_id: &str,
    objective: String,
) -> Result<GoalState, String> {
    let _guard = state.save_lock.lock().await;
    let mut sf = state.store.load(session_id)?;
    let now = now_ms();
    let g = GoalState {
        objective,
        status: "active".into(),
        created_at: now,
        updated_at: now,
    };
    sf.meta.goal = Some(g.clone());
    sf.meta.goal_rounds.clear();
    sf.meta.updated_at = now;
    state.store.save(&sf)?;
    drop(_guard);
    // flip the workflow gate to goal (same map + persist as
    // set_workflow_mode) so the next turn runs under GOAL_DIRECTIVE
    if let Some(m) = state.sm_state.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
        m.remove(session_id);
    }
    state
        .workflow
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(session_id.to_string(), "goal".to_string());
    persist_gate(state, session_id, Some("goal")).await;
    Ok(g)
}

#[tauri::command]
pub async fn goal_set(
    state: State<'_, AppState>,
    session_id: String,
    objective: String,
) -> Result<GoalState, String> {
    let objective = objective.trim().to_string();
    if objective.is_empty() {
        return Err("目标内容不能为空".into());
    }
    if objective.chars().count() > 4_000 {
        return Err("目标内容过长（上限 4000 字符）".into());
    }
    set_goal_inner(&state, &session_id, objective).await
}

/// Goal summary for the command surface and the summary bar: the persisted
/// goal record plus live transcript stats.
#[tauri::command]
pub fn goal_get(state: State<'_, AppState>, session_id: String) -> Result<GoalInfo, String> {
    let store = SessionStore::new(&state.data_dir);
    let sf = store.load(&session_id)?;
    let goal = sf.meta.goal.clone();
    let mut cost = 0f64;
    let mut has_cost = false;
    let mut last_assistant = String::new();
    for m in &sf.messages {
        if m.role == "assistant" {
            if let Some(c) = m.cost_usd {
                cost += c;
                has_cost = true;
            }
            last_assistant = m.content.clone();
        }
    }
    let (checklist_done, checklist_total, checklist_all_met, checklist_claimed) =
        parse_goal_summary_ext(&last_assistant);
    Ok(GoalInfo {
        goal,
        cost_usd: if has_cost { Some(cost) } else { None },
        checklist_done,
        checklist_total,
        checklist_all_met,
        checklist_claimed,
        goal_rounds: sf.meta.goal_rounds.clone(),
    })
}

/// User/runtime-side status transition (pause/resume/soft-stop/achieved
/// bookkeeping). Any whitelist status is allowed here — the restriction to
/// achieved/unmet applies only to the model's update_goal tool.
#[tauri::command]
pub async fn goal_status(
    state: State<'_, AppState>,
    session_id: String,
    status: String,
) -> Result<GoalState, String> {
    if !GOAL_STATUSES.contains(&status.as_str()) {
        return Err(format!("未知目标状态: {status}"));
    }
    // read-modify-write under the save lock
    let _guard = state.save_lock.lock().await;
    let store = SessionStore::new(&state.data_dir);
    let mut sf = store.load(&session_id)?;
    let mut g = sf.meta.goal.clone().ok_or("该会话还没有目标")?;
    g.status = status;
    g.updated_at = now_ms();
    sf.meta.goal = Some(g.clone());
    sf.meta.updated_at = now_ms();
    store.save(&sf)?;
    Ok(g)
}

/// Remove the goal, returning the previous record (for the toast).
#[tauri::command]
pub async fn goal_clear(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<Option<GoalState>, String> {
    let _guard = state.save_lock.lock().await;
    let store = SessionStore::new(&state.data_dir);
    let mut sf = store.load(&session_id)?;
    let prev = sf.meta.goal.take();
    sf.meta.goal_rounds.clear();
    sf.meta.updated_at = now_ms();
    store.save(&sf)?;
    Ok(prev)
}

/// Model-facing goal tools (Codex /goal parity): get_goal (read),
/// create_goal (create + flip the gate), update_goal (declare achieved /
/// unmet only). Pausing, resuming and clearing are NOT reachable from the
/// model — the same safety boundary Codex draws.
pub(crate) async fn handle_goal_tool(state: &AppState, data_dir: &std::path::Path, session_id: &str, name: &str, args: &Value) -> String {
    let store = SessionStore::new(data_dir);
    match name {
        "get_goal" => {
            let Ok(sf) = store.load(session_id) else {
                return "ERROR: 会话不存在".into();
            };
            let Some(g) = sf.meta.goal else {
                return "NONE: 当前会话尚未创建目标（可用 create_goal 创建）".into();
            };
            let last = sf
                .messages
                .iter()
                .rev()
                .find(|m| m.role == "assistant")
                .map(|m| m.content.clone())
                .unwrap_or_default();
            let (ok, total, all_met, claimed) = parse_goal_summary_ext(&last);
            let info = serde_json::json!({
                "objective": g.objective,
                "status": g.status,
                "checklist_done": ok,
                "checklist_total": total,
                "checklist_all_met": all_met,
                "checklist_claimed_no_evidence": claimed,
                "hint": if claimed > 0 { format!("{claimed} 条 ✅ 行内缺少反引号或（…）证据标注——按完成审计规则补上可核验证据，否则降回 ⬜") } else { String::new() },
            });
            format!("OK:{info}")
        }
        "create_goal" => {
            let obj = args
                .get("objective")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if obj.is_empty() {
                return "ERROR: objective 参数不能为空".into();
            }
            if obj.chars().count() > 4_000 {
                return "ERROR: objective 超过 4000 字符上限".into();
            }
            match set_goal_inner(state, session_id, obj).await {
                Ok(_) => "OK: 目标已创建并进入目标模式。请按目标模式规则推进：每轮回复末尾输出 ```goal 验收清单（✅/⬜ + 证据），全部 ✅ 时输出 GOAL_DONE。".into(),
                Err(e) => format!("ERROR: {e}"),
            }
        }
        "update_goal" => {
            let st = args.get("status").and_then(|v| v.as_str()).unwrap_or("");
            if st != "achieved" && st != "unmet" {
                return "ERROR: update_goal 仅允许 status=\"achieved\" 或 \"unmet\"（暂停/恢复/清除只能由用户操作）".into();
            }
            // the whole read-modify-write rides under the save lock
            let outcome = {
                let _guard = state.save_lock.lock().await;
                let Ok(mut sf) = state.store.load(session_id) else {
                    return "ERROR: 会话不存在".into();
                };
                let Some(g) = sf.meta.goal.as_mut() else {
                    return "NONE: 当前会话尚未创建目标".into();
                };
                if g.status == "achieved" || g.status == "unmet" {
                    return format!("ERROR: 目标已是终态（{}），不能再次变更", g.status);
                }
                g.status = st.to_string();
                g.updated_at = now_ms();
                sf.meta.updated_at = now_ms();
                state.store.save(&sf)
            };
            if outcome.is_err() {
                return "ERROR: 目标状态保存失败".into();
            }
            format!("OK: 目标状态已更新为 {st}")
        }
        _ => "ERROR: 未知目标工具".into(),
    }
}

/// Current state-machine gate for a session: "sm:<def_id>:<state>" or "".
#[tauri::command]
pub fn sm_get(state: State<'_, AppState>, session_id: String) -> String {
    state
        .sm_state
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .and_then(|m| m.get(&session_id))
        .cloned()
        .unwrap_or_default()
}

/// Manual state jump (progress-bar chips): validates the target state exists
/// in the definition, then moves the session there. The next turn runs under
/// the new state's directive and tool surface.
#[tauri::command]
pub async fn sm_set(
    state: State<'_, AppState>,
    session_id: String,
    def_id: String,
    state_name: String,
) -> Result<(), String> {
    let cfg = config::load(&state.data_dir);
    let def = cfg
        .workflows
        .iter()
        .find(|d| d.id == def_id)
        .ok_or_else(|| format!("工作流不存在: {def_id}"))?;
    // a DISABLED workflow must not be enterable — same discipline as the
    // set_workflow_mode whitelist; otherwise the progress-bar chips could
    // activate a gate the user turned off
    if !def.enabled {
        return Err(format!("工作流「{}」已停用", def.name));
    }
    if !def.states.iter().any(|s| s.name == state_name) {
        return Err(format!("工作流「{}」没有状态「{state_name}」", def.name));
    }
    let gate = format!("sm:{def_id}:{state_name}");
    sm_put(&state, &session_id, &gate);
    persist_gate(&state, &session_id, Some(&gate)).await;
    Ok(())
}

pub(crate) fn permission_of(state: &AppState, session_id: &str) -> &'static str {
    let m = state
        .permissions
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .and_then(|m| m.get(session_id))
        .cloned();
    match m.as_deref() {
        Some("readonly") => "readonly",
        Some("auto") => "auto",
        _ => "approve",
    }
}

#[tauri::command]
pub fn set_permission_mode(
    state: State<'_, AppState>,
    session_id: String,
    mode: String,
) -> Result<(), String> {
    if !VALID_MODES.contains(&mode.as_str()) {
        return Err(format!("未知权限模式: {mode}"));
    }
    state
        .permissions
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(session_id, mode);
    Ok(())
}

#[cfg(test)]
mod goal_tests {
    use super::parse_goal_summary_ext as pgs;

    #[test]
    fn parses_last_goal_block_only() {
        let content = "开头一个旧清单 ```goal\n✅ 旧一\n⬜ 旧二\n``` 中间说明，最终交付清单：\n```goal\n✅ 标准一\n⬜ 标准二\n⬜ 标准三\n```";
        assert_eq!(pgs(content), (1, 3, false, 1));
    }

    #[test]
    fn detects_goal_done_marker() {
        let content = "全部完成：\n```goal\n✅ a\n✅ b\nGOAL_DONE\n```";
        assert_eq!(pgs(content), (2, 2, true, 2));
    }

    #[test]
    fn unterminated_block_counts_to_end() {
        // 缺收尾围栏时取块尾之后的所有内容（与实现一致）。
        let content = "```goal\n✅ a\n⬜ b\n";
        assert_eq!(pgs(content), (1, 2, false, 1));
    }

    #[test]
    fn no_block_yields_zeroes() {
        assert_eq!(pgs("没有任何清单"), (0, 0, false, 0));
        assert_eq!(pgs(""), (0, 0, false, 0));
    }

    #[test]
    fn progress_notes_without_marker_not_counted() {
        // 无 ✅/⬜ 前缀的进展说明行不计入总数。
        let content = "```goal\n已完成编译检查\n测试全部通过\n```";
        assert_eq!(pgs(content), (0, 0, false, 0));
    }

    #[test]
    fn claimed_counts_ok_rows_without_inline_evidence() {
        // ✅ 行内无反引号且无（…）括注 = claimed（未验证声明）。
        let content = "```goal\n✅ 实现增量计算\n✅ 通过测试 `cargo test 12 通过`\n✅ 覆盖边界（见 src/prefix.rs）\n⬜ 剩余项\n```";
        assert_eq!(pgs(content), (3, 4, false, 1));
    }

    #[test]
    fn claimed_zero_when_all_rows_evidenced() {
        let content = "```goal\n✅ 修复断裂 `git diff --check`\n✅ 复跑通过（cargo test 全绿）\nGOAL_DONE\n```";
        assert_eq!(pgs(content), (2, 2, true, 0));
    }

    #[test]
    fn bullet_prefixed_criteria_still_counted() {
        // 模型常用 Markdown 列表语法输出清单：- / * / 1. / 12、 都要计入
        let content = "```goal\n- ✅ 一 `a.rs`\n* ⬜ 二\n1. ✅ 三（证明）\n12、 ⬜ 四\nplain 说明\n```";
        assert_eq!(pgs(content), (2, 4, false, 0));
    }

    #[test]
    fn bullet_prefixed_claimed_still_flagged() {
        // 列表项剥掉前缀后再做证据审计：无反引号无括注 → claimed
        let content = "```goal\n- ✅ 无证据行\n+ ✅ 也无证据\n```";
        assert_eq!(pgs(content), (2, 2, false, 2));
    }

    #[test]
    fn decimal_lookalike_lines_not_stripped() {
        // "3.5 倍" 不是有序列表前缀——不能把 ".5 倍…" 剥成判定行
        let content = "```goal\n3.5 倍性能达标\n```";
        assert_eq!(pgs(content), (0, 0, false, 0));
    }
}

#[cfg(test)]
mod record_workflow_tests {
    use super::record_workflow_of;

    // P1 regression: "review" once fell through the `_ => None` arm at the
    // user-record write site, leaving chat.rs's review branch unreachable
    // (dead REVIEW_DIRECTIVE) — every mode the rebuild matches must have a
    // write-side mapping. When chat.rs grows a new directive arm, extend
    // this list in the same commit.
    #[test]
    fn record_workflow_covers_every_rebuild_mode() {
        for m in ["plan", "goal", "deep", "review"] {
            assert_eq!(record_workflow_of(m).as_deref(), Some(m), "{m} 缺少写入端映射");
        }
        assert_eq!(record_workflow_of("sm:code:rev"), Some("sm:code:rev".into()));
        // agent / image / unnamed gates carry no directive — must stay unmapped
        assert_eq!(record_workflow_of("agent"), None);
        assert_eq!(record_workflow_of("image"), None);
        assert_eq!(record_workflow_of(""), None);
    }
}

