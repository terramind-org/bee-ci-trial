import type { Metadata } from "next";
import { PageMenu } from "@/components/menu";
import { PrincipalName } from "@/components/domain";
import { Icon } from "@/components/icons";
import { Markdown } from "@/components/markdown";
import { NewRelease } from "@/components/release-actions";
import { Time } from "@/components/time";
import { Badge, EmptyState, PageHeader, Panel } from "@/components/ui";
import { objectIdText } from "@/lib/api/ids";
import { ArchiveLinks } from "./archive-links";
import { type RepoParams, scopeFromParams, serverClient } from "@/lib/api/server";
import { bytes } from "@/lib/format";
import { guard } from "@/lib/page";

export const metadata: Metadata = { title: "Releases" };

function nextTag(tags: string[]): string {
  const latest = tags.map((t) => /^v(\d+)\.(\d+)\.(\d+)/.exec(t)).find(Boolean);
  if (!latest) return "v0.1.0";
  return "v" + latest[1] + "." + (Number(latest[2]) + 1) + ".0";
}

export default async function ReleasesPage({ params }: { params: Promise<RepoParams> }) {
  return guard(async () => {
    const api = await serverClient();
    const { ref, fullName } = await scopeFromParams(params);
    const req = { repo: ref };
    const { releases } = await api.release.listReleases(req);
    return (
      <>
        <PageHeader title="Releases" lede="Tags with notes and assets. A published release can't be changed." actions={<PageMenu op={{ service: "ReleaseService", method: "ListReleases", request: req, label: "List releases" }} />} />
        <div className="flex flex-col gap-10">
          <Panel title="Draft a release" id="draft-title">
            <NewRelease repo={fullName} suggestedTag={nextTag(releases.filter((r) => !r.prerelease).map((r) => r.tag))} />
          </Panel>
          {releases.length === 0 ? (
            <EmptyState title="No releases yet" icon="releases">
              Draft the first one above. Pick a tag, write the notes and publish.
            </EmptyState>
          ) : (
            releases.map((r) => (
              <article key={r.id} id={r.tag} aria-labelledby={"rel-" + r.id} className="rule scroll-mt-16 pt-8">
                <header className="flex flex-wrap items-center gap-x-3 gap-y-2">
                  <h2 id={"rel-" + r.id} className="text-xl font-semibold tracking-tight">
                    {r.tag}
                  </h2>
                  {r.prerelease ? <Badge tone="warn">pre-release</Badge> : <Badge tone="ok">latest stable</Badge>}
                  {r.immutable ? (
                    <Badge tone="outline">
                      <Icon name="lock" size={12} /> locked
                    </Badge>
                  ) : null}
                  <span className="ml-auto flex items-center gap-3 text-base text-ink-2">
                    <PrincipalName principal={r.author} showRuntime={false} />
                    <Time iso={r.createdAt} />
                    <span className="mono text-sm text-ink-3">{objectIdText(r.target, true)}</span>
                  </span>
                </header>
                <div className="mt-4 grid grid-cols-[minmax(0,1fr)] gap-6 md:grid-cols-[minmax(0,1fr)_280px]">
                  <Markdown source={r.notes} repo={fullName} />
                  <div>
                    <h3 className="mb-2 text-base font-medium">Files</h3>
                    {r.assets.length ? (
                      <ul className="flex flex-col gap-1">
                        {r.assets.map((a) => (
                          <li key={a.name} className="flex items-center gap-2 text-base">
                            <Icon name="file" size={14} className="text-ink-3" />
                            <span className="mono min-w-0 flex-1 truncate text-sm" title={a.name}>
                              {a.name}
                            </span>
                            <span className="num text-sm text-ink-3">{bytes(a.size)}</span>
                          </li>
                        ))}
                      </ul>
                    ) : (
                      <p className="mb-2 text-base text-ink-2">No files uploaded.</p>
                    )}
                    {r.target ? <ArchiveLinks repo={fullName} at={objectIdText(r.target)} tag={r.tag} /> : <p className="text-base text-ink-2">This release names no revision, so there is no source archive.</p>}
                  </div>
                </div>
              </article>
            ))
          )}
        </div>
      </>
    );
  });
}
