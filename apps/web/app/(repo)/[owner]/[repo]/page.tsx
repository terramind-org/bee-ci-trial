/**
 * A repository's home, in the Document style (docs/spec/web-ui.md §2): the shell's repo header
 * above, then a calm reading column: the files with no box around them, the README set as a
 * document, and one quiet side note in place of the old About, Working now and Recent changes.
 */
import type { Metadata } from "next";
import Link from "next/link";
import { PageOp } from "@/components/copy-as";
import { Markdown } from "@/components/markdown";
import { PromptCard } from "@/components/prompt-card";
import { FeedLink } from "@/components/repo/feed-link";
import { FileTreeRoot, ROOT_ROWS, orderTree, type LastChange } from "@/components/repo/file-tree-root";
import { RepoNote, type ActiveAgent } from "@/components/repo/note";
import { pathBytes, pathText } from "@/lib/api/ids";
import { type RepoParams, currentPrincipal, scopeFromParams, serverClient } from "@/lib/api/server";
import { BeeError } from "@/lib/api/transport";
import type { TreeEntry } from "@/lib/api/types";
import { guard } from "@/lib/page";
import { feedAlternates } from "@/lib/atom";
import { promptTargets } from "@/lib/prompt-endpoint";
import { repoHref } from "@/lib/routes";

export async function generateMetadata({ params }: { params: Promise<RepoParams> }): Promise<Metadata> {
  const { owner, repo } = await params;
  const fullName = decodeURIComponent(owner) + "/" + decodeURIComponent(repo);
  return { title: fullName, alternates: feedAlternates(repoHref(fullName), fullName) };
}

/** An empty repository has no tip to read: that is a state, not an error. */
async function orEmptyRepo<X>(p: Promise<X>, empty: X): Promise<X> {
  try {
    return await p;
  } catch (err) {
    if (err instanceof BeeError && (err.code === "not_found" || err.code === "failed_precondition")) return empty;
    throw err;
  }
}

/** The README's own top heading repeats the repository name the page already shows. */
function readmeBody(markdown: string, fullName: string): string {
  const lines = markdown.split("\n");
  const first = lines.findIndex((l) => l.trim() !== "");
  const h = /^#\s+(.*)$/.exec(lines[first] ?? "");
  if (h && [fullName, fullName.split("/")[1]].includes((h[1] ?? "").trim())) return lines.slice(first + 1).join("\n");
  return markdown;
}

export default async function RepoHomePage({ params }: { params: Promise<RepoParams> }) {
  return guard(async () => {
    const { fullName, ref, owner } = await scopeFromParams(params);
    const api = await serverClient();
    const [home, me, targets] = await Promise.all([api.repo.getRepoHome({ fullName }), currentPrincipal(), promptTargets()]);
    const at = { expr: home.repo.defaultRef || "main" };
    const [tree, agents] = await Promise.all([
      orEmptyRepo<TreeEntry[]>(api.query.listTree({ repo: ref, at, path: pathBytes("") }).then((t) => t.entries), []),
      // Agents are listed for signed-in viewers; anonymous visitors see leases only.
      me
        ? api.agent
            .listAgents({ org: owner, repo: ref })
            .then((r): ActiveAgent[] => r.agents.filter((a) => a.status === "active").map((a) => ({ principal: a.principal, openChanges: a.openChanges, activeLeases: a.activeLeases })))
            .catch((): ActiveAgent[] => [])
        : Promise.resolve<ActiveAgent[]>([]),
    ]);
    // What last changed each row we show. A path whose history cannot be read just has no line.
    const shown = orderTree(tree).slice(0, ROOT_ROWS);
    const lastOf = await Promise.all(
      shown.map((e) =>
        api.query
          .log({ repo: ref, from: at, path: e.path, firstParent: true, page: { pageSize: 1 } })
          .then((l): LastChange | undefined => {
            const meta = l.changesets[0]?.meta;
            return meta ? { title: meta.message.split("\n")[0]?.trim() ?? "", at: meta.authorTime?.time ?? "" } : undefined;
          })
          .catch(() => undefined),
      ),
    );
    const lasts: Record<string, LastChange | undefined> = {};
    shown.forEach((e, i) => (lasts[pathText(e.path)] = lastOf[i]));
    const readme = home.readme;

    return (
      <>
        <PageOp op={{ service: "RepoService", method: "GetRepoHome", request: { fullName }, label: "Read this repository" }} />
        <div className="mt-10 grid grid-cols-1 gap-x-16 gap-y-14 xl:grid-cols-[minmax(0,1fr)_260px]">
          <div className="flex min-w-0 max-w-[820px] flex-col gap-14">
            <section aria-labelledby="files-heading">
              <h2 id="files-heading" className="sr-only">
                Files
              </h2>
              <FileTreeRoot fullName={fullName} entries={tree} lasts={lasts} leases={home.leases} />
            </section>
            {readme ? (
              <section aria-labelledby="readme-heading">
                <div className="mb-5 flex flex-wrap items-baseline justify-between gap-x-4 border-t border-line pt-6">
                  <h2 id="readme-heading" className="mono text-base font-normal text-ink-2">
                    {readme.path}
                  </h2>
                  <Link href={repoHref(fullName, "code?path=" + encodeURIComponent(readme.path))} className="inline-flex min-h-6 items-center text-base text-accent-ink hover:underline">
                    View source
                  </Link>
                </div>
                <Markdown source={readmeBody(readme.markdown, fullName)} repo={fullName} className="max-w-[70ch]" />
                {readme.truncated ? <p className="mt-4 text-sm text-ink-3">This README is longer than 256 KiB; the rest is in the code view.</p> : null}
              </section>
            ) : (
              <section aria-labelledby="readme-heading" className="flex max-w-xl flex-col gap-4 border-t border-line pt-6">
                <h2 id="readme-heading" className="text-xl font-semibold tracking-tight">
                  No README yet
                </h2>
                <p className="text-md text-ink-2">
                  {tree.length ? "This repository has no README. Add one so the next person, or agent, knows what it is for." : "This repository is empty. Connect a working copy and land the first change."}
                </p>
                <PromptCard id="setup-repo" vars={{ endpoint: targets.endpoint, repo: fullName }} />
              </section>
            )}
          </div>
          <RepoNote repo={home.repo} languages={home.languages} leases={home.leases} leasesVisible={home.leasesVisible} agents={agents} signedIn={!!me} />
          <FeedLink pagePath={repoHref(fullName)} />
        </div>
      </>
    );
  });
}
