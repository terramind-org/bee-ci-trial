import type { Metadata } from "next";
import { CopyAs, PageOp } from "@/components/copy-as";
import { DomainsPanel } from "@/components/admin/domains-panel";
import { SamlForm } from "@/components/admin/saml-form";
import { ScimPanel } from "@/components/admin/scim-panel";
import { TwoFactorPanel } from "@/components/admin/two-factor-panel";
import { EmptyState, Panel } from "@/components/ui";
import type { SearchParams } from "@/lib/page";
import { Failed, ScopeBar, loadScope, serverClient, settle } from "../load";

export const metadata: Metadata = { title: "Admin · Identity" };
export const dynamic = "force-dynamic";

/** How people prove who they are: SAML, SCIM, verified domains and two-factor. */
export default async function IdentityPage({ searchParams }: { searchParams: SearchParams }) {
  const choice = await loadScope(await searchParams);
  if (!choice.selected) return <EmptyState title="Nothing to configure yet" icon="lock">This server has no enterprise and no organizations. Create an organization first.</EmptyState>;
  const { scope } = choice.selected;
  const api = await serverClient();
  const [saml, scim, domains, twoFactor] = await Promise.all([
    settle(api.enterprise.getSamlConfig({ scope })),
    settle(api.enterprise.getScimConfig({ scope })),
    settle(api.enterprise.listVerifiedDomains({ scope })),
    settle(api.enterprise.getTwoFactorRequirement({ scope })),
  ]);
  const samlOp = { service: "EnterpriseService", method: "GetSamlConfig", request: { scope }, label: "Show the SAML configuration" } as const;
  return (
    <>
      <PageOp op={samlOp} />
      <ScopeBar path="/admin/identity" choice={choice} />
      <div className="grid grid-cols-1 gap-10 xl:grid-cols-2">
        <Panel title="SAML single sign-on" id="id-saml" actions={<CopyAs compact op={samlOp} />}>
          {saml.ok ? <SamlForm key={choice.selected.key} scope={scope} config={saml.value} /> : <Failed what="SAML single sign-on" error={saml.error} />}
        </Panel>
        <div className="flex flex-col gap-10">
          <Panel title="SCIM provisioning" id="id-scim" actions={<CopyAs compact op={{ service: "EnterpriseService", method: "GetScimConfig", request: { scope }, label: "Show the SCIM configuration" }} />}>
            {scim.ok ? <ScimPanel key={choice.selected.key} scope={scope} config={scim.value} /> : <Failed what="SCIM provisioning" error={scim.error} />}
          </Panel>
          <Panel title="Two-factor authentication" id="id-2fa" actions={<CopyAs compact op={{ service: "EnterpriseService", method: "GetTwoFactorRequirement", request: { scope }, label: "Show the two-factor requirement" }} />}>
            {twoFactor.ok ? <TwoFactorPanel key={choice.selected.key} scope={scope} requirement={twoFactor.value} /> : <Failed what="The two-factor requirement" error={twoFactor.error} />}
          </Panel>
        </div>
      </div>
      <div className="mt-10">
        <Panel title="Verified domains" id="id-domains" actions={<CopyAs compact op={{ service: "EnterpriseService", method: "ListVerifiedDomains", request: { scope }, label: "List verified domains" }} />}>
          {domains.ok ? <DomainsPanel key={choice.selected.key} scope={scope} domains={domains.value.domains} /> : <Failed what="Verified domains" error={domains.error} />}
        </Panel>
      </div>
    </>
  );
}
