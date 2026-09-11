// Transcript grouping: between two user messages, all assistant rounds and
// tool records of one turn belong together — the multi-round tool loop
// produces several assistant records that must render as ONE message.
import type { MessageRecord, UsageStat } from "../types";

export interface TranscriptItem {
  kind: "user" | "assistantGroup" | "notice";
  user?: MessageRecord;
  group?: MessageRecord[]; // ts-ordered assistant + tool records of one turn
  notice?: MessageRecord; // display-only cache-miss warning (never model context)
}

export function groupTranscript(msgs: MessageRecord[]): TranscriptItem[] {
  const sorted = [...msgs].sort((a, b) => a.ts - b.ts);
  const items: TranscriptItem[] = [];
  let cur: MessageRecord[] | null = null;
  for (const m of sorted) {
    if (m.role === "user") {
      cur = null;
      items.push({ kind: "user", user: m });
    } else if (m.role === "notice") {
      cur = null;
      items.push({ kind: "notice", notice: m });
    } else if (m.role === "assistant" || m.role === "tool") {
      if (!cur) {
        cur = [];
        items.push({ kind: "assistantGroup", group: cur });
      }
      cur.push(m);
    }
  }
  return items;
}

export function groupAssistants(group: MessageRecord[]): MessageRecord[] {
  return group.filter((m) => m.role === "assistant");
}

/** Sum usage across rounds; null when nothing was reported at all. */
export function aggregateUsage(assistants: MessageRecord[]): UsageStat | null {
  const inputs = assistants.map((a) => a.usage?.input).filter((v): v is number => v != null);
  const outputs = assistants.map((a) => a.usage?.output).filter((v): v is number => v != null);
  const cached = assistants.map((a) => a.usage?.cached).filter((v): v is number => v != null);
  if (inputs.length === 0 && outputs.length === 0 && cached.length === 0) {
    return assistants.some((a) => a.usage != null) ? assistants[assistants.length - 1].usage : null;
  }
  return {
    input: inputs.length ? inputs.reduce((x, y) => x + y, 0) : null,
    output: outputs.length ? outputs.reduce((x, y) => x + y, 0) : null,
    cached: cached.length ? cached.reduce((x, y) => x + y, 0) : null,
  };
}

export function aggregateCost(assistants: MessageRecord[]): number | null {
  const costs = assistants.map((a) => a.cost_usd).filter((v): v is number => v != null);
  return costs.length ? costs.reduce((x, y) => x + y, 0) : null;
}

export function aggregateConfidence(assistants: MessageRecord[]): number | null {
  for (let i = assistants.length - 1; i >= 0; i--) {
    if (assistants[i].confidence != null) return assistants[i].confidence;
  }
  return null;
}

export function aggregateStatus(assistants: MessageRecord[]): string {
  if (assistants.some((a) => a.status === "error")) return "error";
  if (assistants.some((a) => a.status === "stopped")) return "stopped";
  return "ok";
}
