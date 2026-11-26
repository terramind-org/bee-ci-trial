/**
 * The browser's client: connect-web to our own route, which proxies or serves the fixture.
 *
 * Loaded on the first call, not with the page. The client carries the generated descriptors of
 * every bee.v1 service (about 200 KB of gzipped script), and the shell imports it on every
 * page (command palette, live refresh), so a static import put it in front of each page's first
 * paint: Lighthouse charged it to the change request's title (LCP). Every browser caller only
 * calls service methods, which already return a promise or an async iterable, so waiting for the
 * module first changes nothing they can see.
 */
import type { BeeClient } from "./client";
import { STREAMING } from "./methods";

/** What the browser can call: every service method, plus the whole-file and whole-diff helpers. */
export type BrowserClient = Omit<BeeClient, "rpc" | "kind">;

let loaded: Promise<BeeClient> | null = null;

function load(): Promise<BeeClient> {
  loaded ??= Promise.all([import("./client"), import("./config"), import("./http")]).then(([c, cfg, h]) => c.createClient(h.httpRpc({ baseUrl: cfg.BROWSER_BASE })));
  // A failed chunk load (offline, a deploy in between) is retried on the next call.
  loaded.catch(() => (loaded = null));
  return loaded;
}

type Call = (req: unknown, opts?: unknown) => unknown;
const methodOf = (client: BeeClient, key: string, method: string) => (client as unknown as Record<string, Record<string, Call>>)[key]![method]!;

function lazyStream(key: string, method: string, req: unknown, opts: unknown): AsyncIterable<unknown> {
  return {
    async *[Symbol.asyncIterator]() {
      const client = await load();
      yield* methodOf(client, key, method)(req, opts) as AsyncIterable<unknown>;
    },
  };
}

const services = new Map<string, Record<string, Call>>();

function service(key: string): Record<string, Call> {
  const known = services.get(key);
  if (known) return known;
  // SERVICE_KEYS are each service's name without "Service", uncapitalised ("change" → ChangeService).
  const name = key.charAt(0).toUpperCase() + key.slice(1) + "Service";
  const proxy = new Proxy({} as Record<string, Call>, {
    get(_target, prop) {
      if (typeof prop !== "string") return undefined;
      const method = prop.charAt(0).toUpperCase() + prop.slice(1);
      if (STREAMING.has(name + "/" + method)) return (req: unknown, opts?: unknown) => lazyStream(key, prop, req, opts);
      return (req: unknown, opts?: unknown) => load().then((client) => methodOf(client, key, prop)(req, opts));
    },
  });
  services.set(key, proxy);
  return proxy;
}

const helpers = {
  readText: (...args: Parameters<BeeClient["readText"]>) => load().then((c) => c.readText(...args)),
  diffAll: (...args: Parameters<BeeClient["diffAll"]>) => load().then((c) => c.diffAll(...args)),
};

let cached: BrowserClient | null = null;

export function browserClient(): BrowserClient {
  cached ??= new Proxy(helpers as unknown as BrowserClient, {
    get(target, prop) {
      if (prop === "readText" || prop === "diffAll") return target[prop];
      return typeof prop === "string" ? service(prop) : undefined;
    },
  }); // boundary: the Proxy answers every service key with a service whose methods match the mapped type.
  return cached;
}
