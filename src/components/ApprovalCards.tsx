// Write-tool approval cards. Rendered app-wide from store.approvals; answers
// via resolve_approval — denial and 120s timeout both fail closed.
import { useState } from "react";
import { useApp } from "../store";
import * as api from "../lib/api";

function ApprovalCard({ id }: { id: string }) {
  const approval = useApp((s) => s.approvals[id]);
  const toast = useApp((s) => s.toast);
  const [remember, setRemember] = useState(false);
  const [answering, setAnswering] = useState(false);
  if (!approval) return null;

  const answer = (approved: boolean) => {
    setAnswering(true);
    void api
      .resolveApproval(approval.approvalId, approval.sessionId, approval.tool, approved, remember)
      .catch((e) => toast("error", `审批提交失败: ${String(e)}`))
      .finally(() => {
        useApp.setState((s) => {
          const approvals = { ...s.approvals };
          delete approvals[id];
          return { approvals };
        });
      });
  };

  return (
    <div className="approval-card">
      <div className="ac-head">
        <span className="ac-badge">写入审批</span>
        <span className="mono ac-tool">{approval.tool}</span>
        <span className="mono ac-path">{approval.path}</span>
      </div>
      <pre className="ac-preview">{approval.preview}</pre>
      <label className="ac-remember">
        <button
          className={`switch ${remember ? "on" : ""}`}
          role="switch"
          aria-checked={remember}
          onClick={() => setRemember((r) => !r)}
        />
        <span>本次会话内记住对该工具的授权（不再逐次询问）</span>
      </label>
      <div className="ac-actions">
        <span className="ac-timeout">120 秒未响应将自动拒绝</span>
        <button className="btn small danger" disabled={answering} onClick={() => answer(false)}>
          拒绝
        </button>
        <button className="btn small primary" disabled={answering} onClick={() => answer(true)}>
          批准执行
        </button>
      </div>
    </div>
  );
}

export function ApprovalCards() {
  const approvals = useApp((s) => s.approvals);
  const ids = Object.keys(approvals);
  if (ids.length === 0) return null;
  return (
    <div className="approval-stack">
      {ids.map((id) => (
        <ApprovalCard key={id} id={id} />
      ))}
    </div>
  );
}
