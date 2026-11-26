import type { Metadata } from "next";
import { requestMode } from "@/app/request-mode";
import { PageFacts } from "@/components/bee/page-facts";
import { EmptyState } from "@/components/kit";
import { PageMenu } from "@/components/menu";
import { Row, Rows, Sect, Sep, State } from "@/components/parts";
import { PageHeader } from "@/components/ui";
import { serverClient } from "@/lib/api/server";
import type { App } from "@/lib/api/types";
import { absoluteTime, plural } from "@/lib/format";
import { guard } from "@/lib/page";
import { toolFor } from "@/lib/tools";
import { SettingsTabs } from "../tabs";

export const metadata: Metadata = { title: "Apps" };

function permissions(app: Pick<App, "permissions">): string {
  return (app.permissions ?? []).map((p) => {
    const level = p.level?.replace("APP_PERMISSION_LEVEL_", "").toLowerCase();
    return level && level !== "unspecified" ? p.resource + ":" + level : p.resource;
  }).join(", ");
}

const noApps = "This organization owns no apps.";
const noInstalls = "No apps are installed.";

export default async function AppsPage({ params }: { params: Promise<{ org: string }> }) {
  return guard(async () => {
    const [api, { org: encodedOrg }, { mode }] = await Promise.all([serverClient(), params, requestMode()]);
    const org = decodeURIComponent(encodedOrg);
    const request = { org };
    const { apps, installations } = await api.app.listApps(request);
    const owned = apps.map((app) => ({
      id: app.id,
      name: app.name || app.slug,
      slug: app.name && app.slug !== app.name ? app.slug : undefined,
      visibility: app.public ? "Public" : "Private",
      description: app.description,
      permissions: permissions(app),
      installations: app.installationCount === undefined ? undefined : plural(Number(app.installationCount), "installation"),
      createdAt: app.createdAt,
    }));
    const installed = installations.map((app) => ({
      id: app.id,
      name: app.appSlug,
      status: app.suspended ? "Suspended" : "Active",
      permissions: permissions(app),
      installer: app.installedBy?.display || app.installedBy?.id,
      createdAt: app.createdAt,
    }));

    if (mode === "bee") {
      return <PageFacts
        page="org-apps"
        title={org + " · apps"}
        groups={[
          {
            title: "owned apps",
            lines: owned.map((app) => [app.name, app.slug, app.visibility, app.description, app.permissions, app.installations, absoluteTime(app.createdAt)].filter(Boolean).join(" · ")),
            empty: noApps,
          },
          {
            title: "installed apps",
            lines: installed.map((app) => [app.name, app.status, app.permissions, app.installer && "installed by " + app.installer, absoluteTime(app.createdAt)].filter(Boolean).join(" · ")),
            empty: noInstalls,
          },
        ]}
        tools={[toolFor("AppService/ListApps", "List this organization’s apps")]}
      />;
    }

    return (
      <>
        <PageHeader eyebrow={org} title="Apps" actions={<PageMenu op={{ service: "AppService", method: "ListApps", request, label: "List apps" }} />} />
        <SettingsTabs org={org} active="apps" />
        <div className="flex max-w-5xl flex-col gap-10">
          <Sect title="Owned apps" id="apps-owned-title">
            {owned.length ? (
              <Rows testId="app-list" className="border-t-0">
                {owned.map((app) => (
                  <Row key={app.id} meta={<>
                    <State shape={app.visibility === "Public" ? "open" : "closed"}>{app.visibility}</State>
                    {app.slug ? <><Sep /><span className="mono [overflow-wrap:anywhere]">{app.slug}</span></> : null}
                    {app.permissions ? <><Sep /><span className="[overflow-wrap:anywhere]">{app.permissions}</span></> : null}
                    {app.installations ? <><Sep />{app.installations}</> : null}
                    {app.createdAt ? <><Sep /><time dateTime={app.createdAt}>{absoluteTime(app.createdAt)}</time></> : null}
                  </>}>
                    {app.name}{app.description ? <span className="text-ink-2"> · {app.description}</span> : null}
                  </Row>
                ))}
              </Rows>
            ) : <EmptyState title={noApps} />}
          </Sect>
          <Sect title="Installed apps" id="apps-installed-title">
            {installed.length ? (
              <Rows className="border-t-0">
                {installed.map((app) => (
                  <Row key={app.id} meta={<>
                    <State shape={app.status === "Suspended" ? "closed" : "open"}>{app.status}</State>
                    {app.permissions ? <><Sep /><span className="[overflow-wrap:anywhere]">{app.permissions}</span></> : null}
                    {app.installer ? <><Sep />Installed by {app.installer}</> : null}
                    {app.createdAt ? <><Sep /><time dateTime={app.createdAt}>{absoluteTime(app.createdAt)}</time></> : null}
                  </>}>
                    {app.name}
                  </Row>
                ))}
              </Rows>
            ) : <EmptyState title={noInstalls} />}
          </Sect>
        </div>
      </>
    );
  });
}
