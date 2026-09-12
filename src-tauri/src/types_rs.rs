// Wire types shared across Rust modules — the wire-shape mirror of
// src/types.ts. Every change must land in BOTH files in the same commit;
// new SessionMeta / SessionFile fields need #[serde(default)] so older
// session files keep loading.
use serde::{Deserialize, Serialize};

/// Active git-worktree isolation for a session (worktree isolation): all
/// agent tools run inside `path` until the user merges or discards.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WtState {
    /// Absolute worktree path (under <data_dir>/worktrees/<session_id>).
    pub path: String,
    /// Branch checked out in the worktree ("cch/<sid8>-<ts>").
    pub branch: String,
    /// Main-workspace HEAD at creation — the diff/merge base, so a model-run
    /// `git commit` inside the worktree can never hide changes.
    pub base_head: String,
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionBinding {
    pub provider_id: String,
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub title: String,
    pub kind: String, // "chat" | "arena" | "sub"
    /// Parent session id, set only for kind "sub" transcripts created by
    /// delegate_subagent. Cascade key: the sub is reachable only through
    /// its parent, so deleting the parent deletes the sub with it.
    #[serde(default)]
    pub parent: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
    pub bindings: Vec<SessionBinding>,
    /// Workspace root for agent tools + AGENTS.md resolution.
    #[serde(default)]
    pub workspace: Option<String>,
    /// Pinned sessions sort first in the sidebar.
    #[serde(default)]
    pub pinned: bool,
    /// Archived sessions collapse into a dedicated sidebar section.
    #[serde(default)]
    pub archived: bool,
    /// Active git-worktree isolation, if any (see worktree.rs).
    #[serde(default)]
    pub wt: Option<WtState>,
    /// Persisted workflow gate ("plan" | "goal" | "deep" | "image" |
    /// "sm:<def_id>:<state>") — the checkpoint for resume-after-restart:
    /// the in-memory WORKFLOW map backfills from here on a miss instead of
    /// silently dropping the session back to agent mode.
    #[serde(default)]
    pub wf_gate: Option<String>,
    /// Goal lifecycle state (Codex /goal parity) — survives restarts so the
    /// goal summary bar and /goal command surface keep working.
    #[serde(default)]
    pub goal: Option<GoalState>,
    /// Per-turn goal snapshots (iteration timeline, ZCode parity). Capped at
    /// the last 60 entries; cleared with the goal.
    #[serde(default)]
    pub goal_rounds: Vec<GoalRound>,
    /// RollingMemo (L6 §4): rule-extracted durable session facts that let
    /// history folding lose less. Distinct from AuxMemo (the response-side
    /// exact cache) — this block rides INSIDE the context as an in-history
    /// system message.
    #[serde(default)]
    pub rolling_memo: Option<RollingMemo>,
    /// Bumped on every memo mutation; the injection watermark compares
    /// against it so a changed memo injects exactly once per change.
    #[serde(default)]
    pub rolling_memo_rev: u64,
    /// Last memo revision injected into the context. A rebuilt epoch
    /// re-injects regardless (the rebuilt Zone H carries no memo messages).
    #[serde(default)]
    pub rolling_memo_injected_rev: u64,
    /// Boundary-compaction ledger (L6 §6.1) — the third book, disjoint
    /// from RequestStat and the AuxMemo aux-ledger. Ring-capped at 60.
    #[serde(default)]
    pub compactions: Vec<CompactionStat>,
    /// Thrash guard (L6 §6.2): while completed user turns are below this
    /// number, the session's auto-compaction trigger line sits at 80%
    /// instead of 70%. Engaged automatically on rapid repeat compactions.
    #[serde(default)]
    pub compact_boost_until_turn: u64,
}

/// RollingMemo (L6 §4): session-internal rolling state block, rule-extracted
/// (zero model calls). Render order is fixed and capped — see
/// `render_memo` in commands.rs.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct RollingMemo {
    /// Session goal (goal mode only).
    #[serde(default)]
    pub goal: Option<String>,
    /// User corrections / binding decisions (FIFO, cap 12).
    #[serde(default)]
    pub decisions: Vec<String>,
    /// Target path → last action (tool name). BTreeMap keeps the render
    /// order deterministic across restarts.
    #[serde(default)]
    pub files: std::collections::BTreeMap<String, String>,
    /// Target that errored earlier and succeeded later in the same turn
    /// (FIFO, cap 8).
    #[serde(default)]
    pub errors_fixed: Vec<String>,
    /// Explicit open TODOs (v1 keeps the slot so the render shape is
    /// stable; not yet populated).
    #[serde(default)]
    pub open_items: Vec<String>,
}

