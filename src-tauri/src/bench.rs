// Benchmark: run a fixed set of Q&A cases against one model and score each
// reply by expected-keyword matching. Runs are persisted to
// data_dir/bench_history.json (newest first, capped) so quality can be
// compared across models and over time.

use crate::chat;
use crate::commands::AppState;
use serde::{Deserialize, Serialize};
use tauri::State;

/// One benchmark case: a question plus the keywords that mark a passing
/// reply (ANY match counts — keyword grading, not exact text).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BenchCase {
    pub id: String,
    pub question: String,
    #[serde(default)]
    pub expect_any: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BenchCaseResult {
    pub case_id: String,
    pub passed: bool,
    pub hit: Option<String>,
    pub error: Option<String>,
    pub latency_ms: u64,
    pub reply_preview: String,
    /// LLM-as-judge verdict (None when judge mode is off, the call failed,
    /// or the case itself errored). Keyword grading above stays authoritative
    /// for `passed`; judge fields are advisory extra signal.
    #[serde(default)]
    pub judge_passed: Option<bool>,
    #[serde(default)]
    pub judge_score: Option<u8>,
    #[serde(default)]
    pub judge_reason: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BenchRun {
    pub id: String,
    pub ts: i64,
    pub provider_id: String,
    pub model: String,
    pub passed: usize,
    pub total: usize,
    #[serde(default)]
    pub judge: bool,
    pub results: Vec<BenchCaseResult>,
}

const HISTORY_CAP: usize = 50;

/// Serializes load-modify-save of the history file: two concurrent bench
/// runs would otherwise race and the loser's run record vanishes.
static HISTORY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn history_path(data_dir: &std::path::Path) -> std::path::PathBuf {
    data_dir.join("bench_history.json")
}

pub fn load_history(data_dir: &std::path::Path) -> Vec<BenchRun> {
    std::fs::read_to_string(history_path(data_dir))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_history(data_dir: &std::path::Path, runs: &[BenchRun]) -> Result<(), String> {
    let mut capped: Vec<BenchRun> = runs.iter().take(HISTORY_CAP).cloned().collect();
    let p = history_path(data_dir);
    let tmp = p.with_extension("json.tmp");
    let body = serde_json::to_string(&capped.drain(..).collect::<Vec<_>>()).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, body).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &p).map_err(|e| e.to_string())
}

/// The built-in template: a compact general-purpose smoke suite covering
/// arithmetic, factual recall, Chinese understanding, instruction following,
/// structured output and light reasoning. Custom case lists can be passed
/// to bench_run instead.
pub fn default_cases() -> Vec<BenchCase> {
    vec![
        BenchCase {
            id: "arith".into(),
            question: "37 × 24 等于多少？只回答数字。".into(),
            expect_any: vec!["888".into()],
        },
        BenchCase {
            id: "logic".into(),
            question: "一个房间里有 3 支蜡烛，吹灭了 1 支，最后房间里剩下几支蜡烛？请简述理由。".into(),
            expect_any: vec!["3".into(), "三".into()],
        },
        BenchCase {
            id: "chinese".into(),
            question: "“画蛇添足”是什么意思？用一句话解释。".into(),
            expect_any: vec!["多余".into(), "多此一举".into(), "反而".into()],
        },
        BenchCase {
            id: "instruction".into(),
            question: "请只输出一个 JSON 对象：{\"ok\": true}，不要输出任何其他文字。".into(),
            expect_any: vec!["\"ok\"".into(), "true".into()],
        },
        BenchCase {
            id: "code".into(),
            question: "用 Python 写一个函数判断字符串是否是回文，给出代码即可。".into(),
            expect_any: vec!["def ".into(), "return".into(), "[::-1]".into()],
        },
        BenchCase {
            id: "summarize".into(),
            question: "把这句话改写成更简洁的英文：The result of the experiment was not what we expected at all. (≤10 words)".into(),
            expect_any: vec!["experiment".into(), "result".into(), "expect".into()],
        },
        BenchCase {
            id: "safety".into(),
            question: "我的目标是学习编程，请推荐第一步该做什么，用不超过 30 个字回答。".into(),
            expect_any: vec!["基础".into(), "语法".into(), "Python".into(), "入门".into(), "选择".into()],
        },
        BenchCase {
            id: "refusal_knowledge".into(),
            question: "光在真空中的速度大约是多少？只回答数值和单位。".into(),
            expect_any: vec!["299".into(), "3×10".into(), "3 × 10".into(), "30万".into(), "30 万".into()],
        },
    ]
}

/// Run one case against one model: ask once (non-streaming), grade by
/// keyword ANY-match, optionally re-grade with an LLM judge, record
/// latency. Never panics — an error becomes a failed result with the
/// message attached.
pub async fn run_case(
    client: &reqwest::Client,
    provider: &crate::config::Provider,
    model: &str,
    case: &BenchCase,
    judge: bool,
) -> BenchCaseResult {
    let started = std::time::Instant::now();
    let sys = "你是被评测的助手。请直接回答问题，简洁准确；除非题目要求，不要输出多余说明。";
    let reply = match chat::ask_once(client, provider, model, sys, &case.question, 800).await {
        Ok(r) => r,
        Err(e) => {
            return BenchCaseResult {
                case_id: case.id.clone(),
                passed: false,
                hit: None,
                error: Some(e),
                latency_ms: started.elapsed().as_millis() as u64,
                reply_preview: String::new(),
                judge_passed: None,
                judge_score: None,
                judge_reason: None,
            }
        }
    };
    let latency = started.elapsed().as_millis() as u64;
    let lower = reply.to_lowercase();
    let hit = case
        .expect_any
        .iter()
        .find(|k| !k.trim().is_empty() && lower.contains(&k.trim().to_lowercase()))
        .cloned();
    let preview: String = reply.trim().lines().next().unwrap_or("").chars().take(120).collect();
    let mut result = BenchCaseResult {
        case_id: case.id.clone(),
        passed: hit.is_some(),
        hit,
        error: None,
        latency_ms: latency,
        reply_preview: preview,
        judge_passed: None,
        judge_score: None,
        judge_reason: None,
    };
    if judge {
        grade_with_judge(client, provider, model, case, &reply, &mut result).await;
    }
    result
}

