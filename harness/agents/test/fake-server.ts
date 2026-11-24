// A tiny in-process bee server for the self-tests: a handful of Connect-shaped HTTP/JSON
// routes over in-memory state. It implements just enough of SPEC §12 (routes, envelopes,
// ErrorDetail) and of the write path (verified blob uploads, changesets, land, log, read) to
// prove the harness end to end. It is not a reference implementation.

import { createHash } from "node:crypto";
import { blobId } from "../src/blake3";
import { encodeEnvelopes, unframe } from "../src/connect";

type Json = Record<string, any>;

interface Cs {
  id: string; // base64 raw
  parent?: string;
  files: Map<string, string>; // path -> base64 content
  delta: Json[];
  changeId: Json;
  meta: Json;
}

interface Principal {
  id: string;
  kind: string;
  display: string;
}

class RpcFail extends Error {
  constructor(
    readonly code: string,
    readonly http: number,
    readonly reason: string,
    message: string,
  ) {
    super(message);
  }
}

const fail = (code: string, http: number, reason: string, msg: string) => new RpcFail(code, http, reason, msg);

export interface FakeServer {
  url: string;
  adminToken: string;
  calls: string[];
  stop(): void;
}

let counter = 0;
const ulid = () => `01J${(++counter).toString().padStart(23, "0")}`;

