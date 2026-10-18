import { useCallback, useEffect, useRef, useState } from "react";
import { backend, BackendHTTPError } from "./backend";
import type { KiroLoginOptions, OAuthSessionInitData, OAuthSessionStatusData } from "./types";

type Ready = Extract<OAuthSessionStatusData, { status: "ready" }>;

export function oauthCompletionMode(data: Pick<OAuthSessionInitData, "completion_mode" | "requires_manual_code"> | null) {
  return data?.completion_mode ?? (data?.requires_manual_code ? "manual_exchange" : "poll");
}

export type OAuthSessionIntent =
  | { kind: "reauthorize_legacy_provider"; providerId: string; useProxy: boolean }
  | { kind: "add_account"; providerId: string }
  | { kind: "reauthorize_account"; providerId: string; accountId: string; expectedVersion: number };

export interface OAuthSessionContext {
  /** UI snapshot only; the backend owns and validates the session's target. */
  intent?: OAuthSessionIntent;
  sessionId: string;
  signal: AbortSignal;
  isCurrent: () => boolean;
}

interface StartOptions {
  vendor: string;
  useProxy: boolean;
  kiro?: KiroLoginOptions;
  /** Custom backend initialization retains cancellation/polling/expiry handling. */
  init?: () => Promise<OAuthSessionInitData>;
  intent?: OAuthSessionIntent;
  onInit?: (session: OAuthSessionInitData) => void;
  onReady?: (status: Ready, context: OAuthSessionContext) => Promise<void> | void;
}

interface SessionRun {
  generation: number;
  sessionId: string | null;
  controller: AbortController;
  timer?: ReturnType<typeof setTimeout>;
  expiryTimer?: ReturnType<typeof setTimeout>;
  expiresAt?: number;
  interval: number;
  failures: number;
  processing: boolean;
  mode: "manual_exchange" | "poll";
  onReady?: StartOptions["onReady"];
  intent?: OAuthSessionIntent;
}

function cancelRemote(sessionId: string) {
  // Cleanup must still run after the session's request controller was aborted.
  void backend("cancel_oauth_session", { sessionId }).catch(() => {});
}

function disposeRun(run: SessionRun | null, cancel: boolean) {
  if (!run) return;
  clearTimeout(run.timer);
  clearTimeout(run.expiryTimer);
  run.controller.abort();
  if (cancel && run.sessionId) cancelRemote(run.sessionId);
}

function intervalMs(seconds: number) {
  return Number.isFinite(seconds) && seconds > 0 ? Math.max(1, seconds) * 1000 : 2000;
}

function transient(error: unknown) {
  if (error instanceof BackendHTTPError) {
    return [408, 429, 500, 502, 503, 504].includes(error.status);
  }
  // Fetch transport errors and the equivalent string errors returned by IPC.
  return error instanceof TypeError || /network|fetch|connection|timed?\s*out|temporarily unavailable/i.test(String(error));
}

