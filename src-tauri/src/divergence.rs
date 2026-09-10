// Miss-divergence localization: the provider reported a cache miss (or a
// sharp partial drop) while the local fingerprint chain says the stable span
// was append-only and intact. Pure classification over persisted
// RequestStats — deterministic, unit-tested, and provider-agnostic.

use crate::types_rs::RequestStat;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Divergence {
    pub seq: u64,
    pub ts: u64,
    /// `chain_broken` | `prefix_shrunk` | `tail_dominant` | `upstream_loss` | `partial_drop`
    pub kind: String,
    /// Human-readable diagnosis + suggested action.
    pub detail: String,
}

/// One previous request per lane (lanes interleave in the ledger).
/// Same-epoch already implies same-model (bind_model bumps the epoch).
#[derive(Clone, Copy)]
struct Prev {
    epoch: u32,
    prefix_bytes: usize,
    cached_tokens: Option<u64>,
    input_tokens: Option<u64>,
}

/// Classify divergences over the chronological request ledger.
///
/// Rules per request, compared against the previous request on the SAME lane:
/// - epoch changed            → expected rebuild, never a divergence;
/// - chain_ok = false         → `chain_broken` (client-side rewrite, a defect);
/// - cached reported as 0     → `prefix_shrunk` (prefix regressed inside an
///   epoch) | `tail_dominant` (this turn's tail is larger than the stable
///   span) | `upstream_loss` (chain intact, bytes look normal — routing /
///   TTL / gateway eviction);
/// - cached > 0 but halved    → `partial_drop` (suspected partial eviction).
pub fn classify(reqs: &[RequestStat]) -> Vec<Divergence> {
    let mut out = Vec::new();
    let mut prev_by_lane: std::collections::HashMap<u32, Prev> = std::collections::HashMap::new();

    for r in reqs {
        let prev = prev_by_lane.get(&r.lane).copied();
        prev_by_lane.insert(
            r.lane,
            Prev {
                epoch: r.epoch,
                prefix_bytes: r.prefix_bytes,
                cached_tokens: r.cached_tokens,
                input_tokens: r.input_tokens,
            },
        );
        let prev = match prev {
            Some(p) => p,
            None => continue, // first request ever on this lane
        };

        // expected: model/thinking/behavior/system change rebuilt the prefix
        if r.epoch != prev.epoch {
            continue;
        }
        // client-side rewrite — tracked as the regression it is
        if !r.chain_ok {
            out.push(Divergence {
                seq: r.seq,
                ts: r.ts,
                kind: "chain_broken".into(),
                detail: "本地字节链断裂：稳定区未完整覆盖上一请求（客户端改写，应视为缺陷上报）".into(),
            });
            continue;
        }
        // provider returned no cache field at all — nothing to localize
        let cached = match r.cached_tokens {
            Some(c) => c,
            None => continue,
        };

        if cached == 0 {
            let kind = if r.prefix_bytes < prev.prefix_bytes {
                "prefix_shrunk"
            } else if r.added_bytes > r.prefix_bytes {
                "tail_dominant"
            } else {
                "upstream_loss"
            };
            let detail = match kind {
                "prefix_shrunk" => "同纪元内前缀字节回退：存在回滚 / 分支残留 / 压缩异常 —— 检查本泳道的历史组装".into(),
                "tail_dominant" => "本轮尾区大于稳定前缀：新增内容占比过高，上游可能拒绝建缓存 —— 拆分提示词或缩短本轮新增".into(),
                _ => "本地链稳定、字节形态正常，但上游报 0 命中：路由切换 / TTL 逐出 / 网关不路由缓存 —— 重试一轮通常恢复，必要时直连官方端点对比".into(),
            };
            out.push(Divergence { seq: r.seq, ts: r.ts, kind: kind.into(), detail });
            continue;
        }

        // partial: cached halved against the previous comparable request
        if let (Some(pc), Some(pi)) = (prev.cached_tokens, prev.input_tokens) {
            if pc >= 1024 && pi > 0 && (cached as f64) < (pc as f64) * 0.5 {
                out.push(Divergence {
                    seq: r.seq,
                    ts: r.ts,
                    kind: "partial_drop".into(),
                    detail: format!(
                        "部分命中骤降（{pc} → {cached} token）：疑似上游部分逐出 —— 连续出现时考虑开新会话"
                    ),
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(seq: u64, lane: u32, epoch: u32, prefix: usize, added: usize, cached: Option<u64>) -> RequestStat {
        RequestStat {
            seq,
            ts: 1_700_000_000_000 + seq,
            lane,
            model: "m".into(),
            epoch,
            prefix_bytes: prefix,
            added_bytes: added,
            chain_ok: true,
            input_tokens: Some(1000),
            cached_tokens: cached,
            output_tokens: Some(10),
            cost_usd: None,
        }
    }

    #[test]
    fn first_of_epoch_and_no_report_are_never_divergences() {
        let reqs = vec![
            stat(1, 0, 0, 100, 500, None),  // first ever on lane
            stat(2, 0, 1, 100, 500, None),  // epoch bump ⇒ expected
            stat(3, 0, 1, 601, 500, None),  // no cache field ⇒ unjudgeable
        ];
        assert!(classify(&reqs).is_empty());
    }

    #[test]
    fn zero_cached_with_stable_chain_is_upstream_loss() {
        let reqs = vec![
            stat(1, 0, 0, 100, 500, Some(0)),
            stat(2, 0, 0, 601, 300, Some(0)), // same epoch, prefix grew, tail small
        ];
        let d = classify(&reqs);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].kind, "upstream_loss");
        assert_eq!(d[0].seq, 2);
    }

    #[test]
    fn prefix_shrink_and_tail_dominant_are_distinguished() {
        let reqs = vec![
            stat(1, 0, 0, 2000, 100, Some(500)),
            stat(2, 0, 0, 1500, 100, Some(0)), // prefix shrank inside an epoch
        ];
        assert_eq!(classify(&reqs)[0].kind, "prefix_shrunk");

        let reqs = vec![
            stat(1, 0, 0, 500, 100, Some(100)),
            stat(2, 0, 0, 601, 1200, Some(0)), // tail (1200) > stable span (601)
        ];
        assert_eq!(classify(&reqs)[0].kind, "tail_dominant");
    }

    #[test]
    fn chain_break_beats_miss_classification() {
        let mut r = stat(2, 0, 0, 1500, 100, Some(0));
        r.chain_ok = false;
        let reqs = vec![stat(1, 0, 0, 2000, 100, Some(500)), r];
        let d = classify(&reqs);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].kind, "chain_broken");
    }

    #[test]
    fn partial_drop_requires_halving_and_prior_hit() {
        let reqs = vec![
            stat(1, 0, 0, 1000, 100, Some(4000)),
            stat(2, 0, 0, 1101, 100, Some(1500)), // halved
            stat(3, 0, 0, 1202, 100, Some(1400)), // mild change, no report
        ];
        let d = classify(&reqs);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].kind, "partial_drop");
        assert_eq!(d[0].seq, 2);
    }

    #[test]
    fn lanes_are_tracked_independently() {
        let reqs = vec![
            stat(1, 0, 0, 100, 500, Some(800)),
            stat(2, 1, 0, 100, 500, Some(800)), // lane 1 first request
            stat(3, 0, 0, 601, 300, Some(0)),   // lane 0 miss
        ];
        let d = classify(&reqs);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].seq, 3);
    }
}
