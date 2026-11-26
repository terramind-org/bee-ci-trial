/** The http adapter: @connectrpc/connect-web with the Connect protocol and the JSON codec. */
import { Code, ConnectError, type Interceptor } from "@connectrpc/connect";
import { createConnectTransport } from "@connectrpc/connect-web";
import { READ, createRpc, type Rpc } from "./rpc";

export interface HttpOptions {
  /** Base URL, e.g. "https://bee.example.com" or "/api/connect" in the browser. */
  baseUrl: string;
  token?: string;
  fetch?: typeof fetch;
  headers?: Record<string, string>;
}

export function httpRpc(opts: HttpOptions): Rpc {
  const auth: Interceptor = (next) => async (req) => {
    if (opts.token) req.header.set("Authorization", "Bearer " + opts.token);
    for (const [k, v] of Object.entries(opts.headers ?? {})) req.header.set(k, v);
    try {
      return await next(req);
    } catch (err) {
      // fetch rejects with a TypeError when the server cannot be reached at all.
      if (err instanceof TypeError) throw new ConnectError("cannot reach the bee server: " + err.message, Code.Unavailable, undefined, undefined, err);
      throw err;
    }
  };
  const doFetch = opts.fetch ?? ((input: RequestInfo | URL, init?: RequestInit) => globalThis.fetch(input, init));
  const transport = createConnectTransport({
    baseUrl: opts.baseUrl.replace(/\/+$/, ""),
    useBinaryFormat: false,
    jsonOptions: READ,
    interceptors: [auth],
    fetch: ((input: RequestInfo | URL, init?: RequestInit) => doFetch(input, { ...init, cache: "no-store" })) as typeof fetch,
  });
  return createRpc(transport, "http");
}