/// One goal-mode round snapshot (ZCode-style iteration timeline): recorded
/// at the end of each completed turn while the goal gate is active. The
/// title is the first pending criterion (or a "done" marker) so scrolling
/// the list reads as the task's progression story.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalRound {
    pub round: u32,
    pub title: String,
    pub done: usize,
    pub total: usize,
    pub claimed: usize,
    pub ts: u64,
}

/// Goal lifecycle record (Codex /goal parity): created by the user via the
/// /goal command surface or by the model through the create_goal tool.
/// Status transitions: active ⇄ paused (user only), active → achieved/unmet
/// (model via update_goal after the completion audit, or mechanical
/// GOAL_DONE detection), active → budget_limited (runtime soft stop).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GoalState {
    /// The objective text: goal + scope + constraints + done-when + stop-if.
    pub objective: String,
    /// "active" | "paused" | "achieved" | "unmet" | "budget_limited"
    pub status: String,
    pub created_at: u64,
    pub updated_at: u64,
}

/// /goal command + get_goal tool response: the goal record plus live
/// transcript stats (accumulated session cost, checklist progress parsed
/// from the last assistant reply's ```goal block).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalInfo {
    /// Goal lifecycle record, if the session has one.
    pub goal: Option<GoalState>,
    /// Accumulated session cost (None when no provider reported cost).
    pub cost_usd: Option<f64>,
    /// ✅ count in the last ```goal checklist.
    pub checklist_done: usize,
    /// Total ✅+⬜ criteria in the last ```goal checklist.
    pub checklist_total: usize,
    /// Whether the last checklist declared GOAL_DONE.
    pub checklist_all_met: bool,
    /// ✅ rows whose line carries NO inline evidence (no backtick span and no
    /// （…） bracket note) — better-harness "claimed vs exercised" grading:
    /// these completions are asserted without verifiable proof.
    pub checklist_claimed: usize,
    /// Per-turn iteration timeline (ZCode parity), oldest first.
    #[serde(default)]
    pub goal_rounds: Vec<GoalRound>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UsageStat {
    #[serde(default)]
    pub input: Option<u64>,
    #[serde(default)]
    pub output: Option<u64>,
    #[serde(default)]
    pub cached: Option<u64>,
    /// Tokens written to the provider cache (Anthropic
    /// `cache_creation_input_tokens`). Anthropic reports this bucket disjoint
    /// from `input`; OpenAI-style providers leave it unset (writes are
    /// unreported and billed at the plain input rate).
    #[serde(default)]
    pub cache_write: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallWire {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// Boundary-compaction event stats (L6 §6.1): what fired, what it cost,
/// what it saved — the numbers the thrash detector and the payback
/// realization check consume.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionStat {
    pub ts: u64,
    /// "auto" | "manual".
    pub trigger: String,
    /// Tokens folded by the summary (FOLD bucket, post-elision).
    pub folded_tokens: u64,
    /// Bytes saved by stale-output elision (DROP bucket), ÷4 = tokens.
    pub dropped_tokens: u64,
    /// Stubs written by the elision rung.
    pub stubs: u32,
    /// Summary size (approx tokens, fixed estimate).
    pub summary_tokens: u64,
    /// RollingMemo render size at compaction time.
    pub memo_chars: usize,
    /// Payback estimate at fire time (None = no pricing data).
    pub payback_turns: Option<u64>,
    /// Completed user turns when it fired (thrash window base).
    pub completed_turns: u64,
    /// Lane-prefix epoch before the rebuild (payback realization checks).
    pub epoch_before: u32,
}

/// Boundary compaction record: the visible transcript is NEVER rewritten;
/// this only affects what goes over the wire. Messages with ts < upto_ts are
/// replaced by `summary` when assembling model context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionRecord {
    pub summary: String,
    pub upto_ts: u64,
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageRecord {
    pub id: String,
    pub lane: u32,
    pub role: String, // "user" | "assistant" | "tool"
    pub content: String,
    #[serde(default)]
    pub reasoning: Option<String>,
    pub ts: u64,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default = "default_status")]
    pub status: String, // "ok" | "stopped" | "error"
    #[serde(default)]
    pub usage: Option<UsageStat>,
    #[serde(default)]
    pub cost_usd: Option<f64>,
    /// Provider-injected `\confidence{NN}` marker, parsed out of the content.
    #[serde(default)]
    pub confidence: Option<u32>,
    /// Assistant batches that requested tool execution.
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCallWire>>,
    /// Links a role="tool" record back to its call.
    #[serde(default)]
    pub tool_call_id: Option<String>,
    /// Skill invocations attached to a user message (bodies resolved at
    /// model-context assembly time; the visible record stores names only).
    #[serde(default)]
    pub skill_calls: Option<Vec<String>>,
    /// Workflow gate active when this user message was sent ("plan").
    /// The plan directive is injected at model-context assembly time (same
    /// mechanism as skill bodies) so live Zone-H appends and restart rebuilds
    /// produce identical bytes.
    #[serde(default)]
    pub workflow: Option<String>,
    /// Frozen directive PREFIX for `sm:<def>:<state>` records: the rendered
    /// state text captured at SEND time. Rebuilds prefer it, so editing or
    /// deleting the workflow definition later cannot rewrite Zone H bytes
    /// (which would full-miss the upstream cache and fork live vs restart
    /// context). None for legacy records and every non-sm workflow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_text: Option<String>,
    /// Attachment filenames (relative, under
    /// <data_dir>/sessions/attachments/<session_id>/) carried by user
    /// messages (pasted/uploaded images) and tool records (e.g.
    /// take_screenshot). Files are immutable once written so model-context
    /// rebuilds stay byte-stable.
    #[serde(default)]
    pub images: Vec<String>,
}

