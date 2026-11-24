import type { Metadata } from "next";
import Link from "next/link";
import { PageMenu } from "@/components/menu";
import { VisibilityBadge } from "@/components/repo-card";
import { Time } from "@/components/time";
import { EmptyState, PageHeader, Panel } from "@/components/ui";
import { serverClient } from "@/lib/api/server";
import { guard } from "@/lib/page";
import { repoHref } from "@/lib/routes";
import { SettingsTabs } from "./tabs";

export const metadata: Metadata = { title: "Organization settings" };

/** The org itself and its repositories, each linking to that repo's policy settings. */
export default async function OrgSettingsPage({ params }: { params: Promise<{ org: string }> }) {
  return guard(async () => {
    const api = await serverClient();
    const org = decodeURIComponent((await params).org);
    const req = { nameOrId: org };
    const [info, repos] = await Promise.all([api.org.getOrg(req), api.repo.listRepos({ org })]);
    return (
      <>
        <PageHeader title="Settings" lede={<>{info.display || info.name}: the organization, and where each repository keeps its policy.</>} actions={<PageMenu op={{ service: "OrgService", method: "GetOrg", request: req, label: "Show this organization" }} />} />
        <SettingsTabs org={org} active="general" />
        <div className="grid grid-cols-1 gap-5 lg:grid-cols-[minmax(0,1fr)_320px]">
          <Panel title="Repositories" id="org-repos">
            {repos.repos.length === 0 ? (
              <EmptyState title="No repositories yet" icon="repo">
                Create one from <Link href="/new" className="link">New</Link>, or bring one across with <Link href="/import" className="link">Import</Link>.
              </EmptyState>
            ) : (
              <ul className="divide-y divide-line">
                {repos.repos.map((r) => (
                  <li key={r.id} className="flex flex-wrap items-center gap-x-3 gap-y-1 py-3">
                    <Link href={repoHref(r.fullName)} className="text-base font-medium hover:underline" data-nav-item>
                      {r.fullName}
                    </Link>
                    <VisibilityBadge visibility={r.visibility} />
                    <Link href={repoHref(r.fullName, "settings")} className="ml-auto text-base text-accent-ink hover:underline">
                      Policy
                    </Link>
                  </li>
                ))}
              </ul>
            )}
          </Panel>
          <Panel title="Organization">
            <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-2 py-3 text-base">
              <dt className="text-ink-3">Name</dt>
              <dd>{info.name}</dd>
              <dt className="text-ink-3">Display name</dt>
              <dd>{info.display || info.name}</dd>
              <dt className="text-ink-3">Id</dt>
              <dd className="mono truncate text-sm">{info.id}</dd>
              {info.createdAt ? (
                <>
                  <dt className="text-ink-3">Created</dt>
                  <dd>
                    <Time iso={info.createdAt} />
                  </dd>
                </>
              ) : null}
            </dl>
          </Panel>
        </div>
      </>
    );
  });
}
