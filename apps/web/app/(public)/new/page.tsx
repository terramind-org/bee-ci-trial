import type { Metadata } from "next";
import { redirect } from "next/navigation";
import { PageFacts } from "@/components/bee/page-facts";
import { ErrorState } from "@/components/error-state";
import { loginFrom } from "@/components/home/person";
import { OnboardingWizard } from "@/components/onboarding/wizard";
import { ButtonLink, PageHeader } from "@/components/ui";
import { edition, serverClient, sessionState } from "@/lib/api/server";
import { guard } from "@/lib/page";
import { toolFor } from "@/lib/tools";
import { requestMode } from "../../request-mode";
import { promptTargets } from "@/lib/prompt-endpoint";

export const metadata: Metadata = { title: "New repository" };
export const dynamic = "force-dynamic";

/** `/new`: owner, repository, then connecting an agent. Needs a session; works on both editions. */
export default async function NewPage() {
  const { me, error } = await sessionState();
  if (error && error.code === "unavailable") return <ErrorState reason={error.reason} message={error.message} retryable />;
  if (!me) redirect("/signin?next=/new");
  return guard(async () => {
    const api = await serverClient();
    const [orgs, targets, ed, { mode }] = await Promise.all([api.org.listOrgs({}), promptTargets(), edition(), requestMode()]);
    if (mode === "bee") {
      return <PageFacts page="new-repository" title="New repository" tools={[
        toolFor("OrgService/ListOrgs", "Choose an organization that will own the repository"),
        toolFor("OrgService/CreateOrg", "Create an organization to own it"),
        toolFor("RepoService/CreateRepo", "Create a repository in the selected namespace"),
        toolFor("RepoService/GetRepoHome", "Read the new repository and its clone URLs"),
      ]} groups={[
        { title: "organizations", lines: orgs.orgs.map((o) => o.display ? o.name + " · " + o.display : o.name), empty: "You do not belong to an organization yet." },
        { title: "repository", facts: [
          { label: "required", value: "owner and repository name" },
          { label: "visibility", value: ed === "enterprise" ? "Private, Public or Internal" : "Private or Public" },
          { label: "initial commit", value: "Optional README" },
        ] },
      ]} />;
    }
    return (
      <div className="max-w-3xl">
        <PageHeader title="New repository" actions={<ButtonLink href="/import" variant="ghost">Import instead</ButtonLink>} />
        <OnboardingWizard login={loginFrom(me.display)} orgs={orgs.orgs.map((o) => ({ name: o.name, display: o.display }))} endpoint={targets.endpoint} edition={ed} />
      </div>
    );
  });
}
