/**
 * The seed's toolbox: typed clients for every bee.v1 service over the server's Connect
 * endpoint (generated descriptors, @connectrpc/connect-node), and a changeset builder that
 * uploads blobs and creates changesets through WriteService like any bee client.
 */
import { createClient, type Client, type Interceptor, type Transport } from "@connectrpc/connect";
import { createConnectTransport } from "@connectrpc/connect-node";
import type { DescService } from "@bufbuild/protobuf";
import { AgentService } from "../../lib/gen/bee/v1/agents_pb";
import { AuditService } from "../../lib/gen/bee/v1/audit_pb";
import { AuthService } from "../../lib/gen/bee/v1/auth_pb";
import { BriefService } from "../../lib/gen/bee/v1/briefs_pb";
import type { ObjectId, Provenance } from "../../lib/gen/bee/v1/common_pb";
import { Mode } from "../../lib/gen/bee/v1/common_pb";
import { ChangeService, CheckService, EventService, LeaseService, OrgService, RepoService } from "../../lib/gen/bee/v1/forge_pb";
import { ImportService } from "../../lib/gen/bee/v1/import_pb";
import { IssueService } from "../../lib/gen/bee/v1/issues_pb";
import { PackageService } from "../../lib/gen/bee/v1/packages_pb";
import { PagesService } from "../../lib/gen/bee/v1/pages_pb";
import { PostService } from "../../lib/gen/bee/v1/posts_pb";
import { ProjectService } from "../../lib/gen/bee/v1/projects_pb";
import { ReleaseService } from "../../lib/gen/bee/v1/releases_pb";
import { SecurityService } from "../../lib/gen/bee/v1/security_pb";
import { TeamService } from "../../lib/gen/bee/v1/teams_pb";
import { AppService } from "../../lib/gen/bee/v1/apps_pb";
import { SearchService } from "../../lib/gen/bee/v1/search_pb";
import { UserService } from "../../lib/gen/bee/v1/users_pb";
import { ObjectService, QueryService, RefService, WriteService } from "../../lib/gen/bee/v1/vcs_pb";
import { RunnerService, WorkflowService } from "../../lib/gen/bee/v1/workflows_pb";
import { blake3 } from "./blake3";

const SERVICES = {
  agent: AgentService,
  audit: AuditService,
  auth: AuthService,
  brief: BriefService,
  change: ChangeService,
  check: CheckService,
  event: EventService,
  lease: LeaseService,
  org: OrgService,
  repo: RepoService,
  import: ImportService,
  issue: IssueService,
  package: PackageService,
  pages: PagesService,
  post: PostService,
  project: ProjectService,
  release: ReleaseService,
  security: SecurityService,
  team: TeamService,
  app: AppService,
  object: ObjectService,
  query: QueryService,
  ref: RefService,
  write: WriteService,
  runner: RunnerService,
  workflow: WorkflowService,
  search: SearchService,
  user: UserService,
} as const;

export type Api = { [K in keyof typeof SERVICES]: Client<(typeof SERVICES)[K]> } & { token: string; endpoint: string };

export function transportFor(endpoint: string, token: string): Transport {
  const auth: Interceptor = (next) => (req) => {
    req.header.set("authorization", "Bearer " + token);
    return next(req);
  };
  // HTTP/2 (cleartext, prior knowledge) so client-streaming UploadObjects works too.
  return createConnectTransport({ baseUrl: endpoint, httpVersion: "2", useBinaryFormat: false, interceptors: [auth] });
}

export function connect(endpoint: string, token: string): Api {
  const transport = transportFor(endpoint, token);
  const api: Record<string, unknown> = { token, endpoint };
  for (const [k, svc] of Object.entries(SERVICES)) api[k] = createClient(svc as DescService, transport);
  return api as Api;
}

export const repoRef = (fullName: string) => ({ repo: { case: "fullName" as const, value: fullName } });
export const enc = (s: string) => new TextEncoder().encode(s);

