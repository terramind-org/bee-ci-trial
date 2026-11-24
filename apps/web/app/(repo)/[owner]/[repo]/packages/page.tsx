import type { Metadata } from "next";
import { PageMenu } from "@/components/menu";
import { PrincipalName } from "@/components/domain";
import { Time } from "@/components/time";
import { Badge, EmptyState, PageHeader } from "@/components/ui";
import { type RepoParams, scopeFromParams, serverClient } from "@/lib/api/server";
import { bytes, compact, principalOf } from "@/lib/format";
import { guard } from "@/lib/page";

export const metadata: Metadata = { title: "Packages" };

const ECOSYSTEM: Record<string, string> = {
  PACKAGE_ECOSYSTEM_CONTAINER: "container",
  PACKAGE_ECOSYSTEM_NPM: "npm",
  PACKAGE_ECOSYSTEM_MAVEN: "maven",
  PACKAGE_ECOSYSTEM_NUGET: "nuget",
  PACKAGE_ECOSYSTEM_RUBYGEMS: "rubygems",
};

export default async function PackagesPage({ params }: { params: Promise<RepoParams> }) {
  return guard(async () => {
    const api = await serverClient();
    const { owner: org } = await scopeFromParams(params);
    const req = { owner: org };
    const { packages } = await api.package.listPackages(req);
    return (
      <>
        <PageHeader
          title="Packages"
          lede={"Containers, npm, Maven, NuGet and RubyGems packages published by " + org + ", with the change and run they came from."}
          actions={<PageMenu op={{ service: "PackageService", method: "ListPackages", request: req, label: "List packages" }} />}
        />
        <div>
          {packages.length === 0 ? (
            <EmptyState title="No packages yet" icon="package">
              Publish with your ecosystem&apos;s client against this server, e.g. <code className="mono">npm publish --registry &lt;server&gt;/npm/{org}/</code>.
            </EmptyState>
          ) : (
            <ul className="divide-y divide-line" data-testid="package-list">
              {packages.map(({ package: p, latest }) => (
                <li key={p.id} className="flex flex-col gap-2 py-4 md:flex-row md:items-center md:gap-6">
                  <div className="min-w-0 flex-1">
                    <p className="flex flex-wrap items-center gap-2">
                      <Badge tone="outline">{ECOSYSTEM[p.ecosystem ?? ""] ?? "package"}</Badge>
                      <span className="mono text-md font-medium text-ink">{p.name}</span>
                      {latest ? <span className="mono text-base text-ink-2">{latest.version}</span> : null}
                      {(latest?.tags ?? []).map((t) => (
                        <Badge key={t}>{t}</Badge>
                      ))}
                      <Badge tone="neutral">{(p.visibility ?? "PACKAGE_VISIBILITY_PRIVATE").replace("PACKAGE_VISIBILITY_", "").toLowerCase()}</Badge>
                    </p>
                    {p.description ? <p className="mt-1 text-base text-ink-2">{p.description}</p> : null}
                    <p className="mt-1 flex flex-wrap gap-x-3 text-sm text-ink-3">
                      {p.registryUrl ? <span className="mono [overflow-wrap:anywhere]">{p.registryUrl}</span> : null}
                      <span>
                        {p.versionCount ?? 0} version{p.versionCount === 1 ? "" : "s"}
                      </span>
                      <span>{compact(p.downloadCount ?? "0")} downloads</span>
                      {latest?.sizeBytes ? <span>{bytes(latest.sizeBytes)}</span> : null}
                    </p>
                  </div>
                  <div className="flex items-center gap-3 text-base text-ink-2">
                    {latest?.publisher ? <PrincipalName principal={principalOf(latest.publisher)} /> : null}
                    <Time iso={latest?.createdAt ?? p.updatedAt} />
                  </div>
                </li>
              ))}
            </ul>
          )}
        </div>
      </>
    );
  });
}
