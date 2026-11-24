/**
 * Typed calls over the generated service descriptors, in proto3 JSON.
 *
 *   const repo = await rpc.call(RepoService.method.getRepo, { repo: { fullName: "acme/api" } });
 *   for await (const ev of rpc.stream(EventService.method.subscribe, { repo })) { … }
 *
 * Requests and responses are the generated `*Json` types (lowerCamelCase, 64-bit integers as
 * decimal strings, bytes as base64, enums by name, Timestamps as RFC 3339). Responses are
 * written with unpopulated fields emitted, so scalars and repeated fields are always present.
 * Underneath, the bytes move through a @connectrpc/connect Transport: connect-web over HTTP for
 * the http adapter, an in-process router for the fixture.
 */
import {
  fromJson,
  toJson,
  type DescMessage,
  type DescMethodServerStreaming,
  type DescMethodUnary,
  type MessageJsonType,
} from "@bufbuild/protobuf";
import { Code, ConnectError, type Transport as ConnectTransport } from "@connectrpc/connect";
import { createAsyncIterable } from "@connectrpc/connect/protocol";
import { ErrorDetailSchema } from "../gen/bee/v1/common_pb";
import { BeeError, type CallOptions, type ConnectCode } from "./transport";
import type { ErrorDetail } from "./types";

export type Json<D extends DescMessage> = MessageJsonType<D>;

/** JSON written for the UI: implicit fields present, enum names, bigint as strings. */
export const WRITE = { alwaysEmitImplicit: true } as const;
/** Newer servers may send fields this build does not know (SPEC §12.1). */
export const READ = { ignoreUnknownFields: true } as const;

export interface Rpc {
  readonly kind: "http" | "fixture";
  call<I extends DescMessage, O extends DescMessage>(method: DescMethodUnary<I, O>, req: Json<I>, opts?: CallOptions): Promise<Json<O>>;
  stream<I extends DescMessage, O extends DescMessage>(method: DescMethodServerStreaming<I, O>, req: Json<I>, opts?: CallOptions): AsyncIterable<Json<O>>;
}

const CODE_NAMES: Record<Code, ConnectCode> = {
  [Code.Canceled]: "canceled",
  [Code.Unknown]: "unknown",
  [Code.InvalidArgument]: "invalid_argument",
  [Code.DeadlineExceeded]: "deadline_exceeded",
  [Code.NotFound]: "not_found",
  [Code.AlreadyExists]: "already_exists",
  [Code.PermissionDenied]: "permission_denied",
  [Code.ResourceExhausted]: "resource_exhausted",
  [Code.FailedPrecondition]: "failed_precondition",
  [Code.Aborted]: "aborted",
  [Code.OutOfRange]: "out_of_range",
  [Code.Unimplemented]: "unimplemented",
  [Code.Internal]: "internal",
  [Code.Unavailable]: "unavailable",
  [Code.DataLoss]: "data_loss",
  [Code.Unauthenticated]: "unauthenticated",
};
const CODES_BY_NAME = new Map(Object.entries(CODE_NAMES).map(([k, v]) => [v, Number(k) as Code]));

/** Default reasons for codes that arrive without a detail (SPEC §12.2). */
const DEFAULT_REASON: Partial<Record<ConnectCode, string>> = {
  canceled: "CANCELLED",
  out_of_range: "INVALID_ARGUMENT",
  failed_precondition: "INVALID_STATE",
  aborted: "VERSION_MISMATCH",
  data_loss: "INTEGRITY",
  unknown: "INTERNAL",
};

/** A Connect error as the UI sees it: a BeeError carrying the bee.v1.ErrorDetail. */
export function toBeeError(err: unknown, signal?: AbortSignal): BeeError {
  if (err instanceof BeeError) return err;
  const ce = ConnectError.from(err);
  const code = CODE_NAMES[ce.code] ?? "unknown";
  if (signal?.aborted || code === "canceled") return new BeeError("canceled", { reason: "CANCELLED", message: "request canceled", retryable: true });
  const found = ce.findDetails(ErrorDetailSchema)[0];
  const detail = found ? (toJson(ErrorDetailSchema, found, WRITE) as unknown as ErrorDetail) : undefined;
  return new BeeError(code, {
    reason: detail?.reason || DEFAULT_REASON[code] || code.toUpperCase(),
    message: detail?.message || ce.rawMessage || "request failed",
    retryable: detail?.retryable ?? (code === "unavailable" || code === "resource_exhausted" || code === "deadline_exceeded"),
    leaseConflict: detail?.leaseConflict,
    policyDenial: detail?.policyDenial,
  });
}

/** The server side of the same mapping: a BeeError as a ConnectError with its detail. */
export function toConnectError(err: unknown): ConnectError {
  if (err instanceof ConnectError) return err;
  const e = err instanceof BeeError ? err : new BeeError("internal", { reason: "INTERNAL", message: (err as Error)?.message ?? "internal error" });
  const detail = fromJson(ErrorDetailSchema, JSON.parse(JSON.stringify(e.detail)), READ);
  return new ConnectError(e.message, CODES_BY_NAME.get(e.code) ?? Code.Unknown, undefined, [{ desc: ErrorDetailSchema, value: detail }]);
}

export function createRpc(transport: ConnectTransport, kind: Rpc["kind"]): Rpc {
  return {
    kind,
    async call(method, req, opts) {
      try {
        const res = await transport.unary(method, opts?.signal, undefined, undefined, fromJson(method.input, JSON.parse(JSON.stringify(req ?? {})) as never, READ));
        return toJson(method.output, res.message, WRITE) as never;
      } catch (err) {
        throw toBeeError(err, opts?.signal);
      }
    },
    async *stream(method, req, opts) {
      let res;
      try {
        res = await transport.stream(method, opts?.signal, undefined, undefined, createAsyncIterable([fromJson(method.input, JSON.parse(JSON.stringify(req ?? {})) as never, READ)]));
      } catch (err) {
        throw toBeeError(err, opts?.signal);
      }
      try {
        for await (const m of res.message) yield toJson(method.output, m, WRITE) as never;
      } catch (err) {
        if (opts?.signal?.aborted) return;
        throw toBeeError(err, opts?.signal);
      }
    },
  };
}
