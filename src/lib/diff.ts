// Line-level diff via LCS dynamic programming. Bounded: sides larger than
// MAX_LINES fall back to a truncated summary instead of O(n*m) blowing up.
import type { DiffLine, DiffResult } from "../types";

const MAX_LINES = 1500;

export function lineDiff(before: string, after: string): DiffResult {
  const a = before.replace(/\r\n/g, "\n").split("\n");
  const b = after.replace(/\r\n/g, "\n").split("\n");
  // strip one trailing empty element produced by a final newline
  if (a.length > 1 && a[a.length - 1] === "") a.pop();
  if (b.length > 1 && b[b.length - 1] === "") b.pop();

  const truncated = a.length > MAX_LINES || b.length > MAX_LINES;
  if (truncated) {
    // degraded mode: show head of both sides + counters, no alignment
    const lines: DiffLine[] = [];
    for (const t of a.slice(0, 40)) lines.push({ type: "del", text: t });
    for (const t of b.slice(0, 40)) lines.push({ type: "add", text: t });
    return {
      lines: [
        { type: "same", text: `文件过大（${a.length} → ${b.length} 行），跳过逐行对齐，仅显示两侧前 40 行：` },
        ...lines,
      ],
      addCount: b.length,
      delCount: a.length,
      truncated: true,
    };
  }

  // LCS table (Int32Array keeps 1500×1500 cheap)
  const n = a.length;
  const m = b.length;
  const dp = new Int32Array((n + 1) * (m + 1));
  const at = (i: number, j: number) => i * (m + 1) + j;
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      dp[at(i, j)] = a[i] === b[j] ? dp[at(i + 1, j + 1)] + 1 : Math.max(dp[at(i + 1, j)], dp[at(i, j + 1)]);
    }
  }

  const lines: DiffLine[] = [];
  let addCount = 0;
  let delCount = 0;
  let i = 0;
  let j = 0;
  while (i < n && j < m) {
    if (a[i] === b[j]) {
      lines.push({ type: "same", text: a[i] });
      i++;
      j++;
    } else if (dp[at(i + 1, j)] >= dp[at(i, j + 1)]) {
      lines.push({ type: "del", text: a[i] });
      delCount++;
      i++;
    } else {
      lines.push({ type: "add", text: b[j] });
      addCount++;
      j++;
    }
  }
  while (i < n) {
    lines.push({ type: "del", text: a[i] });
    delCount++;
    i++;
  }
  while (j < m) {
    lines.push({ type: "add", text: b[j] });
    addCount++;
    j++;
  }

  // collapse long runs of unchanged context to keep the view scannable
  const CONTEXT = 3;
  const out: DiffLine[] = [];
  let run: number[] = [];
  const flushRun = () => {
    if (run.length > CONTEXT * 2 + 2) {
      for (const idx of run.slice(0, CONTEXT)) out.push(lines[idx]);
      out.push({ type: "same", text: `⋯ ${run.length - CONTEXT * 2} 行未变化 ⋯` });
      for (const idx of run.slice(-CONTEXT)) out.push(lines[idx]);
    } else {
      for (const idx of run) out.push(lines[idx]);
    }
    run = [];
  };
  lines.forEach((l, idx) => {
    if (l.type === "same") run.push(idx);
    else flushRun();
  });
  // flush the trailing run but keep its head only if it opens the file
  if (run.length > 0) {
    if (run.length > CONTEXT && out.length > 0 && out.some((l) => l.type !== "same" || !l.text.startsWith("⋯"))) {
      for (const idx of run.slice(0, CONTEXT)) out.push(lines[idx]);
    } else {
      for (const idx of run) out.push(lines[idx]);
    }
  }

  return { lines: out, addCount, delCount, truncated: false };
}