/** Deterministic 16 bytes from a label (ChangeIds, prompt digests): the seed is reproducible. */
export function digest16(label: string): Uint8Array {
  return blake3(enc(label)).subarray(0, 16);
}

export function blobId(data: Uint8Array): Uint8Array {
  const id = new Uint8Array(34);
  id[0] = 0x01; // BLAKE3-256
  id[1] = 0x01; // blob
  id.set(blake3(data), 2);
  return id;
}

export const hex = (b: Uint8Array) => Array.from(b, (x) => x.toString(16).padStart(2, "0")).join("");
/** SPEC §3.1 text form, e.g. c1_9f3a… */
export const idText = (id: ObjectId | undefined) => (id ? "?btcm"[id.raw[1] ?? 0] + "1_" + hex(id.raw.subarray(2)) : "");

export interface Commit {
  files: Record<string, string | null>; // null deletes
  message: string;
  author: string; // "Name <email>"
  provenance?: Partial<Pick<Provenance, "runtime" | "model" | "session">> & { inputTokens?: number; outputTokens?: number; usdMicros?: number; checks?: [string, string, number][] };
  /** Seconds since the epoch for the author time; the seed spaces its history out. */
  time?: number;
  /** Extra request headers, e.g. a push-protection bypass reason. */
  headers?: Record<string, string>;
}

/** Uploads the blobs a commit needs and creates its changeset on top of `parent`. */
export async function commit(api: Api, repo: string, parent: ObjectId | undefined, c: Commit, label: string): Promise<ObjectId> {
  const puts = Object.entries(c.files).filter(([, v]) => v !== null) as [string, string][];
  const blobs = puts.map(([, text]) => enc(text));
  if (blobs.length) {
    await api.write.uploadObjects(
      (async function* () {
        for (let i = 0; i < blobs.length; i++) {
          const data = blobs[i] as Uint8Array;
          yield { repo: i === 0 ? repoRef(repo) : undefined, id: { raw: blobId(data) }, offset: 0n, data, last: true };
        }
      })(),
    );
  }
  const delta = Object.entries(c.files).map(([path, text]) =>
    text === null
      ? { path: enc(path), op: { case: "delete" as const, value: true } }
      : { path: enc(path), op: { case: "put" as const, value: { mode: Mode.FILE, blob: { raw: blobId(enc(text)) }, size: BigInt(enc(text).length) } } },
  );
  const p = c.provenance;
  const time = c.time ? { name: c.author, time: { seconds: BigInt(c.time), nanos: 0 }, tzOffsetMinutes: 0 } : undefined;
  const res = await api.write.createChangeset({
    repo: repoRef(repo),
    parents: parent ? [parent] : [],
    delta,
    changeId: { raw: digest16("change:" + label) },
    author: c.author,
    authorTime: time,
    message: c.message,
    provenance: p
      ? {
          runtime: p.runtime ?? "",
          model: p.model ?? "",
          session: p.session ?? "",
          promptDigest: blake3(enc("prompt:" + label)),
          checksRun: (p.checks ?? []).map(([name, result, ms]) => ({ name, result, durationMs: BigInt(ms) })),
          cost: { inputTokens: BigInt(p.inputTokens ?? 0), outputTokens: BigInt(p.outputTokens ?? 0), usdMicros: BigInt(p.usdMicros ?? 0) },
        }
      : undefined,
  }, { headers: c.headers });
  const id = res.changeset?.id;
  if (!id) throw new Error("CreateChangeset returned no id for " + label);
  return id;
}

export async function tip(api: Api, repo: string, ref = "main"): Promise<ObjectId | undefined> {
  return (await api.object.resolveRef({ repo: repoRef(repo), name: ref })).ref?.target;
}

export function log(...parts: unknown[]): void {
  console.log("[seed]", ...parts);
}
