/**
 * The top of every repo page, in the Document style (docs/spec/web-ui.md §2):
 *
 *   owner · private repository                                  [actions] [Star] […]
 *   name                         40–48px on the repo's home and Code, 28px elsewhere
 *   The description, as a lede.                                 home and Code only
 *   main · 412 files · "Fix rounding" merged 2h ago     [Clone] home and Code only
 *   Code  Change requests 6  Issues 11  Runs  Updates  More ▾
 *
 * `actions` is the slot other features mount header buttons in, left of Star (Follow does).
 * The info band loads separately (`RepoInfoBand`, inside Suspense), so the header never waits
 * on a tree walk. The "…" menu holds Copy as for GetRepoHome; it does not take over `y`, which
 * stays with the page below.
 */
import type { ReactNode } from "react";
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
  /** Open change requests and issues, shown beside their tabs. */
  counts: { changes: number; issues: number };
  /** The info band: pass `<Suspense><RepoInfoBand …/></Suspense>`. */
  band?: ReactNode;
  /** Header buttons left of Star. Follow mounts here. */
  actions?: ReactNode;
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
    <div className="mt-6 flex flex-wrap items-center gap-x-6 gap-y-2 rounded-xl bg-sunken/70 px-4 py-2.5 text-base text-ink-2" data-testid="repo-band">
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
  return <div aria-hidden="true" className="mt-6 h-[52px] animate-pulse rounded-xl bg-sunken/70" />;
}