/** One in-flight request per session, shared by provider creation and reconnect. */
export function useOAuthSession() {
  const active = useRef<SessionRun | null>(null);
  const generation = useRef(0);
  const [session, setSession] = useState<OAuthSessionInitData | null>(null);
  const [status, setStatus] = useState<OAuthSessionStatusData | null>(null);
  const [busy, setBusy] = useState(false);
  const [retryCount, setRetryCount] = useState(0);
  const [expiresAt, setExpiresAt] = useState<number | null>(null);

  const reset = useCallback((cancel = true) => {
    generation.current += 1;
    disposeRun(active.current, cancel);
    active.current = null;
    setSession(null);
    setStatus(null);
    setBusy(false);
    setRetryCount(0);
    setExpiresAt(null);
  }, []);

  useEffect(() => () => {
    generation.current += 1;
    disposeRun(active.current, true);
    active.current = null;
  }, []);

  function isCurrent(run: SessionRun) {
    return active.current === run && generation.current === run.generation && !run.controller.signal.aborted;
  }

  function fail(run: SessionRun, code: string, error: unknown) {
    if (!isCurrent(run)) return;
    disposeRun(run, true);
    active.current = null;
    setBusy(false);
    setRetryCount(0);
    setExpiresAt(null);
    setStatus({ status: "error", code, message: error instanceof Error ? error.message : String(error) });
  }

  function schedule(run: SessionRun, delay: number) {
    clearTimeout(run.timer);
    run.timer = setTimeout(() => { void poll(run); }, delay);
  }

  async function accept(run: SessionRun, next: OAuthSessionStatusData) {
    if (!isCurrent(run)) return;
    setRetryCount(0);
    run.failures = 0;
    if (next.status === "error") {
      fail(run, next.code, next.message);
      return;
    }
    setStatus(next);
    clearTimeout(run.expiryTimer);
    // Ready sessions also expire: expires_in is the backend's remaining
    // consumption TTL, not the lifetime of the eventual provider credential.
    if (!Number.isFinite(next.expires_in) || next.expires_in <= 0) {
      fail(run, "OAUTH_TIMEOUT", "OAuth session timed out. Please start authorization again.");
      return;
    }
    run.expiresAt = Date.now() + next.expires_in * 1000;
    setExpiresAt(run.expiresAt);
    run.expiryTimer = setTimeout(() => {
      fail(run, "OAUTH_TIMEOUT", "OAuth session timed out. Please start authorization again.");
    }, next.expires_in * 1000);
    if (next.status === "pending") {
      run.mode = oauthCompletionMode(next);
      // Each pending response controls the NEXT delay; never use an async interval.
      run.interval = intervalMs(next.interval);
      setBusy(run.mode === "poll");
      if (run.mode === "poll") schedule(run, run.interval);
      return;
    }
    clearTimeout(run.timer);
    setBusy(Boolean(run.onReady));
    try {
      if (run.sessionId) {
        const sessionId = run.sessionId;
        await run.onReady?.(next, {
          intent: run.intent,
          sessionId,
          signal: run.controller.signal,
          isCurrent: () => isCurrent(run) && run.sessionId === sessionId,
        });
      }
      if (isCurrent(run)) setBusy(false);
    } catch (error) {
      fail(run, "OAUTH_BIND_FAILED", error);
    }
  }

  async function poll(run: SessionRun) {
    if (!isCurrent(run) || !run.sessionId || run.processing) return;
    const sessionId = run.sessionId;
    run.processing = true;
    try {
      const next = await backend<OAuthSessionStatusData>("get_oauth_session_status", { sessionId }, { signal: run.controller.signal });
      if (isCurrent(run) && run.sessionId === sessionId) await accept(run, next);
    } catch (error) {
      if (!isCurrent(run)) return;
      if (transient(error) && run.failures < 3) {
        run.failures += 1;
        setRetryCount(run.failures);
        schedule(run, Math.max(run.interval, Math.min(30000, run.interval * 2 ** run.failures)));
      } else {
        fail(run, "OAUTH_STATUS_FAILED", error);
      }
    } finally {
      run.processing = false;
    }
  }

  async function start(options: StartOptions) {
    reset();
    const run: SessionRun = {
      generation: generation.current,
      sessionId: null,
      controller: new AbortController(),
      interval: 2000,
      failures: 0,
      processing: false,
      mode: "poll",
      onReady: options.onReady,
      intent: options.intent ? { ...options.intent } : undefined,
    };
    active.current = run;
    setBusy(true);
    try {
      // Do not abort init: it may allocate a remote session before cancellation.
      // Receive its ID even when superseded so that session can be cancelled too.
      const init = await (options.init ? options.init() : backend<OAuthSessionInitData>("init_oauth_session", {
        vendor: options.vendor,
        useProxy: options.useProxy,
        ...(options.kiro ? { kiro: options.kiro } : {}),
      }));
      if (!isCurrent(run)) {
        cancelRemote(init.session_id);
        return;
      }
      run.sessionId = init.session_id;
      setSession(init);
      options.onInit?.(init);
      await accept(run, { ...init, status: "pending" });
    } catch (error) {
      fail(run, "OAUTH_INIT_FAILED", error);
    }
  }

  async function complete(input: { code?: string; callbackUrl?: string }) {
    const run = active.current;
    if (!run?.sessionId || !isCurrent(run) || run.processing || run.mode !== "manual_exchange") return;
    run.processing = true;
    setBusy(true);
    try {
      const next = await backend<OAuthSessionStatusData>("complete_oauth_session", { sessionId: run.sessionId, ...input }, { signal: run.controller.signal });
      await accept(run, next);
    } catch (error) {
      fail(run, "OAUTH_COMPLETE_FAILED", error);
    } finally {
      run.processing = false;
    }
  }

  function cancel() {
    reset();
    setStatus({ status: "error", code: "OAUTH_CANCELLED", message: "Authorization cancelled. Please start again when ready." });
  }

  function ownsSession(sessionId: string) {
    const run = active.current;
    return Boolean(run && run.sessionId === sessionId && isCurrent(run));
  }

  function canConsume(sessionId: string) {
    const run = active.current;
    if (!run || !ownsSession(sessionId)) return false;
    // Timers may be throttled in background tabs. Check the deadline again
    // synchronously before a create/bind request can consume a ready session.
    if (!run.expiresAt || run.expiresAt <= Date.now()) {
      fail(run, "OAUTH_TIMEOUT", "OAuth session timed out. Please start authorization again.");
      return false;
    }
    return status?.status === "ready";
  }

  return { session, status, busy, retryCount, expiresAt, start, complete, cancel, reset, ownsSession, canConsume };
}
