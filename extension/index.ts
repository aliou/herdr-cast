import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import {
  AD_NOTIFY_ATTENTION_EVENT,
  AD_NOTIFY_DANGEROUS_EVENT,
  AD_NOTIFY_DONE_EVENT,
  HERDR_BLOCKED_EVENT,
  type AdNotifyAttentionEvent,
  type AdNotifyDangerousEvent,
  type AdNotifyDoneEvent,
  type HerdrBlockedEvent,
} from "./events";
import type { ActiveBlock, BlockKind } from "./types";

type DangerousNotification = AdNotifyDangerousEvent & {
  source?: string;
};

const activeBlocks = new Map<string, ActiveBlock>();

export function _resetForTesting(): void {
  activeBlocks.clear();
}

function block(
  pi: ExtensionAPI,
  key: string,
  activeBlock: ActiveBlock,
  label: string,
): void {
  if (activeBlocks.has(key)) return;

  activeBlocks.set(key, activeBlock);
  pi.events.emit(HERDR_BLOCKED_EVENT, { active: true, label } satisfies HerdrBlockedEvent);
}

function unblock(pi: ExtensionAPI, key: string): void {
  if (!activeBlocks.delete(key)) return;
  pi.events.emit(HERDR_BLOCKED_EVENT, { active: false } satisfies HerdrBlockedEvent);
}

function unblockWhere(
  pi: ExtensionAPI,
  predicate: (activeBlock: ActiveBlock) => boolean,
): void {
  for (const [key, activeBlock] of activeBlocks) {
    if (predicate(activeBlock)) unblock(pi, key);
  }
}

function notificationKey(kind: BlockKind, toolCallId?: string): string {
  return toolCallId ? `${kind}:${toolCallId}` : kind;
}

function handleDangerous(pi: ExtensionAPI): () => void {
  return pi.events.on(AD_NOTIFY_DANGEROUS_EVENT, (data) => {
    const payload = data as DangerousNotification;
    // Guardrails' own Herdr adapter tracks the approval prompt precisely.
    // This compatibility notification is still useful to other harness hooks,
    // but treating it as another Herdr block leaves an uncorrelated block.
    if (payload.source === "defaults:event-compat:guardrails") return;

    block(
      pi,
      notificationKey("dangerous", payload.toolCallId),
      { kind: "dangerous", toolCallId: payload.toolCallId },
      payload.description || "Dangerous action detected",
    );
  });
}

function handleAttention(pi: ExtensionAPI): () => void {
  return pi.events.on(AD_NOTIFY_ATTENTION_EVENT, (data) => {
    const payload = data as AdNotifyAttentionEvent;
    block(
      pi,
      notificationKey("attention", payload.toolCallId),
      { kind: "attention", toolCallId: payload.toolCallId },
      payload.description ?? payload.reason ?? "Waiting for user input",
    );
  });
}

function handleError(pi: ExtensionAPI): () => void {
  return pi.events.on(AD_NOTIFY_DONE_EVENT, (data) => {
    const payload = data as AdNotifyDoneEvent;
    if (payload.status === "error") {
      block(pi, "error", { kind: "error" }, "An error occurred");
    } else if (payload.status === "ok") {
      // A successful run resolves a prior error block (e.g. a retry that
      // recovered). No-op when no error block is active.
      unblock(pi, "error");
    }
  });
}

export default function herdr(pi: ExtensionAPI): void {
  const stopListening = [
    handleError(pi),
    handleDangerous(pi),
    handleAttention(pi),
  ];

  pi.on("tool_execution_end", (event) => {
    unblockWhere(
      pi,
      ({ kind, toolCallId }) =>
        kind !== "error" && toolCallId === event.toolCallId,
    );
  });

  pi.on("agent_start", () => {
    // A retry fires agent_start again before agent_settled. The prior run
    // may have errored on a low-level run that pi recovered from (or that
    // the user aborted), so clear every kind-only block on every start,
    // including "error". Per-toolCallId attention/dangerous blocks survive
    // until their tool_execution_end or agent_settled.
    unblockWhere(pi, ({ toolCallId }) => !toolCallId);
  });

  pi.on("agent_settled", () => {
    // Non-error blocks (attention/dangerous) are per-run. Error blocks
    // persist past a failed settle until the next agent_start, a successful
    // run, or shutdown.
    unblockWhere(pi, ({ kind }) => kind !== "error");
  });

  pi.on("session_shutdown", () => {
    unblockWhere(pi, () => true);
    for (const stop of stopListening) stop();
  });
}
