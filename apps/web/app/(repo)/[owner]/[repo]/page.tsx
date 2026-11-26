/**
 * A repository's home, the Code tab's first page (docs/spec/web-ui.md §3, "Code"): under the
 * shell's repository header, one reading column. The README first, set as a document; then the
 * files, the recent commits, who is editing now, and what the repository is. No side column.
 *
 * In the agent's view the same facts are plain text, followed by the tools this page calls.
 */
import type { Metadata } from "next";
import Link from "next/link";
import { repositoryWork } from "@/components/agents/work";
import { PageFacts, type FactGroup } from "@/components/bee/page-facts";
import { PageOp } from "@/components/copy-as";
import { Markdown } from "@/components/markdown";
import { Quiet, Sect } from "@/components/parts";
import { PromptCard } from "@/components/prompt-card";
import { CommitRows, commitLine, commitView } from "@/components/repo/commits";
import { FeedLink } from "@/components/repo/feed-link";
import { FileTreeRoot, ROOT_ROWS, orderTree, type LastChange } from "@/components/repo/file-tree-root";
import { activityHref } from "@/components/repo/presence";
import { languagesText } from "@/components/repo/language-bar";
import { RepoNote, editingMentions, type ActiveAgent } from "@/components/repo/note";
import { pathBytes, pathText } from "@/lib/api/ids";
import { type RepoParams, currentPrincipal, scopeFromParams, serverClient } from "@/lib/api/server";
import { BeeError } from "@/lib/api/transport";
import type { Changeset, Principal, TreeEntry } from "@/lib/api/types";
import { directoryOf } from "@/lib/pulls";
import { pageTools } from "@/lib/tools";
import { requestMode } from "../../../request-mode";
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

/** Commits shown on the home; the rest are in the code browser's history. */
const RECENT_COMMITS = 5;

const VISIBILITY: Record<string, string> = { VISIBILITY_PUBLIC: "Public", VISIBILITY_INTERNAL: "Internal" };

