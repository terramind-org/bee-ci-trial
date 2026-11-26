"use client";

/**
 * The change request's tabs: Brief (default), Code, Checks and For agents, as plain links over one
 * hairline. The active tab comes from the layout segment, so the layout itself stays a server
 * component and never needs the URL.
 */
import { useSelectedLayoutSegment } from "next/navigation";
import { TabLinks } from "@/components/ui";
import { CHANGE_TABS, changeHref, tabOf } from "@/lib/change-page";

export function ChangeTabs({ repo, id, counts }: { repo: string; id: string; counts: { checks?: number; files?: number } }) {
  const active = tabOf(useSelectedLayoutSegment());
  const count = { brief: undefined, code: counts.files, checks: counts.checks, agents: undefined };
  return (
    <TabLinks
      label="Pull request"
      items={CHANGE_TABS.map((t) => ({ href: changeHref(repo, id, t.tab), label: t.label, active: t.tab === active, count: count[t.tab] }))}
    />
  );
}