/// One successful workspace write, captured for the review panel.
/// before/after are truncated file snapshots (see WRITE_LOG_CAP).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteLog {
    pub ts: u64,
    /// "write_file" | "edit_file"
    pub tool: String,
    /// Workspace-relative path as requested by the model.
    pub path: String,
    #[serde(default)]
    pub before: Option<String>,
    #[serde(default)]
    pub after: Option<String>,
}

fn default_status() -> String {
    "ok".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestStat {
    pub seq: u64,
    pub ts: u64,
    pub lane: u32,
    pub model: String,
    pub epoch: u32,
    pub prefix_bytes: usize,
    pub added_bytes: usize,
    /// Local digest-chain continuity: this request's stable span covers at
    /// least the previous request's prefix + additions. False ⇒ client-side
    /// rewrite (a regression); True with a provider miss ⇒ upstream behavior.
    #[serde(default = "default_true_fn")]
    pub chain_ok: bool,
    #[serde(default)]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub cached_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: Option<u64>,
    #[serde(default)]
    pub cost_usd: Option<f64>,
    /// Significant-miss analysis (pi-runtime parity): the stable prefix was
    /// re-billed at full price beyond the legitimate new tail.
    #[serde(default)]
    pub significant_miss: bool,
    #[serde(default)]
    pub rebilled_tokens: u64,
    #[serde(default)]
    pub rebilled_cost: Option<f64>,
    /// "upstream" | "client" | "expected" (see chat::analyze_cache_miss).
    #[serde(default)]
    pub miss_cause: Option<String>,
}

fn default_true_fn() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetrySummary {
    pub requests: u64,
    pub avg_hit_rate: Option<f64>,
    pub steady_hit_rate: Option<f64>,
    pub total_input: u64,
    pub total_cached: u64,
    pub total_output: u64,
    pub total_cost: f64,
    pub current_epoch: u32,
    pub prefix_bytes: usize,
    /// Cumulative tokens re-billed by significant misses (unexpected ones).
    #[serde(default)]
    pub rebilled_tokens: u64,
    /// Their cost at the uncached-minus-cached price spread (None = model
    /// has no pricing configured; UI then shows tokens only).
    #[serde(default)]
    pub rebilled_cost: Option<f64>,
    #[serde(default)]
    pub significant_misses: u64,
}

/// Pre-compaction break-even estimate (pi pruning economics): rewriting the
/// transcript costs the compacted summary at full input price, while every
/// subsequent turn saves the folded tokens' cache-read price.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactEstimate {
    /// Approximate tokens folded into the summary.
    pub folded_tokens: u64,
    /// Approximate tokens of the summary that replaces them.
    pub summary_tokens: u64,
    /// One-time rewrite cost of the new prefix (None = no pricing config).
    pub rewrite_cost_usd: Option<f64>,
    /// Per-turn saving once the folded tokens stop being re-read.
    pub save_per_turn_usd: Option<f64>,
    /// Turns needed to amortize the rewrite (None when no pricing config
    /// or savings are zero).
    pub payback_turns: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTelemetry {
    pub session_id: String,
    pub requests: Vec<RequestStat>,
    pub epochs: Vec<u64>,
    pub summary: TelemetrySummary,
    /// Miss-divergence localization: requests where the provider reported a
    /// miss/drop while the local chain looked intact (see divergence.rs).
    #[serde(default)]
    pub divergences: Vec<crate::divergence::Divergence>,
    /// Completed user turns right now (thrash/boost display base).
    #[serde(default)]
    pub completed_turns: u64,
    /// While completed turns are below this, the session's auto-compaction
    /// trigger line is temporarily raised to 80% (L6 §6.2). 0 = inactive.
    #[serde(default)]
    pub boost_until_turn: u64,
    /// Boundary-compaction ledger, newest last (L6 §6.1).
    #[serde(default)]
    pub compactions: Vec<crate::types_rs::CompactionStat>,
}

// ---- streaming events ----

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    Started {
        lane: u32,
        model: String,
        message_id: String,
    },
    Delta {
        lane: u32,
        message_id: String,
        text: String,
    },
    Reasoning {
        lane: u32,
        message_id: String,
        text: String,
    },
    Usage {
        lane: u32,
        message_id: String,
        usage: UsageStat,
        request: RequestStat,
    },
    Done {
        lane: u32,
        message_id: String,
        status: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        confidence: Option<u32>,
    },
    Error {
        lane: u32,
        message: String,
    },
    ToolCall {
        lane: u32,
        /// Wire id of the tool call (used by the UI to pair results/progress).
        call_id: String,
        name: String,
        args: String,
    },
    ToolResult {
        lane: u32,
        call_id: String,
        name: String,
        result: String,
    },
    /// Live output from a delegated sub-agent, forwarded while the parent
    /// round is still running (see chat::ProgressTap).
    SubProgress {
        lane: u32,
        call_id: String,
        title: String,
        text: String,
        done: bool,
    },
    /// A mutating tool wants to run — the frontend shows an approval card and
    /// must answer via the `resolve_approval` command within 120s (else deny).
    ApprovalRequest {
        lane: u32,
        approval_id: String,
        tool: String,
        path: String,
        preview: String,
    },
}

