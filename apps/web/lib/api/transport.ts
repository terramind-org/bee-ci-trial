import type { ErrorDetail } from "./types";

export interface CallOptions {
  signal?: AbortSignal;
}

/** Connect error codes (https://connectrpc.com/docs/protocol#error-codes). */
export type ConnectCode =
  | "canceled"
  | "unknown"
  | "invalid_argument"
  | "deadline_exceeded"
  | "not_found"
  | "already_exists"
  | "permission_denied"
  | "resource_exhausted"
  | "failed_precondition"
  | "aborted"
  | "out_of_range"
  | "unimplemented"
  | "internal"
  | "unavailable"
  | "data_loss"
  | "unauthenticated";

/**
 * An RPC failure. `reason` is the stable SPEC §12.2 reason (e.g. POLICY_DENIED); UI code
 * branches on it, never on the message. `rpc.ts` builds it from the bee.v1.ErrorDetail that
 * rides in the Connect error, and turns it back into one on the fixture's server side.
 */
const BRAND = Symbol.for("bee.v1.BeeError");

export class BeeError extends Error {
  readonly code: ConnectCode;
  readonly detail: ErrorDetail;
  readonly [BRAND] = true;

  /**
   * Server bundles can load this module more than once (the fixture store is a process-wide
   * singleton created by whichever route touched it first), so identity checks use a registered
   * symbol instead of the class object.
   */
  static override [Symbol.hasInstance](v: unknown): boolean {
    return typeof v === "object" && v !== null && (v as Record<symbol, unknown>)[BRAND] === true;
  }

  constructor(code: ConnectCode, detail: Partial<ErrorDetail> & { reason: string; message: string }) {
    super(detail.message);
    this.name = "BeeError";
    this.code = code;
    this.detail = { retryable: false, ...detail };
  }

  get reason(): string {
    return this.detail.reason;
  }

  get retryable(): boolean {
    return this.detail.retryable;
  }

  /**
   * A Connect JSON error body, for plain HTTP routes outside the RPC surface (`/api/session`).
   * The ErrorDetail rides in `details[].debug` as proto3 JSON (SPEC §12.2).
   */
  toJSON(): ConnectErrorBody {
    return { code: this.code, message: this.message, details: [{ type: "bee.v1.ErrorDetail", value: "", debug: this.detail }] };
  }

  static fromJSON(body: unknown, fallbackStatus = 500): BeeError {
    const b = (body ?? {}) as Partial<ConnectErrorBody>;
    const code = isConnectCode(b.code) ? b.code : fallbackStatus === 401 ? "unauthenticated" : "unknown";
    const detail = b.details?.find((d) => d.type === "bee.v1.ErrorDetail")?.debug;
    const message = typeof b.message === "string" && b.message ? b.message : `request failed (${fallbackStatus})`;
    return new BeeError(code, {
      reason: detail?.reason ?? code.toUpperCase(),
      message: detail?.message ?? message,
      retryable: detail?.retryable ?? (code === "unavailable" || code === "resource_exhausted"),
      leaseConflict: detail?.leaseConflict,
      policyDenial: detail?.policyDenial,
    });
  }
}

export interface ConnectErrorBody {
  code: ConnectCode;
  message: string;
  details: { type: string; value: string; debug?: ErrorDetail }[];
}

const HTTP_STATUS: Record<ConnectCode, number> = {
  canceled: 499,
  unknown: 500,
  invalid_argument: 400,
  deadline_exceeded: 504,
  not_found: 404,
  already_exists: 409,
  permission_denied: 403,
  resource_exhausted: 429,
  failed_precondition: 400,
  aborted: 409,
  out_of_range: 400,
  unimplemented: 501,
  internal: 500,
  unavailable: 503,
  data_loss: 500,
  unauthenticated: 401,
};

export function httpStatusOf(code: ConnectCode): number {
  return HTTP_STATUS[code];
}

export function isConnectCode(v: unknown): v is ConnectCode {
  return typeof v === "string" && v in HTTP_STATUS;
}
