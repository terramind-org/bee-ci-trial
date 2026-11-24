/**
 * A run's "What happened" summary: one sentence that says how the run ended and why, and one detail
 * line naming the first failing step. Built by `lib/runsummary.ts`; it sits above the jobs.
 */
import type { RunSummary } from "@/lib/runsummary";
import { StatusIcon } from "../domain";
import { cx } from "../ui";

const TONE: Record<RunSummary["tone"], string> = { ok: "text-ok", bad: "text-bad", warn: "text-warn", neutral: "text-ink" };

export function WhatHappened({ summary }: { summary: RunSummary }) {
  return (
    <section aria-labelledby="what-happened" className="mb-10" data-testid="what-happened">
      <h2 id="what-happened" className="mb-2 text-xl font-semibold tracking-tight text-ink">
        What happened
      </h2>
      <p className={cx("flex items-start gap-2.5 text-lg leading-7 font-medium", TONE[summary.tone])}>
        <span className="mt-1.5 shrink-0">
          <StatusIcon status={summary.status} />
        </span>
        <span data-testid="run-sentence">{summary.sentence}</span>
      </p>
      {summary.detail ? <p className="mt-1 pl-[26px] text-base text-ink-2">{summary.detail}</p> : null}
    </section>
  );
}
