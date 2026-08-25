// Inlined from @harness/events (pi-harness). Kept local so this extension
// loads standalone as a pi package without the private @harness/* workspace.

export const AD_NOTIFY_DANGEROUS_EVENT = "ad:notify:dangerous";
export const AD_NOTIFY_ATTENTION_EVENT = "ad:notify:attention";
export const AD_NOTIFY_DONE_EVENT = "ad:notify:done";

export interface AdNotifyDangerousEvent {
  description: string;
  toolName?: string;
  toolCallId?: string;
}

export interface AdNotifyAttentionEvent {
  description?: string;
  reason?: string;
  toolName?: string;
  toolCallId?: string;
}

export interface AdNotifyDoneEvent {
  summary?: string;
  status?: "ok" | "error";
  loops?: number;
  toolCalls?: number;
}

/**
 * Block lifecycle event published on the pi event bus and consumed by
 * herdr's installed `herdr-agent-state` pi extension, which translates it
 * into `pane.report_agent` socket requests.
 *
 * Emitted with `{ active: true, label }` when a block starts and
 * `{ active: false }` when it clears. Pairs are per-key balanced: every
 * `active: true` is matched by exactly one `active: false`.
 */
export const HERDR_BLOCKED_EVENT = "herdr:blocked";

export interface HerdrBlockedEvent {
  /** `true` when a block starts, `false` when it clears. */
  active: boolean;
  /** Human-readable reason for the block. Only present on `active: true`. */
  label?: string;
}
