import type { Metadata } from "next";
import { CopyAs, PageOp } from "@/components/copy-as";
import { PoliciesForm } from "@/components/admin/policies-form";
import { EmptyState, Panel } from "@/components/ui";
import { Failed, enterpriseSlug, serverClient, settle } from "../load";

export const metadata: Metadata = { title: "Admin · Policies" };
export const dynamic = "force-dynamic";

/** What members of every organization may do, plus the instance-wide anonymous read setting. */
export default async function PoliciesPage() {
  const api = await serverClient();
  const slug = enterpriseSlug();
  const policies = await settle(api.enterprise.getEnterprisePolicies({ enterprise: slug }));
  const op = { service: "EnterpriseService", method: "GetEnterprisePolicies", request: { enterprise: slug }, label: "Show the enterprise policies" } as const;
  return (
    <>
      <PageOp op={op} />
      <div className="grid grid-cols-1 gap-10 xl:grid-cols-[minmax(0,3fr)_minmax(0,1fr)]">
        <Panel title="Enterprise policies" id="pol-main" actions={<CopyAs compact op={op} />}>
          {policies.ok ? (
            <PoliciesForm enterprise={slug} policies={policies.value} />
          ) : policies.error.code === "not_found" ? (
            <EmptyState title="No enterprise account yet" icon="team">
              Policies belong to the enterprise account, and this server has none.
            </EmptyState>
          ) : (
            <Failed what="Enterprise policies" error={policies.error} />
          )}
        </Panel>
        <Panel title="Anonymous read" id="pol-anon">
          <div className="py-3 text-base text-ink-2">
            <p>
              Whether signed-out visitors can read public repositories is set by <span className="mono">[forge] allow_anonymous_public_read</span> in <span className="mono">server.toml</span> (it is on unless set to false). The server does not report it to the console, so change it there and restart.
            </p>
          </div>
        </Panel>
      </div>
    </>
  );
}
