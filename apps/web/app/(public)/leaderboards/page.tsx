import type { Metadata } from "next";
import { PageFacts } from "@/components/bee/page-facts";
import { PageMenu } from "@/components/menu";
import { LeaderboardTable } from "@/components/discover/leaderboard-table";
import { BOARDS, boardById, metricSupported, parseChoice, type Since } from "@/components/discover/params";
import { SINCE_LABEL, SINCE_WINDOW } from "@/components/discover/params";
import { ExploreTabs } from "@/components/discover/explore-tabs";
import { Pager, href } from "@/components/discover/pager";
import { SectionError, attempt, discoveryFacts } from "@/components/discover/section";
import { Segments, WindowSwitch } from "@/components/discover/window-switch";
import { Time } from "@/components/time";
import { EmptyState, PageHeader } from "@/components/ui";
import { serverClient } from "@/lib/api/server";
import { ownerHref, repoHref } from "@/lib/routes";
import { toolFor } from "@/lib/tools";
import { requestMode } from "../../request-mode";
import { guard, param, type SearchParams } from "@/lib/page";

export const metadata: Metadata = { title: "Leaderboards" };

const SINCES: readonly Since[] = ["week", "month"];

export default async function LeaderboardsPage({ searchParams }: { searchParams: SearchParams }) {
  const sp = await searchParams;
  const board = boardById(param(sp, "board"));
  const since = parseChoice(param(sp, "since"), SINCES, "week");
  const after = param(sp, "after") || undefined;
  const here = (over: Record<string, string | undefined>) => href("/leaderboards", { board: board.id === BOARDS[0]?.id ? undefined : board.id, since: since === "week" ? undefined : since, ...over });

  return guard(async () => {
    const [api, { mode }] = await Promise.all([serverClient(), requestMode()]);
    const req = { window: SINCE_WINDOW[since], subject: board.subject, metric: board.metric, page: { pageSize: 25, ...(after ? { pageToken: after } : {}) } };
    const op = { service: "SearchService" as const, method: "ListLeaderboard", request: req, label: board.label + ", " + SINCE_LABEL[since].toLowerCase() };
    // The boards table only lists combinations the server answers; a hand-edited URL falls back to the first board.
    const loaded = metricSupported(board.subject, board.metric) ? await attempt(api.search.listLeaderboard(req)) : null;
    const res = loaded?.ok ? loaded.data : null;
    const next = res?.page?.nextPageToken ? here({ after: res.page.nextPageToken }) : null;
    const filters = (
      <div className="mb-8 flex flex-col gap-3">
        <Segments label="Board" current={board.id} segments={BOARDS.map((b) => ({ value: b.id, label: b.label, href: here({ board: b.id === BOARDS[0]?.id ? undefined : b.id, after: undefined }) }))} />
        <WindowSwitch current={since} options={SINCES} hrefFor={(s) => here({ since: s === "week" ? undefined : s, after: undefined })} />
      </div>
    );
    if (mode === "bee") {
      return (
        <>
          <PageFacts page="leaderboards" title="Leaderboards" tools={[toolFor("SearchService/ListLeaderboard", "Read the ranking by subject, metric and time window")]} groups={[
            { title: "board", facts: [
              { label: "metric", value: board.label }, { label: "window", value: SINCE_LABEL[since] },
              res?.computedAt ? { label: "computed at", value: res.computedAt } : null,
            ] },
            loaded ? discoveryFacts("rankings", loaded, ({ entries }) => entries.flatMap((e, i) => {
              const name = e.user?.login ?? e.repo?.fullName;
              return name ? [[e.rank || i + 1, name, e.user?.principal?.runtime, e.score + " " + board.unit, e.user ? ownerHref(name) : repoHref(name)].filter(Boolean).join(" · ")] : [];
            }), "Nobody on this board yet.") : { title: "rankings", empty: "This board is not available." },
          ]} />
          <div className="mt-6"><ExploreTabs current="leaderboards" />{filters}<Pager nextHref={next} /></div>
        </>
      );
    }
    return (
      <>
        <PageHeader
          title="Leaderboards"
          meta={
            res?.computedAt ? (
              <span className="text-sm text-ink-3">
                Computed <Time iso={res.computedAt} />
              </span>
            ) : undefined
          }
          actions={<PageMenu op={op} />}
        />
        <ExploreTabs current="leaderboards" />
        {filters}
        <div>
          {res ? <LeaderboardTable entries={res.entries} unit={board.unit} caption={board.label + ", " + SINCE_LABEL[since].toLowerCase()} /> : loaded && !loaded.ok ? <SectionError what="Leaderboard" error={loaded.error} /> : <EmptyState title="This board is not available" />}
          <Pager nextHref={next} />
        </div>
      </>
    );
  });
}
