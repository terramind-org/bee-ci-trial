import { State } from "@/components/parts";
import type { RunSummary } from "@/lib/runsummary";
import { RUN_STATES } from "./run-facts";

export function WhatHappened({ summary }: { summary: RunSummary }) {
  const [shape] = RUN_STATES[summary.status];
  return (
    <section aria-labelledby="what-happened" data-testid="what-happened">
      <h2 id="what-happened" className="mb-2 text-sm font-medium text-ink-2">What happened</h2>
      <div className="text-xl font-light">
        <State shape={shape} testId="run-sentence">
          {summary.sentence}
        </State>
      </div>
      {summary.detail && (
        <p className="mt-2 text-base text-ink-2">{summary.detail}</p>
      )}
    </section>
  );
}
