/**
 * The typed client screens use. One function per UI method, grouped by service:
 *
 *   client.change.getChange({ repo, id })
 *   for await (const ev of client.event.subscribe({ repo })) { … }   // server-streaming
 *
 * Each call runs its mapping in `ops/` over generated bee.v1 descriptors and a Connect
 * transport (`rpc.ts`): connect-web for the http adapter, an in-process router for the fixture.
 */
import { base64ToBytes } from "./ids";
import type { MethodName, Methods, Req, Res, ServiceName, StreamingRoute } from "./methods";
import { STREAMING } from "./methods";
import { opOf } from "./ops";
import type { StreamOp, UnaryOp } from "./ops/shared";
import type { Rpc } from "./rpc";
import type { CallOptions } from "./transport";
import type { DiffItem, DiffRequest, ReadPathRequest } from "./types";

type ServiceKey<S extends string> = S extends `${infer Base}Service` ? Uncapitalize<Base> : never;

type ServiceClient<S extends ServiceName> = {
  [M in MethodName<S> as Uncapitalize<M>]: `${S}/${M}` extends StreamingRoute
    ? (req: Req<S, M>, opts?: CallOptions) => AsyncIterable<Res<S, M>>
    : (req: Req<S, M>, opts?: CallOptions) => Promise<Res<S, M>>;
};

export type BeeClient = { [S in ServiceName as ServiceKey<S>]: ServiceClient<S> } & {
  readonly rpc: Rpc;
  readonly kind: Rpc["kind"];
  /** Reads a whole file through the streaming ReadPath RPC and decodes it as UTF-8. */
  readText(req: ReadPathRequest, opts?: CallOptions): Promise<{ text: string; size: number; binary: boolean }>;
  /** Collects the streaming Diff RPC. */
  diffAll(req: DiffRequest, opts?: CallOptions): Promise<DiffItem[]>;
};

export const SERVICE_KEYS: Record<ServiceName, string> = {
  OrgService: "org",
  RepoService: "repo",
  ChangeService: "change",
  CheckService: "check",
  LeaseService: "lease",
  EventService: "event",
  ObjectService: "object",
  QueryService: "query",
  WriteService: "write",
  RefService: "ref",
  AuthService: "auth",
  ImportService: "import",
  AgentService: "agent",
  WorkflowService: "workflow",
  IssueService: "issue",
  ProjectService: "project",
  ReleaseService: "release",
  SecurityService: "security",
  PolicyService: "policy",
  AuditService: "audit",
  PackageService: "package",
  PagesService: "pages",
  SearchService: "search",
  TeamService: "team",
  AppService: "app",
  PostService: "post",
  BriefService: "brief",
  UserService: "user",
  InsightsService: "insights",
  BillingService: "billing",
  EnterpriseService: "enterprise",
  OpsService: "ops",
};

export function createClient(rpc: Rpc): BeeClient {
  const services: Record<string, unknown> = {};
  for (const [service, key] of Object.entries(SERVICE_KEYS) as [ServiceName, string][]) {
    services[key] = new Proxy(
      {},
      {
        get(_target, prop) {
          if (typeof prop !== "string") return undefined;
          const method = prop.charAt(0).toUpperCase() + prop.slice(1);
          const op = opOf(service, method as MethodName<typeof service>) as UnaryOp<unknown, unknown> | StreamOp<unknown, unknown>;
          return (req: unknown, opts?: CallOptions) => op.run(rpc, req, opts);
        },
      },
    );
  }

  const client = services as unknown as BeeClient; // boundary: Proxy-built object matches the mapped type.

  async function readText(req: ReadPathRequest, opts?: CallOptions) {
    const parts: Uint8Array[] = [];
    let size = 0;
    for await (const chunk of client.query.readPath(req, opts)) {
      parts.push(base64ToBytes(chunk.data));
      size = Number(chunk.size);
    }
    const all = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
    let o = 0;
    for (const p of parts) {
      all.set(p, o);
      o += p.length;
    }
    const binary = all.includes(0);
    return { text: binary ? "" : new TextDecoder().decode(all), size, binary };
  }

  async function diffAll(req: DiffRequest, opts?: CallOptions) {
    const out: DiffItem[] = [];
    for await (const item of client.query.diff(req, opts)) out.push(item);
    return out;
  }

  return Object.assign(client, { rpc, kind: rpc.kind, readText, diffAll });
}

/** Whether a UI method streams. */
export function isStreaming(service: ServiceName, method: string): boolean {
  return STREAMING.has(service + "/" + method);
}

export type { Methods };