export function startFakeServer(): FakeServer {
  const adminToken = "admin-secret";
  const tokens = new Map<string, { principal: Principal; session?: string }>();
  const admin: Principal = { id: ulid(), kind: "PRINCIPAL_KIND_HUMAN", display: "admin" };
  tokens.set(adminToken, { principal: admin });
  const orgs = new Map<string, Json>();
  const repos = new Map<string, Json & { refs: Map<string, { target: string; version: number }> }>();
  const blobs = new Map<string, string>(); // base64 id -> base64 data
  const changesets = new Map<string, Cs>();
  const calls: string[] = [];

  const repoOf = (ref: Json | undefined) => {
    const r = ref?.fullName ? repos.get(ref.fullName) : [...repos.values()].find((x) => x.id === ref?.id);
    if (!r) throw fail("not_found", 404, "NOT_FOUND", "no such repo");
    return r;
  };
  const csId = (seed: string) => Buffer.concat([Buffer.from([1, 3]), createHash("sha256").update(seed).digest()]).toString("base64");

  function newRepo(org: string, name: string): Json {
    const root: Cs = { id: csId(`root ${org}/${name}`), files: new Map(), delta: [], changeId: { raw: Buffer.alloc(16).toString("base64") }, meta: { message: "root" } };
    changesets.set(root.id, root);
    const repo = { id: ulid(), name, fullName: `${org}/${name}`, defaultRef: "main", refs: new Map([["main", { target: root.id, version: 1 }]]) };
    repos.set(repo.fullName, repo);
    return repo;
  }

  const unary: Record<string, (b: Json, who: { principal: Principal; session?: string }) => Json> = {
    "OrgService/CreateOrg": (b) => {
      if (orgs.has(b.name)) throw fail("already_exists", 409, "ALREADY_EXISTS", "org exists");
      const o = { id: ulid(), name: b.name, display: b.display };
      orgs.set(b.name, o);
      return o;
    },
    "RepoService/CreateRepo": (b) => {
      if (!orgs.has(b.org)) throw fail("not_found", 404, "NOT_FOUND", "no such org");
      const { refs: _, ...r } = newRepo(b.org, b.name) as Json;
      return r;
    },
    "AuthService/CreateAgent": (b, who) => ({ id: ulid(), kind: "PRINCIPAL_KIND_AGENT", display: b.display, ownerId: who.principal.id, runtime: b.runtime }),
    "AuthService/MintToken": (b) => {
      const token = `tok-${ulid()}`;
      tokens.set(token, { principal: { id: b.principalId, kind: "PRINCIPAL_KIND_AGENT", display: b.principalId }, session: b.scope?.session });
      return { token, tokenId: token.slice(4) };
    },
    "OrgService/SetMember": () => ({}),
    "RepoService/AddCollaborator": () => ({}),
    "AuthService/WhoAmI": (_b, who) => ({ principal: who.principal }),
    "ObjectService/ResolveRef": (b) => {
      const repo = repoOf(b.repo);
      const ref = repo.refs.get(b.name);
      if (!ref) throw fail("not_found", 404, "NOT_FOUND", "no such ref");
      return { ref: { name: b.name, target: { raw: ref.target }, version: String(ref.version) } };
    },
    "WriteService/CreateChangeset": (b, who) => {
      repoOf(b.repo);
      const parent = changesets.get(b.parents?.[0]?.raw);
      if (!parent) throw fail("failed_precondition", 400, "MISSING_OBJECT", "no parent");
      const files = new Map(parent.files);
      for (const d of b.delta ?? []) {
        const path = Buffer.from(d.path, "base64").toString();
        if (d.delete) files.delete(path);
        else {
          const data = blobs.get(d.put.blob.raw);
          if (data === undefined) throw fail("failed_precondition", 400, "MISSING_OBJECT", "blob not uploaded");
          files.set(path, data);
        }
      }
      const id = csId(JSON.stringify([parent.id, b.delta, b.message, b.changeId]));
      const cs: Cs = {
        id,
        parent: parent.id,
        files,
        delta: b.delta ?? [],
        changeId: b.changeId,
        meta: { message: b.message, author: b.author, creator: who.principal.id, provenance: { ...(b.provenance ?? {}), session: who.session ?? b.provenance?.session ?? "" } },
      };
      changesets.set(id, cs);
      return { changeset: view(cs, false) };
    },
    "RefService/CreateRef": (b, who) => {
      // Like the real policy: an agent holds no role granting manage_refs.
      if (who.principal.kind === "PRINCIPAL_KIND_AGENT") throw fail("permission_denied", 403, "POLICY_DENIED", `${who.principal.id} holds no role granting \`manage_refs\``);
      const repo = repoOf(b.repo);
      if (repo.refs.has(b.name)) throw fail("already_exists", 409, "ALREADY_EXISTS", "ref exists");
      repo.refs.set(b.name, { target: b.target.raw, version: 1 });
      return { name: b.name, target: b.target, version: "1" };
    },
    "WriteService/Land": (b) => {
      const repo = repoOf(b.repo);
      const ref = repo.refs.get(b.ref);
      const head = changesets.get(b.head?.raw);
      if (!ref || !head) throw fail("not_found", 404, "NOT_FOUND", "no such ref or head");
      if (head.parent !== ref.target) throw fail("failed_precondition", 400, "MERGE_CONFLICT", "fake server only fast-forwards");
      ref.target = head.id;
      ref.version += 1;
      return { ref: { name: b.ref, target: { raw: head.id }, version: String(ref.version) }, landed: [{ changeId: head.changeId, source: { raw: head.id }, result: { raw: head.id } }], attempts: 1 };
    },
    "QueryService/Log": (b) => {
      const repo = repoOf(b.repo);
      let cur = changesets.get(repo.refs.get(b.from?.expr ?? "main")?.target ?? "");
      const out: Json[] = [];
      const max = b.page?.pageSize ?? 50;
      while (cur && cur.parent && out.length < max) {
        out.push(view(cur, Boolean(b.includeMeta)));
        cur = changesets.get(cur.parent);
      }
      return { changesets: out };
    },
  };

  function view(cs: Cs, meta: boolean): Json {
    return { id: { raw: cs.id }, parents: cs.parent ? [{ raw: cs.parent }] : [], changeId: cs.changeId, delta: cs.delta, ...(meta ? { meta: cs.meta } : {}) };
  }

  function readPath(b: Json): Json[] {
    const repo = repoOf(b.repo);
    const cs = changesets.get(repo.refs.get(b.at?.expr ?? "main")?.target ?? "");
    const data = cs?.files.get(Buffer.from(b.path ?? "", "base64").toString());
    if (data === undefined) throw fail("not_found", 404, "NOT_FOUND", "no such path");
    return [{ mode: "MODE_FILE", data, size: String(Buffer.from(data, "base64").length), last: true }];
  }

  function upload(items: Json[]): Json {
    const stored: Json[] = [];
    for (const it of items) {
      const bytes = Buffer.from(it.data ?? "", "base64");
      const want = Buffer.from(blobId(bytes)).toString("base64");
      if (want !== it.id?.raw) throw fail("data_loss", 500, "INTEGRITY", "bytes do not match id");
      blobs.set(want, it.data ?? "");
      stored.push({ raw: want });
    }
    return { stored };
  }

  function errorBody(e: RpcFail): Json {
    return { code: e.code, message: e.message, details: [{ type: "bee.v1.ErrorDetail", value: "", debug: { reason: e.reason, message: e.message, retryable: false } }] };
  }

  const server = Bun.serve({
    port: 0,
    async fetch(req) {
      const url = new URL(req.url);
      const m = /^\/bee\.v1\.(\w+)\/(\w+)$/.exec(url.pathname);
      if (req.method !== "POST" || !m) return new Response("not found", { status: 404 });
      const key = `${m[1]}/${m[2]}`;
      calls.push(key);
      const streaming = (req.headers.get("content-type") ?? "").startsWith("application/connect+json");
      const raw = new Uint8Array(await req.arrayBuffer());
      try {
        const auth = req.headers.get("authorization")?.replace(/^Bearer /, "") ?? "";
        const who = tokens.get(auth);
        if (!who) throw fail("unauthenticated", 401, "UNAUTHENTICATED", "missing or unknown token");
        if (streaming) {
          // Like the real server: an end-stream frame is not allowed in a request.
          if (unframe(raw).some((e) => e.flags & 2)) throw fail("invalid_argument", 400, "INVALID_ARGUMENT", "end-stream frames are not allowed in requests");
          const msgs = unframe(raw)
            .filter((e) => !(e.flags & 2))
            .map((e) => JSON.parse(new TextDecoder().decode(e.payload)) as Json);
          if (key === "WriteService/UploadObjects") {
            return new Response(encodeEnvelopes([upload(msgs)], {}), { headers: { "content-type": "application/connect+json" } });
          }
          if (key === "QueryService/ReadPath") {
            try {
              return new Response(encodeEnvelopes(readPath(msgs[0] ?? {}), {}), { headers: { "content-type": "application/connect+json" } });
            } catch (e) {
              if (!(e instanceof RpcFail)) throw e;
              return new Response(encodeEnvelopes([], { error: errorBody(e) }), { headers: { "content-type": "application/connect+json" } });
            }
          }
          throw fail("unimplemented", 501, "UNIMPLEMENTED", `${key} is not served here`);
        }
        const handler = unary[key];
        if (!handler) throw fail("unimplemented", 501, "UNIMPLEMENTED", `${key} is not served here`);
        const body = raw.length ? (JSON.parse(new TextDecoder().decode(raw)) as Json) : {};
        return Response.json(handler(body, who));
      } catch (e) {
        if (e instanceof RpcFail) return Response.json(errorBody(e), { status: e.http });
        return Response.json({ code: "internal", message: String(e) }, { status: 500 });
      }
    },
  });
  return { url: `http://127.0.0.1:${server.port}`, adminToken, calls, stop: () => server.stop(true) };
}
