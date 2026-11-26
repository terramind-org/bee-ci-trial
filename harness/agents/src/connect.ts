// A minimal Connect-protocol HTTP/JSON client (SPEC §12.1): unary, server-streaming and
// half-duplex client-streaming calls, with errors decoded to their SPEC §12.2 reason.

export interface RpcError {
  /** Connect code, e.g. "permission_denied". */
  code: string;
  message: string;
  /** SPEC §12.2 reason without the ERROR_REASON_ prefix, e.g. "POLICY_DENIED". */
  reason: string;
  retryable: boolean;
  /** The ErrorDetail in proto3-JSON, when the server sent its debug form. */
  detail: Record<string, unknown>;
  httpStatus: number;
}

export interface RpcResult {
  ok: boolean;
  /** The response message (unary/client-streaming) or `{messages: [...]}` for server streams. */
  body: unknown;
  error?: RpcError;
  ms: number;
}

export interface CallOptions {
  token?: string;
  timeoutMs?: number;
  headers?: Record<string, string>;
  /** Server streams: stop reading after this many messages. */
  maxMessages?: number;
}

const CODE_REASON: Record<string, string> = {
  canceled: "CANCELLED",
  invalid_argument: "INVALID_ARGUMENT",
  out_of_range: "INVALID_ARGUMENT",
  deadline_exceeded: "DEADLINE_EXCEEDED",
  not_found: "NOT_FOUND",
  already_exists: "ALREADY_EXISTS",
  permission_denied: "PERMISSION_DENIED",
  resource_exhausted: "RESOURCE_EXHAUSTED",
  failed_precondition: "INVALID_STATE",
  aborted: "VERSION_MISMATCH",
  unimplemented: "UNIMPLEMENTED",
  unavailable: "UNAVAILABLE",
  data_loss: "INTEGRITY",
  unauthenticated: "UNAUTHENTICATED",
};

const HTTP_CODE: Record<number, string> = {
  400: "invalid_argument",
  401: "unauthenticated",
  403: "permission_denied",
  404: "unimplemented",
  408: "deadline_exceeded",
  409: "aborted",
  429: "resource_exhausted",
  499: "canceled",
  501: "unimplemented",
  502: "unavailable",
  503: "unavailable",
  504: "deadline_exceeded",
};

export function normalizeReason(r: string): string {
  return r.replace(/^ERROR_REASON_/, "");
}

export function parseError(httpStatus: number, raw: unknown): RpcError {
  const obj = (raw && typeof raw === "object" ? raw : {}) as Record<string, unknown>;
  const code = typeof obj.code === "string" ? obj.code : (HTTP_CODE[httpStatus] ?? "unknown");
  const details = Array.isArray(obj.details) ? (obj.details as Record<string, unknown>[]) : [];
  const d = details.find((x) => String(x.type ?? "").endsWith("ErrorDetail"));
  const debug = (d?.debug && typeof d.debug === "object" ? d.debug : {}) as Record<string, unknown>;
  const reason = typeof debug.reason === "string" && debug.reason ? normalizeReason(debug.reason) : (CODE_REASON[code] ?? "INTERNAL");
  return {
    code,
    message: typeof obj.message === "string" ? obj.message : "",
    reason,
    retryable: debug.retryable === true,
    detail: debug,
    httpStatus,
  };
}

function frame(flags: number, payload: Uint8Array): Uint8Array {
  const out = new Uint8Array(5 + payload.length);
  out[0] = flags;
  new DataView(out.buffer).setUint32(1, payload.length, false);
  out.set(payload, 5);
  return out;
}

/** Splits a Connect streaming body into its envelopes. */
export function unframe(buf: Uint8Array): { flags: number; payload: Uint8Array }[] {
  const out: { flags: number; payload: Uint8Array }[] = [];
  let i = 0;
  while (i + 5 <= buf.length) {
    const flags = buf[i]!;
    const len = new DataView(buf.buffer, buf.byteOffset + i + 1, 4).getUint32(0, false);
    if (i + 5 + len > buf.length) break;
    out.push({ flags, payload: buf.subarray(i + 5, i + 5 + len) });
    i += 5 + len;
  }
  return out;
}

export function encodeEnvelopes(messages: unknown[], end?: unknown): Uint8Array {
  const enc = new TextEncoder();
  const parts = messages.map((m) => frame(0, enc.encode(JSON.stringify(m))));
  if (end !== undefined) parts.push(frame(2, enc.encode(JSON.stringify(end))));
  const total = parts.reduce((n, p) => n + p.length, 0);
  const out = new Uint8Array(total);
  let o = 0;
  for (const p of parts) {
    out.set(p, o);
    o += p.length;
  }
  return out;
}

/** The socket path of a `unix:/path` or `unix:///path` endpoint (a local node daemon), else undefined. */
export function unixSocket(endpoint: string): string | undefined {
  const m = /^unix:(?:\/\/)?(\/.*)$/.exec(endpoint);
  return m ? m[1] : undefined;
}

export class ConnectClient {
  constructor(
    readonly baseUrl: string,
    readonly defaultToken?: string,
  ) {}

  private url(service: string, method: string): string {
    const base = unixSocket(this.baseUrl) ? "http://localhost" : this.baseUrl.replace(/\/+$/, "");
    return `${base}/bee.v1.${service}/${method}`;
  }

  private headers(contentType: string, opts: CallOptions): Record<string, string> {
    const h: Record<string, string> = {
      "content-type": contentType,
      "connect-protocol-version": "1",
      ...opts.headers,
    };
    const token = opts.token ?? this.defaultToken;
    if (token) h.authorization = `Bearer ${token}`;
    if (opts.timeoutMs) h["connect-timeout-ms"] = String(opts.timeoutMs);
    return h;
  }

