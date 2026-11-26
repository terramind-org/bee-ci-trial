/**
 * Building blocks for the mapping layer: one `Op` per UI method turns a view-model request into
 * bee.v1 calls and the responses back into view models.
 */
import type { DescMessage, DescMethodServerStreaming, DescMethodUnary } from "@bufbuild/protobuf";
import type { MethodName, Req, Res, ServiceName, StreamingRoute } from "../methods";
import type { Json, Rpc } from "../rpc";
import { BeeError, type CallOptions } from "../transport";

/** The primary bee.v1 call behind a UI method, for Copy as (HTTP, MCP). */
export interface Wire {
  service: string;
  method: string;
  streaming: boolean;
  /** The proto3-JSON request body. */
  request: unknown;
}

export interface UnaryOp<Req, Res> {
  wire(req: Req): Wire;
  run(rpc: Rpc, req: Req, opts?: CallOptions): Promise<Res>;
}
export interface StreamOp<Req, Res> {
  wire(req: Req): Wire;
  run(rpc: Rpc, req: Req, opts?: CallOptions): AsyncIterable<Res>;
}

export type OpOf<S extends ServiceName, M extends MethodName<S>> = `${S}/${M}` extends StreamingRoute ? StreamOp<Req<S, M>, Res<S, M>> : UnaryOp<Req<S, M>, Res<S, M>>;
/** The mapping for every UI method of the services `K`. */
export type Ops<K extends ServiceName = ServiceName> = { [S in K]: { [M in MethodName<S>]: OpOf<S, M> } };

/** Some of the UI methods of the services `K`: a service's mappings may be spread over several files. */
export type PartialOps<K extends ServiceName = ServiceName> = { [S in K]: { [M in MethodName<S>]?: OpOf<S, M> } };

/** Drops `undefined` members so the object is valid proto3 JSON. */
export function clean<T>(v: T): T {
  return v === undefined ? v : (JSON.parse(JSON.stringify(v)) as T);
}

export function wire(method: DescMethodUnary | DescMethodServerStreaming, request: unknown): Wire {
  return { service: method.parent.name, method: method.name, streaming: method.methodKind === "server_streaming", request: clean(request) };
}

/**
 * A UI method that is exactly one bee.v1 call. The view types are a subset of the proto3-JSON
 * shapes (checked at compile time in `conformance.ts`), so the request and response pass through.
 */
export function pass<I extends DescMessage, O extends DescMessage, Req, Res>(method: DescMethodUnary<I, O>, toReq: (r: Req) => Json<I> = (r) => r as Json<I>): UnaryOp<Req, Res> {
  return {
    wire: (r) => wire(method, toReq(r)),
    run: async (rpc, r, opts) => (await rpc.call(method, clean(toReq(r)), opts)) as unknown as Res,
  };
}

/** The server-streaming twin of `pass`. */
export function passStream<I extends DescMessage, O extends DescMessage, Req, Res>(method: DescMethodServerStreaming<I, O>, toReq: (r: Req) => Json<I> = (r) => r as Json<I>): StreamOp<Req, Res> {
  return {
    wire: (r) => wire(method, toReq(r)),
    run: (rpc, r, opts) => rpc.stream(method, clean(toReq(r)), opts) as unknown as AsyncIterable<Res>,
  };
}

/** Collects a server stream. */
export async function collect<T>(it: AsyncIterable<T>): Promise<T[]> {
  const out: T[] = [];
  for await (const x of it) out.push(x);
  return out;
}

/**
 * Diff hunk lines are "+x", "-x" or " x"; some servers keep each line's own newline. Screens get
 * the line without it, so both forms render the same.
 */
export function diffLines<I extends { hunks?: { lines?: string[] }[] }>(item: I): I {
  if (!item.hunks?.some((h) => h.lines?.some((l) => l.endsWith("\n")))) return item;
  return { ...item, hunks: item.hunks.map((h) => ({ ...h, lines: (h.lines ?? []).map((l) => l.replace(/\r?\n$/, "")) })) };
}

export function pageOf(p: { nextPageToken?: string } | undefined): { nextPageToken: string } | undefined {
  return p ? { nextPageToken: p.nextPageToken ?? "" } : undefined;
}

export const num = (v: string | number | undefined | null): number => Number(v ?? 0) || 0;
export const str = (v: string | undefined | null): string => v ?? "";
export const list = <T>(v: T[] | undefined | null): T[] => v ?? [];

export function notFound(reason: string, message: string): BeeError {
  return new BeeError("not_found", { reason, message });
}

/** Runs `f` over items with at most `n` calls in flight; keeps order. */
export async function mapLimit<T, R>(items: T[], n: number, f: (x: T) => Promise<R>): Promise<R[]> {
  const out: R[] = new Array(items.length);
  let next = 0;
  await Promise.all(
    Array.from({ length: Math.min(n, items.length) }, async () => {
      while (next < items.length) {
        const i = next++;
        out[i] = await f(items[i] as T);
      }
    }),
  );
  return out;
}
