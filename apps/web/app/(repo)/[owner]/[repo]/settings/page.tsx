import type { Metadata } from "next";
import Link from "next/link";
import { PageMenu } from "@/components/menu";
import { PolicyEditor } from "@/components/policy-editor";
import { PageHeader } from "@/components/ui";
import { pathBytes } from "@/lib/api/ids";
import { type RepoParams, scopeFromParams, serverClient } from "@/lib/api/server";
import { BeeError } from "@/lib/api/transport";
import type { Policy } from "@/lib/api/types";
import { guard } from "@/lib/page";
import { orgSettingsHref } from "@/lib/routes";

export const metadata: Metadata = { title: "Policy" };

/** SPEC §9.4: the repo policy file. */
const POLICY_PATH = ".beehive/policy.toml";

export default async function SettingsPage({ params }: { params: Promise<RepoParams> }) {
  return guard(async () => {
    const api = await serverClient();
    const { ref, fullName, owner } = await scopeFromParams(params);
    const req = { repo: ref };
    let policy: Policy;
    let unavailable: { reason: string; message: string } | undefined;
    try {
      policy = await api.policy.getPolicy(req);
    } catch (err) {
      if (!(err instanceof BeeError) || err.code !== "unimplemented") throw err;
      // No PolicyService here: show the file itself, read through QueryService like any reader.
      unavailable = { reason: err.reason, message: err.message };
      const repo = await api.repo.getRepo({ repo: ref });
      const file = await api.readText({ repo: ref, at: { expr: repo.defaultRef || "main" }, path: pathBytes(POLICY_PATH) }).catch((e: unknown) => {
        if (e instanceof BeeError && e.code === "not_found") return { text: "", size: 0, binary: false };
        throw e;
      });
      policy = { path: POLICY_PATH, source: file.text, epoch: "" };
    }
    return (
      <>
        <PageHeader
          title="Settings"
          lede={
            <>
              The rules for this repository live in the repository, as a file. Try a change in evaluate mode before you propose it. Members, teams and the audit log are in <Link href={orgSettingsHref(owner)} className="link">organization settings</Link>.
            </>
          }
          actions={<PageMenu op={{ service: "PolicyService", method: "GetPolicy", request: req, label: "Show the effective policy" }} />}
        />
        <PolicyEditor repo={fullName} source={policy.source} epoch={policy.epoch} path={policy.path} unavailable={unavailable} />
      </>
    );
  });
}
