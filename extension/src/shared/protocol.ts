/**
 * Envelope construction and validation for the extension side of the wire protocol.
 *
 * Types come from protocol.generated.ts, which is generated from
 * schema/protocol.schema.json. This module adds the runtime pieces a generated type
 * cannot provide: envelope construction, and validation of messages arriving from the
 * agent.
 *
 * Messages from the agent crossed a process boundary, so they are parsed defensively.
 * Unknown message types and unknown fields are ignored rather than fatal
 * (docs/05-ipc-protocol.md) so the two halves can be updated independently by their
 * respective stores.
 */

import type {
  AgentUnavailableBody,
  CloseTabsBody,
  CloseTabsResultBody,
  Envelope,
  HelloAckBody,
  HelloBody,
  RestoreResultBody,
  RestoreSessionBody,
  SettingsChangedBody,
  StateBody,
} from "./protocol.generated.js";

export const PROTOCOL_VERSION = 1 as const;

/** Messages this side sends. */
export type OutboundType =
  | "hello"
  | "tab_delta"
  | "full_state"
  | "restore_result"
  | "close_tabs_result";

export type OutboundBody =
  | HelloBody
  | StateBody
  | RestoreResultBody
  | CloseTabsResultBody;

/** Messages this side receives. */
export type InboundMessage =
  | { type: "hello_ack"; body: HelloAckBody }
  | { type: "restore_session"; body: RestoreSessionBody }
  | { type: "close_tabs"; body: CloseTabsBody }
  | { type: "settings_changed"; body: SettingsChangedBody }
  | { type: "agent_unavailable"; body: AgentUnavailableBody };

/**
 * Monotonic-ish unique id. Not a real ULID - we need uniqueness for correlation and
 * log tracing, not sortability or cryptographic properties.
 */
export function newMessageId(): string {
  const t = Date.now().toString(36).padStart(9, "0");
  const r = Math.random().toString(36).slice(2, 10);
  return `${t}${r}`;
}

/**
 * Builds an outbound envelope. Note there is no `src` - the relay stamps that, and a
 * value we set here would be overwritten anyway.
 */
export function envelope(type: OutboundType, body: OutboundBody): Omit<Envelope, "src"> {
  return {
    v: PROTOCOL_VERSION,
    id: newMessageId(),
    type,
    ts: Date.now(),
    body: body as Envelope["body"],
  };
}

function isRecord(x: unknown): x is Record<string, unknown> {
  return typeof x === "object" && x !== null && !Array.isArray(x);
}

/**
 * Validates a message from the agent.
 *
 * Returns null for anything we cannot safely act on - malformed, wrong protocol
 * version, or a type we do not recognize. Callers log and drop; they never throw, so
 * one bad message cannot take down the background context.
 */
export function parseInbound(raw: unknown): InboundMessage | null {
  if (!isRecord(raw)) return null;
  if (typeof raw["type"] !== "string") return null;

  // A missing version is tolerated (the relay's own control messages omit it);
  // a version from the future is not, since field meanings may have changed.
  const v = raw["v"];
  if (v !== undefined && (typeof v !== "number" || v > PROTOCOL_VERSION)) return null;

  const body = raw["body"];
  if (!isRecord(body)) return null;

  switch (raw["type"]) {
    case "hello_ack":
      if (typeof body["capture_enabled"] !== "boolean") return null;
      if (typeof body["capture_private"] !== "boolean") return null;
      return { type: "hello_ack", body: body as unknown as HelloAckBody };

    case "restore_session":
      if (typeof body["run_id"] !== "number") return null;
      if (!Array.isArray(body["windows"])) return null;
      return { type: "restore_session", body: body as unknown as RestoreSessionBody };

    case "close_tabs":
      if (typeof body["run_id"] !== "number") return null;
      if (!Array.isArray(body["urls"])) return null;
      return { type: "close_tabs", body: body as unknown as CloseTabsBody };

    case "settings_changed":
      if (typeof body["capture_enabled"] !== "boolean") return null;
      if (typeof body["capture_private"] !== "boolean") return null;
      return { type: "settings_changed", body: body as unknown as SettingsChangedBody };

    case "agent_unavailable":
      if (typeof body["reason"] !== "string") return null;
      return { type: "agent_unavailable", body: body as unknown as AgentUnavailableBody };

    default:
      return null;
  }
}
