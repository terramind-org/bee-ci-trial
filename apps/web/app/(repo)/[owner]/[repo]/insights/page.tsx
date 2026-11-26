import type { Metadata } from "next";
import { ButtonLink, EmptyState, PageHeader } from "@/components/ui";
import { type RepoParams, scopeFromParams } from "@/lib/api/server";
import { repoHref } from "@/lib/routes";

export const metadata: Metadata = { title: "Insights" };

/**
 * The Insights tab's place. Its numbers (merged without a person, refused by the rules, time
 * from issue to merge, cost, attempts that came back empty) are not derived on the server yet,
 * so this page shows none rather than estimates. The live stream is one link away.
 */
export default async function InsightsPage({ params }: { params: Promise<RepoParams> }) {
  const { fullName } = await scopeFromParams(params);
  return (
    <>
      <PageHeader title="Insights" lede={"How " + fullName + " is doing."} />
      <EmptyState title="Insights are not built yet" action={<ButtonLink href={repoHref(fullName, "activity")}>Activity</ButtonLink>}>
        This page will show a handful of numbers a person steers by, each with its trend. The server does not compute them yet, so none are shown. What is happening in {fullName} right now is on Activity.
      </EmptyState>
    </>
  );
}
