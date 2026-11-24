import type { Metadata } from "next";
import { CopyAs, PageOp } from "@/components/copy-as";
import { IpAllowPanel } from "@/components/admin/ip-allow-panel";
import { EmptyState, Panel } from "@/components/ui";
import type { SearchParams } from "@/lib/page";
import { Failed, ScopeBar, loadScope, serverClient, settle } from "../load";

export const metadata: Metadata = { title: "Admin · Security" };
export const dynamic = "force-dynamic";

/** Network access: the IP allow list. */
export default async function SecurityPage({ searchParams }: { searchParams: SearchParams }) {
  const choice = await loadScope(await searchParams);
  if (!choice.selected) return <EmptyState title="Nothing to configure yet" icon="lock">This server has no enterprise and no organizations. Create an organization first.</EmptyState>;
  const { scope } = choice.selected;
  const entries = await settle((await serverClient()).enterprise.listIpAllowList({ scope }));
  const op = { service: "EnterpriseService", method: "ListIpAllowList", request: { scope }, label: "List the IP allow list" } as const;
  return (
    <>
      <PageOp op={op} />
      <ScopeBar path="/admin/security" choice={choice} />
      <Panel title="IP allow list" id="sec-ip" actions={<CopyAs compact op={op} />}>
        {entries.ok ? <IpAllowPanel key={choice.selected.key} scope={scope} entries={entries.value.entries} /> : <Failed what="The IP allow list" error={entries.error} />}
      </Panel>
    </>
  );
}
