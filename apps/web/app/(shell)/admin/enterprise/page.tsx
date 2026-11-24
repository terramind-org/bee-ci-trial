import type { Metadata } from "next";
import { CopyAs, PageOp } from "@/components/copy-as";
import { EnterpriseForm } from "@/components/admin/enterprise-form";
import { PrincipalName } from "@/components/domain";
import { Time } from "@/components/time";
import { Badge, EmptyState, Panel } from "@/components/ui";
import { Failed, enterpriseSlug, serverClient, settle } from "../load";

export const metadata: Metadata = { title: "Admin · Enterprise" };
export const dynamic = "force-dynamic";

/** The enterprise account: its details, the organizations in it and the people who own it. */
export default async function EnterprisePage() {
  const api = await serverClient();
  const slug = enterpriseSlug();
  const ent = await settle(api.enterprise.getEnterprise({ enterprise: slug }));

  if (!ent.ok && ent.error.code === "not_found") {
    const orgs = await settle(api.org.listOrgs({}));
    return (
      <Panel title="Organizations on this server" id="server-orgs">
        <EmptyState title="No enterprise account yet" icon="team">
          This server has no enterprise account, so there is nothing to edit here. Identity and security settings apply to each organization instead.
        </EmptyState>
        {orgs.ok && orgs.value.orgs.length ? (
          <ul aria-label="Organizations" className="divide-y divide-line border-t border-line">
            {orgs.value.orgs.map((o) => (
              <li key={o.id} className="flex items-center gap-3 py-3">
                <span className="text-base font-medium">{o.display || o.name}</span>
                <span className="text-sm text-ink-3">{o.name}</span>
              </li>
            ))}
          </ul>
        ) : null}
      </Panel>
    );
  }
  if (!ent.ok) return <Failed what="The enterprise" error={ent.error} />;

  const e = ent.value;
  const [orgs, owners] = await Promise.all([settle(api.enterprise.listEnterpriseOrgs({ enterprise: e.slug })), settle(api.enterprise.listEnterpriseOwners({ enterprise: e.slug }))]);
  const op = { service: "EnterpriseService", method: "GetEnterprise", request: { enterprise: slug }, label: "Show the enterprise" } as const;
  return (
    <>
      <PageOp op={op} />
      <div className="grid grid-cols-1 gap-10 xl:grid-cols-[minmax(0,2fr)_minmax(0,3fr)]">
        <Panel title="Details" id="ent-details" actions={<CopyAs compact op={op} />}>
          <EnterpriseForm enterprise={e} />
          <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1.5 border-t border-line py-3 text-base">
            <dt className="text-ink-3">Organizations</dt>
            <dd>{e.orgCount}</dd>
            <dt className="text-ink-3">Members</dt>
            <dd>{e.memberCount}</dd>
            {e.createdAt ? (
              <>
                <dt className="text-ink-3">Created</dt>
                <dd>
                  <Time iso={e.createdAt} />
                </dd>
              </>
            ) : null}
          </dl>
        </Panel>
        <div className="flex flex-col gap-10">
          <Panel title="Organizations" id="ent-orgs" actions={<CopyAs compact op={{ service: "EnterpriseService", method: "ListEnterpriseOrgs", request: { enterprise: e.slug }, label: "List the organizations" }} />}>
            {orgs.ok ? (
              orgs.value.orgs.length ? (
                <ul aria-label="Organizations" className="divide-y divide-line">
                  {orgs.value.orgs.map((o) => (
                    <li key={o.orgId} className="flex flex-wrap items-center gap-x-3 py-3">
                      <span className="text-base font-medium">{o.org}</span>
                      <Badge>{o.memberCount} members</Badge>
                      <Badge>{o.repoCount} repositories</Badge>
                      {o.joinedAt ? (
                        <span className="ml-auto text-sm text-ink-3">
                          joined <Time iso={o.joinedAt} />
                        </span>
                      ) : null}
                    </li>
                  ))}
                </ul>
              ) : (
                <EmptyState title="No organizations" icon="team" />
              )
            ) : (
              <Failed what="The organization list" error={orgs.error} />
            )}
          </Panel>
          <Panel title="Owners" id="ent-owners" actions={<CopyAs compact op={{ service: "EnterpriseService", method: "ListEnterpriseOwners", request: { enterprise: e.slug }, label: "List the owners" }} />}>
            {owners.ok ? (
              owners.value.owners.length ? (
                <ul aria-label="Owners" className="divide-y divide-line">
                  {owners.value.owners.map((o) => (
                    <li key={o.principal?.id} className="flex items-center gap-3 py-3">
                      <PrincipalName principal={o.principal} />
                      {o.addedAt ? (
                        <span className="ml-auto text-sm text-ink-3">
                          added <Time iso={o.addedAt} />
                        </span>
                      ) : null}
                    </li>
                  ))}
                </ul>
              ) : (
                <EmptyState title="No owners" icon="user" />
              )
            ) : (
              <Failed what="The owner list" error={owners.error} />
            )}
          </Panel>
        </div>
      </div>
    </>
  );
}
