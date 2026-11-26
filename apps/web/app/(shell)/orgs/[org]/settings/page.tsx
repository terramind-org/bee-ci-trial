import type { Metadata } from "next";
import Link from "next/link";
import { requestMode } from "@/app/request-mode";
import { PageFacts, type FactGroup } from "@/components/bee/page-facts";
import { ButtonLink, EmptyState } from "@/components/kit";
import { PageMenu } from "@/components/menu";
import { Kv, Row, Rows, Sect, State, type Shape } from "@/components/parts";
import { PageHeader } from "@/components/ui";
import { serverClient } from "@/lib/api/server";
import type { Visibility } from "@/lib/api/types";
import { absoluteTime } from "@/lib/format";
import { guard } from "@/lib/page";
import { repoHref } from "@/lib/routes";
import { toolFor } from "@/lib/tools";
import { SettingsTabs } from "./tabs";

export const metadata: Metadata = { title: "Organization settings" };

const VISIBILITY: Partial<Record<Visibility, { label: string; shape: Shape }>> = {
  VISIBILITY_PUBLIC: { label: "Public", shape: "open" },
  VISIBILITY_INTERNAL: { label: "Internal", shape: "waiting" },
  VISIBILITY_PRIVATE: { label: "Private", shape: "closed" },
};

/** The organization and the repositories whose rules it can review. */
export default async function OrgSettingsPage({ params }: { params: Promise<{ org: string }> }) {
  return guard(async () => {
    const [api, { org: encodedOrg }, { mode }] = await Promise.all([serverClient(), params, requestMode()]);
    const org = decodeURIComponent(encodedOrg);
    const request = { nameOrId: org };
    const [info, { repos }] = await Promise.all([api.org.getOrg(request), api.repo.listRepos({ org })]);
    const title = info.display || info.name;
    const organization: FactGroup = {
      title: "organization",
      facts: [
        { label: "Name", value: info.name },
        info.display && { label: "Display name", value: info.display },
        { label: "ID", value: <span className="mono">{info.id}</span> },
        info.createdAt && { label: "Created", value: <time dateTime={info.createdAt}>{absoluteTime(info.createdAt)}</time> },
      ].map((fact) => fact || null),
    };
    const empty = "No repositories yet";
    if (mode === "bee") {
      return <PageFacts
        page="org-settings"
        title={title + " · settings"}
        groups={[
          organization,
          {
            title: "repositories",
            lines: repos.map((repo) => [repo.fullName, VISIBILITY[repo.visibility]?.label, "rules at " + repoHref(repo.fullName, "settings")].filter(Boolean).join(" · ")),
            empty,
          },
        ]}
        tools={[
          toolFor("OrgService/GetOrg", "Read this organization’s profile"),
          toolFor("RepoService/ListRepos", "List the repositories you can see in this organization"),
        ]}
      />;
    }

    return (
      <>
        <PageHeader eyebrow={title} title="Organization settings" actions={<PageMenu op={{ service: "OrgService", method: "GetOrg", request, label: "Show this organization" }} />} />
        <SettingsTabs org={org} active="general" />
        <div className="flex max-w-5xl flex-col gap-10">
          <Sect title="Organization" id="org-profile">
            <Kv rows={(organization.facts ?? []).map((fact) => fact ? [fact.label, fact.value] : null)} />
          </Sect>
          <Sect title="Repositories" id="org-repos">
            {repos.length ? (
              <Rows label="Repositories" className="border-t-0">
                {repos.map((repo) => {
                  const visibility = VISIBILITY[repo.visibility];
                  return <Row key={repo.id}
                    meta={visibility && <State shape={visibility.shape}>{visibility.label}</State>}
                    action={<ButtonLink href={repoHref(repo.fullName, "settings")} size="sm"><span>Rules<span className="sr-only"> for {repo.fullName}</span></span></ButtonLink>}
                  >
                    <Link href={repoHref(repo.fullName)} className="link inline-block min-h-6" data-nav-item>{repo.fullName}</Link>
                  </Row>;
                })}
              </Rows>
            ) : <EmptyState title={empty}>Start with <Link href="/new" className="link inline-block min-h-6">a new repository</Link> or <Link href="/import" className="link inline-block min-h-6">import one</Link>.</EmptyState>}
          </Sect>
        </div>
      </>
    );
  });
}