/// LLM-as-judge: ask a model to score the reply against the question and
/// its expected points. Parses JSON {pass, score, reason}; any failure
/// leaves the judge fields as None (keyword verdict stands). Best-effort —
/// bench must never fail because judging failed.
async fn grade_with_judge(
    client: &reqwest::Client,
    provider: &crate::config::Provider,
    model: &str,
    case: &BenchCase,
    reply: &str,
    result: &mut BenchCaseResult,
) {
    let points = if case.expect_any.is_empty() {
        "（题目未给参考要点，请按常识判断回答是否正确）".to_string()
    } else {
        format!("参考要点（命中任意一条即算关键词判分通过）：{}", case.expect_any.join("、"))
    };
    let clip = |s: &str, n: usize| s.chars().take(n).collect::<String>();
    let prompt = format!(
        "题目：{}\n{points}\n\n被评测模型的回答：\n{}\n\n请评审该回答是否正确、完整。只输出 JSON：{{\"pass\":true|false,\"score\":0-10,\"reason\":\"一句话理由\"}}",
        clip(&case.question, 600),
        clip(reply, 1_200)
    );
    let Ok(v) = chat::ask_once(
        client,
        provider,
        model,
        "你是严格的评测评审专家。只输出 JSON，不要输出其他内容。",
        &prompt,
        220,
    )
    .await
    else {
        return;
    };
    let v = v.trim();
    let json_txt = match (v.find('{'), v.rfind('}')) {
        (Some(s), Some(e)) if e > s => &v[s..=e],
        _ => v,
    };
    let Ok(j) = serde_json::from_str::<serde_json::Value>(json_txt) else {
        return;
    };
    result.judge_passed = j.get("pass").and_then(|x| x.as_bool());
    result.judge_score = j
        .get("score")
        .and_then(|x| x.as_u64())
        .map(|n| n.min(10) as u8);
    result.judge_reason = j
        .get("reason")
        .and_then(|x| x.as_str())
        .map(|s| clip(s.trim(), 200).to_string())
        .filter(|s| !s.is_empty());
}

#[tauri::command]
pub fn bench_cases_default() -> Vec<BenchCase> {
    default_cases()
}

#[tauri::command]
pub fn bench_history(state: State<'_, AppState>) -> Vec<BenchRun> {
    load_history(&state.data_dir)
}

/// Run the (default or custom) case list against one provider+model,
/// sequentially, persisting the result at the front of the history.
#[tauri::command]
pub async fn bench_run(
    state: State<'_, AppState>,
    provider_id: String,
    model: String,
    cases: Option<Vec<BenchCase>>,
    judge: Option<bool>,
) -> Result<BenchRun, String> {
    let cfg = crate::config::load(&state.data_dir);
    let provider = cfg
        .providers
        .iter()
        .find(|p| p.id == provider_id && p.enabled && !p.api_key.is_empty())
        .cloned()
        .ok_or("Provider 未配置、未启用或未填 API Key")?;
    if model.trim().is_empty() {
        return Err("模型名不能为空".into());
    }
    let judge = judge.unwrap_or(false);
    let case_list = match cases {
        Some(c) if !c.is_empty() => c,
        _ => default_cases(),
    };
    let mut results = Vec::new();
    for case in &case_list {
        results.push(run_case(&state.client, &provider, model.trim(), case, judge).await);
    }
    let passed = results.iter().filter(|r| r.passed).count();
    let run = BenchRun {
        id: uuid::Uuid::new_v4().to_string(),
        ts: crate::sessions::now_ms() as i64,
        provider_id,
        model: model.trim().to_string(),
        passed,
        total: results.len(),
        judge,
        results,
    };
    let _g = HISTORY_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut history = load_history(&state.data_dir);
    history.insert(0, run.clone());
    save_history(&state.data_dir, &history).ok(); // best-effort persistence
    Ok(run)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_cases_shape() {
        let cases = default_cases();
        assert!(cases.len() >= 6);
        for c in &cases {
            assert!(!c.id.is_empty());
            assert!(!c.question.trim().is_empty());
            assert!(c.expect_any.iter().any(|k| !k.trim().is_empty()));
        }
    }

    #[test]
    fn history_roundtrip_and_cap() {
        let dir = std::env::temp_dir().join(format!("ccharness-bench-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(load_history(&dir).is_empty());
        let mk = |i: usize| BenchRun {
            id: format!("run-{i}"),
            ts: i as i64,
            provider_id: "p".into(),
            model: "m".into(),
            passed: i,
            total: 10,
            judge: false,
            results: vec![],
        };
        // insert 60 runs newest-first; load must cap at HISTORY_CAP with run-0 first
        let runs: Vec<BenchRun> = (0..60).map(mk).collect();
        save_history(&dir, &runs).unwrap();
        let loaded = load_history(&dir);
        assert_eq!(loaded.len(), HISTORY_CAP);
        assert_eq!(loaded[0].id, "run-0");
        assert_eq!(loaded[HISTORY_CAP - 1].id, format!("run-{}", HISTORY_CAP - 1));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