#[cfg(test)]
mod compat_tests {
    use super::*;

    /// Forward-compat gold sample: metadata written by an OLDER build (no
    /// RollingMemo / CompactionStat / boost fields) must keep loading —
    /// `#[serde(default)]` on every newly added field is a hard upgrade
    /// invariant, or a new binary would brick every existing session file.
    #[test]
    fn old_meta_json_still_loads() {
        let raw = r#"{
            "id": "abc123",
            "title": "旧会话",
            "kind": "chat",
            "created_at": 100,
            "updated_at": 200,
            "bindings": [{ "provider_id": "p", "model": "m" }]
        }"#;
        let meta: SessionMeta = serde_json::from_str(raw).expect("旧版 JSON 必须可加载");
        assert_eq!(meta.bindings[0].model, "m");
        assert!(meta.rolling_memo.is_none());
        assert_eq!(meta.rolling_memo_rev, 0);
        assert_eq!(meta.rolling_memo_injected_rev, 0);
        assert!(meta.compactions.is_empty());
        assert_eq!(meta.compact_boost_until_turn, 0);
        assert!(meta.wf_gate.is_none());
        // round-trip keeps the required fields and stays re-loadable
        let out = serde_json::to_string(&meta).unwrap();
        let back: SessionMeta = serde_json::from_str(&out).unwrap();
        assert_eq!(back.id, "abc123");
    }

    /// The full-file shape: a session file with messages survives a
    /// round-trip with byte-stable semantics on the fields that feed the
    /// request rebuild (role/lane/ts/content are the byte chain inputs).
    #[test]
    fn message_record_roundtrip_keeps_chain_fields() {
        let raw = r#"{
            "id": "m1", "lane": 0, "role": "user", "content": "你好",
            "ts": 42, "status": "ok", "images": []
        }"#;
        let m: MessageRecord = serde_json::from_str(raw).unwrap();
        assert_eq!(m.ts, 42);
        assert!(m.tool_calls.is_none());
        let out = serde_json::to_string(&m).unwrap();
        let back: MessageRecord = serde_json::from_str(&out).unwrap();
        assert_eq!(back.content, "你好");
        assert_eq!(back.lane, 0);
    }
}