export default async function RepoHomePage({ params }: { params: Promise<RepoParams> }) {
  return guard(async () => {
    const { fullName, ref, owner } = await scopeFromParams(params);
    const api = await serverClient();
    const [home, me, targets, { mode }] = await Promise.all([api.repo.getRepoHome({ fullName }), currentPrincipal(), promptTargets(), requestMode()]);
    const branch = home.repo.defaultRef || "main";
    const at = { expr: branch };
    const [tree, agents, log, members, work] = await Promise.all([
      orEmptyRepo<TreeEntry[]>(api.query.listTree({ repo: ref, at, path: pathBytes("") }).then((t) => t.entries), []),
      // Agents are listed for signed-in viewers; anonymous visitors see leases only.
      me
        ? api.agent
            .listAgents({ org: owner, repo: ref })
            .then((r): ActiveAgent[] => r.agents.filter((a) => a.status === "active").map((a) => ({ principal: a.principal, openChanges: a.openChanges, activeLeases: a.activeLeases })))
            .catch((): ActiveAgent[] => [])
        : Promise.resolve<ActiveAgent[]>([]),
      // History that cannot be read (an empty repository, a server without Log) is simply not shown.
      api.query
        .log({ repo: ref, from: at, firstParent: true, page: { pageSize: RECENT_COMMITS } })
        .then((l) => l.changesets)
        .catch((): Changeset[] => []),
      // The organisation's people, to name the person behind an agent's commit. Members only.
      me
        ? api.org
            .listMembers({ org: owner })
            .then((r) => r.members.map((m) => m.principal))
            .catch((): (Principal | undefined)[] => [])
        : Promise.resolve<(Principal | undefined)[]>([]),
      me ? repositoryWork(api, ref).catch(() => []) : [],
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
    const commits = log.map((c) => commitView(c, directoryOf(members)));
    const historyHref = repoHref(fullName, "code?view=history");

    if (mode === "bee") {
      const repo = home.repo;
      const groups: FactGroup[] = [
        {
          title: "facts",
          facts: [
            { label: "repository", value: fullName },
            repo.description ? { label: "description", value: repo.description } : null,
            { label: "visibility", value: (VISIBILITY[repo.visibility] ?? "Private").toLowerCase() },
            { label: "default branch", value: branch },
            { label: "open pull requests", value: String(home.openChanges) },
            languagesText(home.languages) ? { label: "languages", value: languagesText(home.languages) } : null,
            repo.topics.length ? { label: "topics", value: repo.topics.join(", ") } : null,
            repo.licenseSpdx ? { label: "license", value: repo.licenseSpdx } : null,
            home.cloneUrls?.https ? { label: "clone", value: home.cloneUrls.https } : null,
            { label: "readme", value: readme ? readme.path + " · " + repoHref(fullName, "code?path=" + encodeURIComponent(readme.path)) : "none" },
          ],
        },
        {
          title: "files at " + branch,
          lines: orderTree(tree).map((e) => {
            const name = pathText(e.path);
            const last = lasts[name];
            return name + (e.mode === "MODE_TREE" ? "/" : "") + (e.restricted ? " · restricted" : "") + (last?.title ? " · " + last.title : "");
          }),
          empty: "The repository is empty.",
        },
        { title: "commits", lines: commits.map(commitLine), empty: "No commits yet." },
        { title: "editing now", lines: editingMentions(home.leases, agents).map(({ id, line, principal, lease }) => <Link key={id} href={activityHref(fullName, work, me ? principal : undefined, lease)} className="link">{line}</Link>), empty: home.leasesVisible || me ? "Nobody is editing right now." : "Not shown to signed-out readers." },
      ];
      return <PageFacts page="code" title={fullName + " · code"} groups={groups} tools={pageTools("code")} level={2} />;
    }

    return (
      <>
        <PageOp op={{ service: "RepoService", method: "GetRepoHome", request: { fullName }, label: "Read this repository" }} />
        <div className="flex max-w-[820px] min-w-0 flex-col gap-10">
          {readme ? (
            <section aria-labelledby="readme-heading">
              <h2 id="readme-heading" className="sr-only">
                README
              </h2>
              <Markdown source={readmeBody(readme.markdown, fullName)} repo={fullName} className="max-w-[70ch]" />
              <p className="mt-5 flex flex-wrap items-baseline gap-x-2 text-xs text-ink-2">
                <span className="mono">{readme.path}</span>
                <span aria-hidden="true">·</span>
                <Link href={repoHref(fullName, "code?path=" + encodeURIComponent(readme.path))} className="link inline-flex min-h-6 items-center">
                  View source
                </Link>
                {readme.truncated ? <span>· This README is longer than 256 KiB; the rest is in the code view.</span> : null}
              </p>
            </section>
          ) : (
            <section aria-labelledby="readme-heading" className="flex max-w-xl flex-col gap-4">
              <h2 id="readme-heading" className="text-xl font-light tracking-tight">
                No README yet
              </h2>
              <p className="text-md text-ink-2">
                {tree.length ? "This repository has no README. Add one so the next person, or agent, knows what it is for." : "This repository is empty. Connect a working copy and merge the first commit."}
              </p>
              <PromptCard id="setup-repo" vars={{ endpoint: targets.endpoint, repo: fullName }} />
            </section>
          )}
          <Sect id="files-heading" title="Files">
            <FileTreeRoot fullName={fullName} entries={tree} lasts={lasts} leases={home.leases} work={work} />
          </Sect>
          <Sect id="commits-heading" title="Commits">
            {commits.length ? (
              <>
                <CommitRows commits={commits} />
                <Quiet className="mt-4">
                  Every commit names who wrote it, and for an agent the person it answers to.{" "}
                  <Link href={historyHref} className="link">
                    All commits
                  </Link>
                </Quiet>
              </>
            ) : (
              <Quiet>No commits yet.</Quiet>
            )}
          </Sect>
          <RepoNote repo={home.repo} languages={home.languages} leases={home.leases} leasesVisible={home.leasesVisible} agents={agents} signedIn={!!me} work={work} />
          <FeedLink pagePath={repoHref(fullName)} />
        </div>
      </>
    );
  });
}
