import type { Metadata } from "next";
import { redirect } from "next/navigation";
import { ErrorState } from "@/components/error-state";
import { loginFrom } from "@/components/home/person";
import { OnboardingWizard } from "@/components/onboarding/wizard";
import { PageHeader } from "@/components/ui";
import { edition, serverClient, sessionState } from "@/lib/api/server";
import { guard } from "@/lib/page";
import { promptTargets } from "@/lib/prompt-endpoint";

export const metadata: Metadata = { title: "New repository" };
export const dynamic = "force-dynamic";

/** `/new`: owner, repository, then connecting an agent. Needs a session; works on both editions. */
export default async function NewPage() {
  const { me, error } = await sessionState();
  if (error && error.code === "unavailable") return <ErrorState reason={error.reason} message={error.message} retryable />;
  if (!me) redirect("/signin?next=/new");
  return guard(async () => {
    const [orgs, targets, ed] = await Promise.all([(await serverClient()).org.listOrgs({}), promptTargets(), edition()]);
    return (
      <div className="mx-auto max-w-[1120px]">
        <PageHeader title="New repository" lede="Pick an owner, name the repository, then connect your agent." />
        <OnboardingWizard login={loginFrom(me.display)} orgs={orgs.orgs.map((o) => ({ name: o.name, display: o.display }))} endpoint={targets.endpoint} edition={ed} />
      </div>
    );
  });
}
