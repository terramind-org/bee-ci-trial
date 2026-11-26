/** Objects, queries, writes and refs (vcs.proto); auth and import, which pass through as well. */
import { AuthService } from "../../gen/bee/v1/auth_pb";
import { ImportService } from "../../gen/bee/v1/import_pb";
import { ObjectService, QueryService, RefService, WriteService } from "../../gen/bee/v1/vcs_pb";
import type * as T from "../types";
import { RepoService } from "../../gen/bee/v1/forge_pb";
import { clean, diffLines, pass, passStream, str, wire, type PartialOps, type StreamOp, type UnaryOp } from "./shared";

export const vcsOps = {
  ObjectService: {
    ResolveRef: pass(ObjectService.method.resolveRef),
  },
  QueryService: {
    IsAncestor: pass(QueryService.method.isAncestor),
    GetChangesets: pass(QueryService.method.getChangesets),
    ReadPath: passStream(QueryService.method.readPath),
    ListTree: pass(QueryService.method.listTree),
    // History shows author, message and provenance, which live in Meta.
    Log: pass(QueryService.method.log, (r: T.LogRequest) => ({ ...r, includeMeta: true })),
    Blame: pass(QueryService.method.blame),
    CompareRevisions: pass(QueryService.method.compareRevisions),
    Diff: {
      wire: (r) => wire(QueryService.method.diff, r),
      run: async function* (rpc, r, opts) {
        for await (const item of rpc.stream(QueryService.method.diff, clean(r) as never, opts)) yield diffLines(item) as unknown as T.DiffItem;
      },
    } satisfies StreamOp<T.DiffRequest, T.DiffItem>,
  },
  WriteService: {
    Land: pass(WriteService.method.land),
  },
  RefService: {
    ListRefs: pass(RefService.method.listRefs),
    GetRefLog: pass(RefService.method.getRefLog),
  },
  AuthService: {
    Login: pass(AuthService.method.login),
    WhoAmI: pass(AuthService.method.whoAmI),
    MintToken: {
      wire: (r) => wire(AuthService.method.mintToken, r),
      run: async (rpc, r, opts) => {
        // Token scopes name repos by id (or org/*); resolve full names the form accepts.
        const repos = await Promise.all((r.scope?.repos ?? []).map(async (name) => (/^[\w.-]+\/[\w.-]+$/.test(name) ? str((await rpc.call(RepoService.method.getRepo, { repo: { fullName: name } }, opts)).id) || name : name)));
        return (await rpc.call(AuthService.method.mintToken, clean({ ...r, scope: r.scope && { ...r.scope, repos } }), opts)) as unknown as T.MintTokenResponse;
      },
    } satisfies UnaryOp<T.MintTokenRequest, T.MintTokenResponse>,
    Revoke: pass(AuthService.method.revoke),
  },
  ImportService: {
    StartImport: pass(ImportService.method.startImport),
    GetImport: pass(ImportService.method.getImport),
    ListImports: pass(ImportService.method.listImports),
    SetImportPhase: pass(ImportService.method.setImportPhase),
    CancelImport: pass(ImportService.method.cancelImport),
    GetFidelityReport: pass(ImportService.method.getFidelityReport),
  },
} satisfies PartialOps<"ObjectService" | "QueryService" | "WriteService" | "RefService" | "AuthService" | "ImportService">;