  private async send(url: string, init: RequestInit, timeoutMs: number): Promise<Response> {
    const unix = unixSocket(this.baseUrl);
    return fetch(url, { ...init, ...(unix ? { unix } : {}), signal: AbortSignal.timeout(timeoutMs) });
  }

  async unary(service: string, method: string, body: unknown, opts: CallOptions = {}): Promise<RpcResult> {
    const t0 = performance.now();
    const timeout = opts.timeoutMs ?? 30_000;
    try {
      const res = await this.send(
        this.url(service, method),
        { method: "POST", headers: this.headers("application/json", opts), body: JSON.stringify(body ?? {}) },
        timeout,
      );
      const text = await res.text();
      let parsed: unknown = {};
      try {
        parsed = text ? (JSON.parse(text) as unknown) : {};
      } catch {
        parsed = { message: text.slice(0, 500) };
      }
      const ms = performance.now() - t0;
      if (res.status === 200) return { ok: true, body: parsed, ms };
      return { ok: false, body: parsed, error: parseError(res.status, parsed), ms };
    } catch (e) {
      return transportError(e, performance.now() - t0);
    }
  }

  async serverStream(service: string, method: string, body: unknown, opts: CallOptions = {}): Promise<RpcResult> {
    const t0 = performance.now();
    const timeout = opts.timeoutMs ?? 30_000;
    const max = opts.maxMessages ?? Number.POSITIVE_INFINITY;
    const ctrl = new AbortController();
    const timer = setTimeout(() => ctrl.abort(), timeout);
    const messages: unknown[] = [];
    try {
      const res = await fetch(this.url(service, method), {
        method: "POST",
        headers: this.headers("application/connect+json", opts),
        body: encodeEnvelopes([body ?? {}]),
        signal: ctrl.signal,
      });
      if (res.status !== 200) {
        const text = await res.text();
        let parsed: unknown = {};
        try {
          parsed = JSON.parse(text) as unknown;
        } catch {
          parsed = { message: text.slice(0, 500) };
        }
        return { ok: false, body: parsed, error: parseError(res.status, parsed), ms: performance.now() - t0 };
      }
      const reader = res.body!.getReader();
      let buf: Uint8Array = new Uint8Array(0);
      const dec = new TextDecoder();
      for (;;) {
        let chunk: Awaited<ReturnType<typeof reader.read>>;
        try {
          chunk = await reader.read();
        } catch {
          break; // timeout: return what arrived
        }
        if (chunk.done) break;
        const merged = new Uint8Array(buf.length + chunk.value.length);
        merged.set(buf);
        merged.set(chunk.value, buf.length);
        buf = merged;
        let consumed = 0;
        for (const env of unframe(buf)) {
          consumed += 5 + env.payload.length;
          const json = JSON.parse(dec.decode(env.payload)) as Record<string, unknown>;
          if (env.flags & 2) {
            clearTimeout(timer);
            if (json.error) {
              return { ok: false, body: { messages }, error: parseError(200, json.error), ms: performance.now() - t0 };
            }
            return { ok: true, body: { messages }, ms: performance.now() - t0 };
          }
          messages.push(json);
          if (messages.length >= max) {
            ctrl.abort();
            clearTimeout(timer);
            return { ok: true, body: { messages }, ms: performance.now() - t0 };
          }
        }
        buf = buf.subarray(consumed);
      }
      clearTimeout(timer);
      return { ok: true, body: { messages }, ms: performance.now() - t0 };
    } catch (e) {
      clearTimeout(timer);
      if (messages.length) return { ok: true, body: { messages }, ms: performance.now() - t0 };
      return transportError(e, performance.now() - t0);
    }
  }

  async clientStream(service: string, method: string, items: unknown[], opts: CallOptions = {}): Promise<RpcResult> {
    const t0 = performance.now();
    try {
      const res = await this.send(
        this.url(service, method),
        { method: "POST", headers: this.headers("application/connect+json", opts), // A Connect request carries no end-stream frame (only responses do); the server rejects one.
          body: encodeEnvelopes(items) },
        opts.timeoutMs ?? 60_000,
      );
      const buf = new Uint8Array(await res.arrayBuffer());
      const ms = performance.now() - t0;
      if (res.status !== 200) {
        let parsed: unknown = {};
        try {
          parsed = JSON.parse(new TextDecoder().decode(buf)) as unknown;
        } catch {
          /* not JSON */
        }
        return { ok: false, body: parsed, error: parseError(res.status, parsed), ms };
      }
      const dec = new TextDecoder();
      let message: unknown = {};
      for (const env of unframe(buf)) {
        const json = JSON.parse(dec.decode(env.payload)) as Record<string, unknown>;
        if (env.flags & 2) {
          if (json.error) return { ok: false, body: message, error: parseError(200, json.error), ms };
        } else {
          message = json;
        }
      }
      return { ok: true, body: message, ms };
    } catch (e) {
      return transportError(e, performance.now() - t0);
    }
  }
}

function transportError(e: unknown, ms: number): RpcResult {
  const timeout = e instanceof Error && (e.name === "TimeoutError" || e.name === "AbortError");
  return {
    ok: false,
    body: {},
    ms,
    error: {
      code: timeout ? "deadline_exceeded" : "unavailable",
      message: e instanceof Error ? e.message : String(e),
      reason: timeout ? "DEADLINE_EXCEEDED" : "UNAVAILABLE",
      retryable: true,
      detail: {},
      httpStatus: 0,
    },
  };
}
