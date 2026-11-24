/** "Needs you": one plain sentence per item, the asker's avatar, and one action. */
import Link from "next/link";
import { Avatar, toAvatar } from "@/components/avatar";
import { Icon } from "@/components/icons";
import { ButtonLink } from "@/components/ui";
import { relativeTime } from "@/lib/format";
import { ACTION, type Need } from "@/lib/inbox/needs";
import { capitalize, list, riskOf } from "@/lib/inbox/words";
import { repoHref } from "@/lib/routes";

/** Where a need's one action goes: the change request, or the post panel on it. */
export function needHref(need: Need): string {
  return repoHref(need.repo, "changes/" + need.change.id) + (need.kind === "post" ? "#post" : "");
}

export function NeedsList({ needs, now }: { needs: Need[]; now: number }) {
  return (
    <ul className="divide-y divide-line" data-testid="needs-you">
      {needs.map((n, i) => (
        <li key={n.kind + n.repo + n.change.id} data-kind={n.kind} className="flex flex-col gap-3 py-5 sm:flex-row sm:items-start sm:gap-4">
          <div className="flex min-w-0 flex-1 items-start gap-4">
            <Avatar principal={toAvatar(n.who)} size={36} className="mt-0.5" />
            <div className="min-w-0 flex-1">
              <p className="text-lg leading-[26px] text-ink [overflow-wrap:anywhere]">
                <Sentence need={n} />
              </p>
              <p className="mt-1 flex flex-wrap items-center gap-x-2 text-base text-ink-3">
                <Reason need={n} />
                {n.at ? (
                  <>
                    <span aria-hidden="true">·</span>
                    <time dateTime={n.at}>{relativeTime(n.at, now)}</time>
                  </>
                ) : null}
              </p>
            </div>
          </div>
          {/* The first item is the next thing to do and carries the one accent button; the rest are quiet. */}
          <ButtonLink href={needHref(n)} variant={i === 0 ? "primary" : "secondary"} icon={i === 0 ? "arrowRight" : undefined} className="ml-13 self-start sm:ml-0">
            {ACTION[n.kind]}
            <span className="sr-only">: {n.change.title}</span>
          </ButtonLink>
        </li>
      ))}
    </ul>
  );
}

function Title({ need }: { need: Need }) {
  return (
    <Link href={needHref(need)} className="font-semibold underline decoration-line-strong decoration-1 underline-offset-[5px] hover:decoration-ink">
      {need.change.title}
    </Link>
  );
}

function Who({ need }: { need: Need }) {
  return <span className="font-semibold">{need.who?.display ?? "Someone"}</span>;
}

function Repo({ need }: { need: Need }) {
  return <span className="text-ink-2">{need.repo}</span>;
}

function Sentence({ need }: { need: Need }) {
  switch (need.kind) {
    case "review":
      return need.revisedAfterYou ? (
        <>
          <Who need={need} /> revised <Title need={need} /> in <Repo need={need} /> after you asked for changes. It needs your review.
        </>
      ) : (
        <>
          <Who need={need} /> wants to merge <Title need={need} /> into <Repo need={need} />. It needs your review.
        </>
      );
    case "failing":
      return need.mine ? (
        <>
          Your change <Title need={need} /> in <Repo need={need} /> failed the {list(need.failing)} check.
        </>
      ) : (
        <>
          <Who need={need} /> is stuck: <Title need={need} /> in <Repo need={need} /> failed the {list(need.failing)} check.
        </>
      );
    case "merge":
      return (
        <>
          <Who need={need} /> finished <Title need={need} />. It is ready to merge into <Repo need={need} />.
        </>
      );
    case "blocked":
      return (
        <>
          <Title need={need} /> by <Who need={need} /> in <Repo need={need} /> is blocked: it {list(need.reasons)}.
        </>
      );
    case "post":
      return (
        <>
          <Who need={need} /> wrote a post about <Title need={need} /> in <Repo need={need} />. It waits for your approval to publish.
        </>
      );
  }
}

function Reason({ need }: { need: Need }) {
  switch (need.kind) {
    case "failing":
      return <Why icon={<Icon name="x" size={17} weight="bold" className="text-bad" />}>Checks failed</Why>;
    case "merge":
      return <Why icon={<Icon name="merge" size={17} className="text-ok" />}>All rules pass</Why>;
    case "blocked":
      return <Why icon={<Icon name="lock" size={17} className="text-warn" />}>Blocked by policy</Why>;
    case "post":
      return <Why icon={<Icon name="updates" size={17} className="text-ink-3" />}>Merged; the post is not public yet</Why>;
    case "review": {
      const risk = riskOf(need.change.revisions.at(-1)?.diffClass ?? "");
      return (
        <Why icon={<Icon name="check" size={17} className="text-ink-3" />}>
          {capitalize(need.reasons.find((r) => r.includes("approval")) ?? "needs an approval")}
          <span className="text-ink-3"> · {risk.level.toLowerCase()} risk</span>
        </Why>
      );
    }
  }
}

function Why({ icon, children }: { icon: React.ReactNode; children: React.ReactNode }) {
  return (
    <span className="inline-flex items-center gap-1.5">
      {icon}
      <span>{children}</span>
    </span>
  );
}
