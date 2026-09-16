/**
 * Native messaging connection to the relay.
 *
 * MV3 evicts the service worker routinely, so a `connectNative` port dying is **not an
 * error condition** (docs/07-extension.md). This deliberately does not try to hold a
 * port open: keeping the worker alive with heartbeats is fragile, burns battery, and
 * is a known cause of store review friction. It connects lazily, reconnects with
 * backoff, and lets the 60s reconcile repair anything missed in between.
 */

import { envelope, parseInbound, type InboundMessage, type OutboundBody, type OutboundType } from "../shared/protocol.js";

export const HOST_NAME = "com.sessionrestore.relay";

/** Backoff schedule, in ms. Ends at 5 minutes and stays there. */
const BACKOFF_MS = [5_000, 15_000, 60_000, 300_000];

export interface PortDeps {
  onMessage: (msg: InboundMessage) => void;
  onStatusChange?: (connected: boolean, reason?: string) => void;
}

export class AgentPort {
  private port: chrome.runtime.Port | null = null;
  private attempt = 0;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private lastError: string | undefined;

  constructor(private deps: PortDeps) {}

  get connected(): boolean {
    return this.port !== null;
  }

  get status(): string | undefined {
    return this.lastError;
  }

  /** Connects if not already connected. Safe to call on every event. */
  ensure(): void {
    if (this.port) return;
    if (this.reconnectTimer !== null) return;

    try {
      const p = chrome.runtime.connectNative(HOST_NAME);

      p.onMessage.addListener((raw: unknown) => {
        const msg = parseInbound(raw);
        if (!msg) {
          console.warn("[session-restore] dropped unusable message from agent");
          return;
        }
        if (msg.type === "agent_unavailable") {
          this.lastError = `agent unavailable: ${msg.body.reason}`;
          this.deps.onStatusChange?.(false, this.lastError);
          // The relay exits after this; let onDisconnect schedule the retry.
          return;
        }
        this.deps.onMessage(msg);
      });

      p.onDisconnect.addListener(() => {
        const err = chrome.runtime.lastError?.message;
        this.port = null;
        if (err) {
          this.lastError = err;
          this.deps.onStatusChange?.(false, err);
        }
        this.scheduleReconnect();
      });

      this.port = p;
      this.attempt = 0;
      this.lastError = undefined;
      this.deps.onStatusChange?.(true);
    } catch (e) {
      // Usually means the native messaging host is not registered - the installer did
      // not run, or a browser update cleared the registry key.
      this.lastError = e instanceof Error ? e.message : String(e);
      this.deps.onStatusChange?.(false, this.lastError);
      this.scheduleReconnect();
    }
  }

  private scheduleReconnect(): void {
    if (this.reconnectTimer !== null) return;
    const delay = BACKOFF_MS[Math.min(this.attempt, BACKOFF_MS.length - 1)]!;
    this.attempt++;
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      this.ensure();
    }, delay);
  }

  /**
   * Sends a message, connecting first if needed.
   *
   * Throws when the port is unavailable, which is what makes the outbox retain the
   * batch rather than dropping it.
   */
  send(type: OutboundType, body: OutboundBody): void {
    this.ensure();
    if (!this.port) throw new Error(this.lastError ?? "not connected to the agent");
    this.port.postMessage(envelope(type, body));
  }

  disconnect(): void {
    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    this.port?.disconnect();
    this.port = null;
  }
}
