// Block lifecycle types for the herdr:blocked producer.
//
// "attention" blocks come from ad:notify:attention (a tool or run is waiting
// for the user). "dangerous" blocks come from ad:notify:dangerous (a guardrail
// prompt). "error" blocks come from ad:notify:done with status:"error" (an
// agent-level failure; see handleError in index.ts).

export type BlockKind = "attention" | "dangerous" | "error";

export interface ActiveBlock {
  kind: BlockKind;
  toolCallId?: string;
}
