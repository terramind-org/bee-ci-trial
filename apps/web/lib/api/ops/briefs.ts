/**
 * Briefs and evidence (briefs.proto), plus the three calls the change page's conversation and
 * team need: the change request's comments (ChangeService), commenting on it, and the agent
 * sessions behind it (AgentService).
 *
 * Every RPC is one bee.v1 call. Evidence responses are reshaped in one place: each asset's
 * signed `/_evidence/…` URL becomes a same-origin `/api/evidence/…` URL (`lib/media/evidence-url.ts`),
 * so no screen ever holds an address on the bee server.
 */
import type { EvidenceAssetJson, EvidenceJson } from "../../gen/bee/v1/briefs_pb";
import { BriefService } from "../../gen/bee/v1/briefs_pb";
import { AgentService } from "../../gen/bee/v1/agents_pb";
import { ChangeService } from "../../gen/bee/v1/forge_pb";
import { evidenceUrl } from "../../media/evidence-url";
import type * as T from "../types";
import { clean, list, pass, str, wire, type PartialOps, type UnaryOp } from "./shared";

function assetView(a: EvidenceAssetJson): T.EvidenceAsset {
  return { ...(a as T.EvidenceAsset), url: evidenceUrl(a.url, a.digest) };
}

/** An Evidence as screens read it: same-origin asset URLs, everything else as sent. */
export function evidenceView(e: EvidenceJson): T.Evidence {
  return { ...(e as T.Evidence), assets: list(e.assets).map(assetView) };
}

/** A pass-through call whose response is an Evidence. */
function evidenceOp<Req>(method: Parameters<typeof wire>[0], toReq: (r: Req) => unknown = (r) => r): UnaryOp<Req, T.Evidence> {
  return {
    wire: (r) => wire(method, toReq(r)),
    run: async (rpc, r, opts) => evidenceView((await rpc.call(method as never, clean(toReq(r)) as never, opts)) as EvidenceJson),
  };
}

export const briefOps = {
  BriefService: {
    SetChangeBrief: pass(BriefService.method.setChangeBrief),
    GetBriefChecklist: pass(BriefService.method.getBriefChecklist),
    AddEvidence: evidenceOp<T.AddEvidenceRequest>(BriefService.method.addEvidence),
    BeginEvidenceUpload: {
      wire: (r) => wire(BriefService.method.beginEvidenceUpload, r),
      run: async (rpc, r, opts) => {
        const res = await rpc.call(BriefService.method.beginEvidenceUpload, clean(r) as never, opts);
        // The PUT goes through the same-origin route too, so the browser never addresses the bee server.
        const uploads = list(res.uploads).map((u) => ({ ...(u as T.EvidenceUploadTarget), uploadUrl: evidenceUrl(u.uploadUrl) }));
        return { ...(res as unknown as T.BeginEvidenceUploadResponse), evidence: res.evidence ? evidenceView(res.evidence) : undefined, uploads };
      },
    } satisfies UnaryOp<T.BeginEvidenceUploadRequest, T.BeginEvidenceUploadResponse>,
    FinalizeEvidence: evidenceOp<T.FinalizeEvidenceRequest>(BriefService.method.finalizeEvidence),
    GetEvidence: evidenceOp<T.GetEvidenceRequest>(BriefService.method.getEvidence),
    ListEvidence: {
      wire: (r) => wire(BriefService.method.listEvidence, r),
      run: async (rpc, r, opts) => {
        const res = await rpc.call(BriefService.method.listEvidence, clean(r) as never, opts);
        return { evidence: list(res.evidence).map(evidenceView), page: res.page ? { nextPageToken: str(res.page.nextPageToken) } : undefined };
      },
    } satisfies UnaryOp<T.ListEvidenceRequest, T.ListEvidenceResponse>,
    UpdateEvidence: evidenceOp<T.UpdateEvidenceRequest>(BriefService.method.updateEvidence),
    DeleteEvidence: pass(BriefService.method.deleteEvidence),
  },
  ChangeService: {
    ListComments: pass(ChangeService.method.listChangeComments),
    CommentChange: pass(ChangeService.method.createChangeComment),
  },
  AgentService: {
    GetSessionsForChange: pass(AgentService.method.getSessionsForChange),
  },
} satisfies PartialOps<"BriefService" | "ChangeService" | "AgentService">;
