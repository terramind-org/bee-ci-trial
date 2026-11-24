/**
 * The fixture adapter: the in-memory FixtureStore served as bee.v1 Connect services, reached
 * through an in-process router transport with the JSON codec. Unit tests and the offline demo
 * (BEE_ADAPTER=fixture) use it; screens get exactly what the http adapter would give them.
 */
import { createRouterTransport, type ConnectRouter } from "@connectrpc/connect";
import { createRpc, READ, type Rpc } from "../rpc";
import { SERVICES } from "./handlers";
import { fixtureRoutes } from "./router";
import { FixtureStore } from "./store";

export { FixtureStore } from "./store";
export type { StoreOptions } from "./store";

/**
 * Connect routes for a store; mount them on any Connect server (Node, tests). `omit` leaves
 * services unserved (by type name), like a server that answers them UNIMPLEMENTED.
 */
export function routesFor(store: FixtureStore, opts: { omit?: string[] } = {}): (router: ConnectRouter) => void {
  const omit = new Set(opts.omit ?? []);
  return fixtureRoutes(store, SERVICES.filter((s) => !omit.has(s.desc.typeName)));
}

export function fixtureRpc(store: FixtureStore, token?: string): Rpc {
  const transport = createRouterTransport(routesFor(store), {
    transport: {
      useBinaryFormat: false,
      jsonOptions: READ,
      interceptors: [
        (next) => async (req) => {
          if (token) req.header.set("Authorization", "Bearer " + token);
          return next(req);
        },
      ],
    },
  });
  return createRpc(transport, "fixture");
}

declare global {
  var __beeFixture: FixtureStore | undefined;
}

/** One store per server process, shared by RSC rendering and the Connect route. */
export function getFixtureStore(): FixtureStore {
  if (!globalThis.__beeFixture) {
    const minute = 60_000;
    globalThis.__beeFixture = new FixtureStore({
      // Content is deterministic; timestamps are relative to process start so the demo reads "now".
      anchor: process.env.BEE_FIXTURE_ANCHOR ? Date.parse(process.env.BEE_FIXTURE_ANCHOR) : Math.floor(Date.now() / minute) * minute,
      live: process.env.BEE_FIXTURE_LIVE !== "0",
      tickMs: Number(process.env.BEE_FIXTURE_TICK_MS || 4000),
      logMs: Number(process.env.BEE_FIXTURE_LOG_MS || 400),
      importMs: Number(process.env.BEE_FIXTURE_IMPORT_MS || 8000),
    });
  }
  return globalThis.__beeFixture;
}
