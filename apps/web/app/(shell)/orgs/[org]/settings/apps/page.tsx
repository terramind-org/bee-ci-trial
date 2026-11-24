import type { Metadata } from "next";
import { PageMenu } from "@/components/menu";
import { PrincipalName } from "@/components/domain";
import { Time } from "@/components/time";
import { Badge, EmptyState, PageHeader, Panel } from "@/components/ui";
import { serverClient } from "@/lib/api/server";
import type { App } from "@/lib/api/types";
import { principalOf } from "@/lib/format";
import { guardSection } from "@/lib/page";
import { SettingsTabs } from "../tabs";

export const metadata: Metadata = { title: "Apps" };

const perms = (a: Pick<App, "permissions">) => (a.permissions ?? []).map((p) => p.resource + ":" + (p.level ?? "").replace("APP_PERMISSION_LEVEL_", "").toLowerCase()).join(", ");

export default async function AppsPage({ params }: { params: Promise<{ org: string }> }) {
  const org = decodeURIComponent((await params).org);
  const req = { org };
  return (
    <>
      <PageHeader title="Settings" lede={<>Apps {org} owns and apps installed on it. An app acts as its own service principal, scoped by the permissions it was granted.</>} actions={<PageMenu op={{ service: "AppService", method: "ListApps", request: req, label: "List apps" }} />} />
      <SettingsTabs org={org} active="apps" />
      {await guardSection("Apps", async () => {
        const api = await serverClient();
        const { apps, installations } = await api.app.listApps(req);
        return (
        <div className="flex flex-col gap-5">
          <Panel title="Owned by this org" id="apps-owned-title">
            {apps.length === 0 ? (
              <EmptyState title="No apps yet" icon="key">
                Register one from a manifest with <code className="mono">bee api AppService/CreateAppFromManifest</code>.
              </EmptyState>
            ) : (
              <ul className="divide-y divide-line" data-testid="app-list">
                {apps.map((a) => (
                  <li key={a.id} className="flex flex-col gap-1 py-3 md:flex-row md:items-center md:gap-4">
                    <div className="min-w-0 flex-1">
                      <p className="flex flex-wrap items-center gap-2">
                        <span className="font-medium text-ink">{a.name}</span>
                        <span className="mono text-sm text-ink-3">{a.slug}</span>
                        <Badge tone={a.public ? "accent" : "neutral"}>{a.public ? "public" : "private"}</Badge>
                      </p>
                      {a.description ? <p className="text-base text-ink-2">{a.description}</p> : null}
                      <p className="mono text-sm text-ink-3">{perms(a) || "no permissions requested"}</p>
                    </div>
                    <span className="text-base text-ink-2">
                      <span className="num font-semibold text-ink">{a.installationCount ?? 0}</span> installations
                    </span>
                    <Time iso={a.createdAt} />
                  </li>
                ))}
              </ul>
            )}
          </Panel>
          <Panel title="Installed on this org" id="apps-installed-title">
            {installations.length === 0 ? (
              <EmptyState title="Nothing installed" icon="key" />
            ) : (
              <ul className="divide-y divide-line">
                {installations.map((i) => (
                  <li key={i.id} className="flex flex-col gap-1 py-3 md:flex-row md:items-center md:gap-4">
                    <span className="mono font-medium">{i.appSlug}</span>
                    {i.suspended ? <Badge tone="bad">suspended</Badge> : <Badge tone="ok">active</Badge>}
                    <span className="mono flex-1 text-sm text-ink-3">{perms(i)}</span>
                    {i.installedBy ? <PrincipalName principal={principalOf(i.installedBy)} /> : null}
                    <Time iso={i.createdAt} />
                  </li>
                ))}
              </ul>
            )}
          </Panel>
        </div>
        );
      })}
    </>
  );
}
