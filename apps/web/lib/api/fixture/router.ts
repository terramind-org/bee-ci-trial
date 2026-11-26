/**
 * The fixture as a Connect server: handlers written against the generated `*Json` types,
 * registered on a ConnectRouter for the generated service descriptors. Requests arrive as
 * messages, are written to proto3 JSON (implicit fields included) for the handler, and the
 * handler's JSON is read back strictly into the output message, so a fixture field the proto
 * does not have fails loudly instead of leaking to the UI.
 */
import { fromJson, toJson, type DescMessage, type DescMethod, type DescMethodServerStreaming, type DescMethodUnary, type DescService, type MessageJsonType } from "@bufbuild/protobuf";
import type { ConnectRouter, HandlerContext } from "@connectrpc/connect";
import { toConnectError, WRITE } from "../rpc";
import { clean } from "../ops/shared";
import { gate } from "./anonymous";
import type { FixtureStore } from "./store";

export interface Ctx {
  token?: string;
  signal?: AbortSignal;
}

type Handler<M> =
  M extends DescMethodUnary<infer I, infer O>
    ? (s: FixtureStore, req: MessageJsonType<I>, ctx: Ctx) => MessageJsonType<O> | Promise<MessageJsonType<O>>
    : M extends DescMethodServerStreaming<infer I, infer O>
      ? (s: FixtureStore, req: MessageJsonType<I>, ctx: Ctx) => AsyncIterable<MessageJsonType<O>>
      : never;

export type Impl<S extends DescService> = { [K in keyof S["method"]]?: Handler<S["method"][K]> };

export interface ServiceDef {
  desc: DescService;
  impl: Record<string, ((s: FixtureStore, req: never, ctx: Ctx) => unknown) | undefined>;
}

export function defineService<S extends DescService>(desc: S, impl: Impl<S>): ServiceDef {
  return { desc, impl: impl as ServiceDef["impl"] };
}

function tokenOf(ctx: HandlerContext): string | undefined {
  return ctx.requestHeader.get("authorization")?.replace(/^Bearer\s+/i, "") || undefined;
}

const read = <D extends DescMessage>(desc: D, json: unknown) => fromJson(desc, clean(json ?? {}) as never);

/** Registers every defined handler; RPCs without one answer `unimplemented`. */
export function fixtureRoutes(store: FixtureStore, services: ServiceDef[]): (router: ConnectRouter) => void {
  return (router) => {
    for (const { desc, impl } of services) {
      const methods: Record<string, unknown> = {};
      for (const m of desc.methods as DescMethod[]) {
        const h = impl[m.localName];
        if (!h) continue;
        if (m.methodKind === "unary") {
          methods[m.localName] = async (req: never, ctx: HandlerContext) => {
            try {
              const json = toJson(m.input, req, WRITE);
              gate(store, `/${desc.typeName}/${m.name}`, json, tokenOf(ctx));
              const out = await h(store, json as never, { token: tokenOf(ctx), signal: ctx.signal });
              return read(m.output, out);
            } catch (err) {
              throw toConnectError(err);
            }
          };
        } else if (m.methodKind === "server_streaming") {
          methods[m.localName] = async function* (req: never, ctx: HandlerContext) {
            try {
              const json = toJson(m.input, req, WRITE);
              gate(store, `/${desc.typeName}/${m.name}`, json, tokenOf(ctx));
              const it = h(store, json as never, { token: tokenOf(ctx), signal: ctx.signal }) as AsyncIterable<unknown>;
              for await (const out of it) yield read(m.output, out);
            } catch (err) {
              if (ctx.signal.aborted) return;
              throw toConnectError(err);
            }
          };
        }
      }
      router.service(desc, methods as never);
    }
  };
}
