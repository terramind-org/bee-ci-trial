/**
 * The top of every repo page (docs/spec/web-ui.md §2), the same on all of them:
 *
 *   owner / name  [Private]  The description, in grey.      Follow · 3  Star · 2  Fork · 0  …
 *   main · 412 files · "Fix rounding" merged 2h ago   [Clone]        the repo's home and Code only
 *   Code  Issues 11  Pull requests 6  Agents  CI  Safety  Insights  Settings  More ▾
 *
 * `actions` is the slot other features mount header buttons in, left of Star (Follow does).
 * The info band loads separately (`RepoInfoBand`, inside Suspense), so the header never waits
 * on a tree walk. The "…" menu holds Copy as for GetRepoHome; it does not take over `y`, which
 * stays with the page below.
 */
import type { ReactNode } from "react";
import { PageFacts } from "./bee/page-facts";
import { pageTools } from "@/lib/tools";
import type { ForkStatus } from "./repo-header-data";
import { pathBytes } from "@/lib/api/ids";
import { serverClient } from "@/lib/api/server";
import type { Visibility } from "@/lib/api/types";
import { plural } from "@/lib/format";
import { Icon } from "./icons";
import { CloneMenu, RepoHeaderView } from "./repo-header-parts";
import { Time } from "./time";

export interface RepoHeaderProps {
  fullName: string;
  description: string;
  visibility: Visibility;
  /** Settings shows only to callers who can write. */
  canWrite: boolean;
  signedIn: boolean;
  star: { starred: boolean; count?: number; unavailable?: string };
  canSync?: boolean;
  agent?: boolean;
  upstream?: ForkStatus;
  /** How many forks the server counts (`Repo.forks`). */
  forks: number;
  /** Open pull requests and issues, shown beside their tabs. */
  counts: { changes: number; issues: number };
  /** The info band: pass `<Suspense><RepoInfoBand …/></Suspense>`. */
  band?: ReactNode;
  /** Header buttons left of Star. Follow mounts here. */
  actions?: ReactNode;
}

export function RepoHeaderFacts({ fullName, description, visibility, star, forks, counts, upstream, following, followers, signedIn, canSync, defaultRef }: Pick<RepoHeaderProps, "fullName" | "description" | "visibility" | "star" | "forks" | "counts" | "upstream" | "signedIn"> & { following?: boolean; followers?: number; canSync: boolean; defaultRef?: string }) {
  const tools = pageTools("repository-header").filter(({ op, kind }) =>
    (signedIn || kind === "read") &&
    ((op !== "RepoService/ListForks") || signedIn) &&
    (op !== "RepoService/SyncFork" || canSync)
  );
  return (
    <div className="mt-10">
      <PageFacts page="repository-header" level={2} title={fullName + " · repository details"} tools={tools} groups={[{ title: "repository", facts: [
        { label: "visibility", value: visibility === "VISIBILITY_PUBLIC" ? "Public" : visibility === "VISIBILITY_INTERNAL" ? "Internal" : "Private" },
        description ? { label: "description", value: description } : null,
        defaultRef ? { label: "default branch", value: defaultRef } : null,
        { label: "open pull requests", value: counts.changes },
        { label: "open issues", value: counts.issues },
        star.count !== undefined && { label: "stars", value: star.count },
        signedIn && !star.unavailable && { label: "starred by you", value: star.starred ? "yes" : "no" },
        followers !== undefined && { label: "followers", value: followers },
        following !== undefined && { label: "followed by you", value: following ? "yes" : "no" },
        { label: "forks", value: forks },
        upstream && { label: "forked from", value: upstream.fullName },
        upstream?.behind !== undefined && { label: "sync status", value: upstream.behind ? "behind upstream" : "up to date with upstream" },
      ] }]} />
    </div>
  );
}

export function RepoHeader(props: RepoHeaderProps) {
  return <RepoHeaderView {...props} />;
}

function titleOf(message: string): string {
  return message.split("\n")[0]?.trim() ?? "";
}

/** Branch, file count, the latest merge and Clone. Renders nothing it cannot load. */
export async function RepoInfoBand({ fullName, defaultRef }: { fullName: string; defaultRef: string }) {
  const api = await serverClient();
  const repo = { fullName };
  const branch = defaultRef || "main";
  const [home, files] = await Promise.all([
    api.repo.getRepoHome({ fullName }).catch(() => null),
    api.query
      .listTree({ repo, at: { expr: branch }, path: pathBytes(""), recursive: true })
      .then((t) => t.entries.filter((e) => e.mode !== "MODE_TREE").length)
      .catch(() => null),
  ]);
  const land = home?.latestLand?.meta;
  return (
    <div className="mb-6 flex flex-wrap items-center gap-x-6 gap-y-2 text-sm text-ink-2" data-testid="repo-band">
      <span className="inline-flex items-center gap-2">
        <Icon name="branch" size={18} className="text-ink-3" />
        <span className="mono text-ink">{branch}</span>
      </span>
      {files !== null ? (
        <span className="inline-flex items-center gap-2">
          <Icon name="files" size={18} className="text-ink-3" />
          <span className="num">{plural(files, "file")}</span>
        </span>
      ) : null}
      {land ? (
        <span className="inline-flex min-w-0 items-center gap-2">
          <Icon name="merge" size={18} className="shrink-0 text-ink-3" />
          <span className="min-w-0 truncate">
            <em className="text-ink">{titleOf(land.message)}</em> merged <Time iso={land.authorTime?.time} />
          </span>
        </span>
      ) : null}
      {home ? (
        <span className="ml-auto">
          <CloneMenu fullName={fullName} https={home.cloneUrls?.https ?? ""} ssh={home.cloneUrls?.ssh} />
        </span>
      ) : null}
    </div>
  );
}

/** The band's place while it loads. */
export function RepoInfoBandSkeleton() {
  return <div aria-hidden="true" className="mb-6 h-[34px] rounded-full bg-sunken" />;
}
