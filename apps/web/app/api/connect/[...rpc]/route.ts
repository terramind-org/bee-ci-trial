/**
 * POST /api/connect/bee.v1.<Service>/<Method>
 *
 * The browser's single entry point. The browser never holds the token: this route reads it from
 * the httpOnly session cookie and forwards the Connect request (unary or server-streaming, the
 * body and response streamed through untouched) to the bee server. With BEE_ADAPTER=fixture
 * it serves the demo world's Connect router instead.
 */
import { createConnectRouter } from "@connectrpc/connect";
import { createFetchHandler } from "@connectrpc/connect/protocol";
import { getFixtureStore, routesFor, type FixtureStore } from "@/lib/api/fixture";
import { READ } from "@/lib/api/rpc";
import { adapterKind, apiUrl, sessionToken } from "@/lib/api/server";
import { BeeError, httpStatusOf } from "@/lib/api/transport";

export const dynamic = "force-dynamic";

function errorResponse(e: BeeError, status = httpStatusOf(e.code)): Response {
  return Response.json(e.toJSON(), { status });
}

type FetchHandler = (req: Request) => Promise<Response>;
let mounted: { store: FixtureStore; handlers: Map<string, FetchHandler> } | null = null;

function fixtureHandlers(): Map<string, FetchHandler> {
  const store = getFixtureStore();
  if (mounted?.store !== store) {
    const router = createConnectRouter({ grpc: false, grpcWeb: false, connect: true, jsonOptions: READ });
    routesFor(store)(router);
    mounted = { store, handlers: new Map(router.handlers.map((h) => [h.requestPath, createFetchHandler(h)])) };
  }
  return mounted.handlers;
}

/** Request headers the bee server reads; everything else (cookies included) stays here. */
const FORWARD = ["content-type", "connect-protocol-version", "connect-timeout-ms", "connect-content-encoding", "connect-accept-encoding", "x-request-id", "bee-push-protection-bypass-id"];

export async function POST(req: Request, ctx: { params: Promise<{ rpc: string[] }> }): Promise<Response> {
  const { rpc } = await ctx.params;
  const path = "/" + rpc.join("/");
  if (!/^\/bee\.v1\.[A-Za-z]+\/[A-Za-z]+$/.test(path)) {
    return errorResponse(new BeeError("not_found", { reason: "NOT_FOUND", message: "expected /bee.v1.<Service>/<Method>" }));
  }
  const token = await sessionToken();

  if (adapterKind() === "http") {
    const upstream = apiUrl();
    const headers = new Headers();
    for (const h of FORWARD) {
      const v = req.headers.get(h);
      if (v) headers.set(h, v);
    }
    if (!headers.has("content-type")) headers.set("content-type", "application/json");
    if (token) headers.set("authorization", "Bearer " + token);
    const res = await fetch(upstream + path, { method: "POST", headers, body: await req.arrayBuffer(), signal: req.signal, cache: "no-store" }).catch((err: Error) => err);
    if (res instanceof Error) {
      if (req.signal.aborted) return new Response(null, { status: 499 });
      return errorResponse(new BeeError("unavailable", { reason: "UNAVAILABLE", message: "cannot reach the bee server at " + upstream + ": " + res.message, retryable: true }));
    }
    const out = new Headers();
    for (const [k, v] of res.headers) if (k === "content-type" || k.startsWith("connect-") || k.startsWith("trailer-") || k === "x-request-id") out.set(k, v);
    out.set("Cache-Control", "no-store");
    out.set("X-Accel-Buffering", "no");
    return new Response(res.body, { status: res.status, headers: out });
  }

  const handler = fixtureHandlers().get(path);
  // Unknown methods answer 404, which Connect clients read as `unimplemented` (SPEC §12.2).
  if (!handler) return errorResponse(new BeeError("unimplemented", { reason: "UNIMPLEMENTED", message: path.slice(1) + " is not served here" }), 404);
  const headers = new Headers(req.headers);
  if (token) headers.set("Authorization", "Bearer " + token);
  const res = await handler(new Request(req.url, { method: "POST", headers, body: await req.arrayBuffer(), signal: req.signal }));
  const out = new Headers(res.headers);
  out.set("Cache-Control", "no-store");
  out.set("X-Accel-Buffering", "no");
  return new Response(res.body, { status: res.status, headers: out });
}
