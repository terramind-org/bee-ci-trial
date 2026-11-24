import type { Metadata } from "next";
import { CopyAs, PageOp } from "@/components/copy-as";
import { LeaderboardTable } from "@/components/discover/leaderboard-table";
import { BOARDS, boardById, metricSupported, parseChoice, type Since } from "@/components/discover/params";
import { SINCE_LABEL, SINCE_WINDOW } from "@/components/discover/params";
import { ExploreTabs } from "@/components/discover/explore-tabs";
import { Pager, href } from "@/components/discover/pager";
import { Segments, WindowSwitch } from "@/components/discover/window-switch";
import { Time } from "@/components/time";
import { EmptyState, PageHeader } from "@/components/ui";
import { serverClient } from "@/lib/api/server";
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
    const api = await serverClient();
    const req = { window: SINCE_WINDOW[since], subject: board.subject, metric: board.metric, page: { pageSize: 25, ...(after ? { pageToken: after } : {}) } };
    const op = { service: "SearchService" as const, method: "ListLeaderboard", request: req, label: board.label + ", " + SINCE_LABEL[since].toLowerCase() };
    // The boards table only lists combinations the server answers; a hand-edited URL falls back to the first board.
    const res = metricSupported(board.subject, board.metric) ? await api.search.listLeaderboard(req) : null;
    const next = res?.page?.nextPageToken ? here({ after: res.page.nextPageToken }) : null;
    return (
      <>
        <PageOp op={op} />
        <PageHeader
          title="Leaderboards"
          lede="Who is landing, reviewing and gaining stars on Beehive."
          meta={
            res?.computedAt ? (
              <span className="text-sm text-ink-3">
                Computed <Time iso={res.computedAt} />
              </span>
            ) : undefined
          }
          actions={<CopyAs op={op} />}
        />
        <ExploreTabs current="leaderboards" />
        <div className="mb-8 flex flex-col gap-3">
          <Segments label="Board" current={board.id} segments={BOARDS.map((b) => ({ value: b.id, label: b.label, href: here({ board: b.id === BOARDS[0]?.id ? undefined : b.id, after: undefined }) }))} />
          <WindowSwitch current={since} options={SINCES} hrefFor={(s) => here({ since: s === "week" ? undefined : s, after: undefined })} />
        </div>
        <div>
          {res ? <LeaderboardTable entries={res.entries} unit={board.unit} caption={board.label + ", " + SINCE_LABEL[since].toLowerCase()} /> : <EmptyState title="This board is not available" icon="dot" />}
          <Pager nextHref={next} />
        </div>
      </>
    );
  });
}
