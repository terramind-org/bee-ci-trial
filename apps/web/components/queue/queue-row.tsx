/**
 * One row of the pull-request list: `#n` and the title, then one small line with the state as a
 * word and a shape, the agent, the person who answers for it (the yellow mark), the branch and
 * its target, and when it last moved. Under Needs you a second line says why. The whole row is
 * one link and a j/k target. A pull request with a picture in its evidence shows it, small.
 */
import Link from "next/link";
import type { ReactNode } from "react";
import type { Evidence, Principal } from "@/lib/api/types";
import { classInfo } from "@/lib/diffclass";
import { thumbnailOf } from "@/lib/evidence-view";
import type { QueueRow } from "@/lib/queue";
import { repoHref } from "@/lib/routes";
import type { StatusKind } from "@/lib/status";
import { BranchLine, Mark, Sep, State, type Shape } from "../parts";
import { Time } from "../time";

const SHAPE: Record<StatusKind, Shape> = { landed: "merged", closed: "closed", queued: "running", ready: "passed", failing: "failed", checking: "running", review: "waiting", blocked: "waiting" };

/** The state's shape: a row under Needs you is an open ring whatever else is true of it. */
export function shapeOf(row: QueueRow, needsYou: boolean): Shape {
  return needsYou ? "open" : SHAPE[row.status.kind];
}

/** The row as one line of text, for the filter and for the agent's view. */
export function rowLine(row: QueueRow, extra: { branch?: string; answers?: Principal | null }): string {
  const c = row.facts.change;
  const who = c.author?.display || c.author?.id || "";
  return ["#" + c.number, c.title, row.status.sentence, who + (extra.answers && extra.answers.id !== c.author?.id ? " for " + extra.answers.display : ""), (extra.branch ? extra.branch + " → " : "→ ") + c.targetRef, row.why !== row.status.sentence ? row.why : ""].filter(Boolean).join(" · ");
}

export function QueueRowView({ row, repo, hero, showWhy, needsYou = false, branch, answers, titleDecoration }: { row: QueueRow; repo: string; hero?: Evidence; showWhy: boolean; needsYou?: boolean; branch?: string; answers?: Principal | null; titleDecoration?: ReactNode }) {
  const { change } = row.facts;
  const rev = change.revisions.at(-1);
  const at = row.facts.landedAt ?? rev?.createdAt;
  const asset = hero ? thumbnailOf(hero) : undefined;
  const author = change.author;
  const self = !!answers && answers.id === author?.id;
  const cls = rev?.diffClass ? classInfo(rev.diffClass).label : "";
  return (
    <Link href={repoHref(repo, "changes/" + change.id)} data-nav-item className="flex items-start gap-5 border-b border-line py-5 transition-colors duration-[120ms] hover:bg-hover focus-visible:bg-hover">
      <span className="flex min-w-0 flex-1 flex-col gap-1.5">
        <span className="text-lg font-light tracking-[-0.01em] text-ink [overflow-wrap:anywhere]">
          <span className="num text-ink-2">#{change.number}</span> {change.title}
        </span>
        {/* The landing picture follows the title, even when a live merge moves the row. */}
        {titleDecoration}
        <span className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 text-xs text-ink-2">
          <State shape={shapeOf(row, needsYou)} testId="status-sentence">
            {needsYou ? "Needs you" : row.status.sentence}
          </State>
          {self ? (
            <>
              <Sep />
              <Mark>{answers.display}</Mark>
            </>
          ) : (
            <>
              {author ? (
                <>
                  <Sep />
                  <span>{author.display || author.id}</span>
                </>
              ) : null}
              {answers ? (
                <>
                  <Sep />
                  <span className="inline-flex items-center gap-1.5">
                    for <Mark>{answers.display}</Mark>
                  </span>
                </>
              ) : null}
            </>
          )}
          <Sep />
          <BranchLine branch={branch} target={change.targetRef} />
          {cls ? (
            <>
              <Sep />
              <span>{cls}</span>
            </>
          ) : null}
          {change.dependsOn ? (
            <>
              <Sep />
              <span>stacked</span>
            </>
          ) : null}
          {at ? (
            <>
              <Sep />
              <Time iso={at} className="num" />
            </>
          ) : null}
        </span>
        {showWhy && row.why !== row.status.sentence ? (
          <span className="text-base text-ink" data-testid="row-why">
            {row.why}
            {needsYou && row.status.sentence !== row.why ? <span className="text-ink-2"> · {row.status.sentence}</span> : null}
          </span>
        ) : needsYou ? (
          <span className="text-base text-ink-2" data-testid="row-why">
            {row.status.sentence}
          </span>
        ) : null}
      </span>
      {asset?.url ? (
        <span className="hidden h-14 w-[88px] shrink-0 place-items-center overflow-hidden rounded-md bg-sunken sm:grid" data-testid="hero-thumb">
          {/* eslint-disable-next-line @next/next/no-img-element -- same-origin evidence thumbnail, never cropped */}
          <img src={asset.url} alt={asset.alt} loading="lazy" decoding="async" className="block size-full object-contain" />
        </span>
      ) : null}
    </Link>
  );
}
