/**
 * Every rule a pull request has to meet, as the server lists them (`LandingEvaluation`): what
 * is still missing and what is met, each in plain words with the raw requirement under it, so a
 * person reads the sentence and an agent matches the token. Read-only: the decisions are the
 * buttons under the title.
 */
import type { LandingEvaluation } from "@/lib/api/types";
// The pull request brief renders on the server, including when it hosts the post panel.
import { LINEAR_REQUIREMENT, explainRequirement, refusesMerge } from "@/lib/infra";
import { missingInWords } from "@/lib/status";
import { LinearRefusal } from "../change-actions";
import { State } from "../parts";

/** A missing requirement as a sentence; one the page has no sentence for falls back to its short name. */
function inWords(entry: string): string {
  const words = missingInWords(entry);
  return words === entry ? explainRequirement(entry) : "It " + words + ".";
}

export function RuleList({ evaluation }: { evaluation: LandingEvaluation }) {
  const missing = evaluation.missing.filter((m) => m !== LINEAR_REQUIREMENT);
  return (
    <div className="flex flex-col gap-4" data-testid="rule-list">
      {refusesMerge(evaluation.missing) ? <LinearRefusal /> : null}
      {missing.length ? (
        <div>
          <p className="mb-1.5 text-xs font-medium text-ink-2">Still needed</p>
          <ul className="flex flex-col gap-2">
            {missing.map((m) => (
              <li key={m} className="text-base">
                <State shape="waiting">{inWords(m)}</State>
                <span className="mono block pl-[18px] text-ink-2 [overflow-wrap:anywhere]">{m}</span>
              </li>
            ))}
          </ul>
        </div>
      ) : null}
      {evaluation.satisfied.length ? (
        <div>
          <p className="mb-1.5 text-xs font-medium text-ink-2">Met</p>
          <ul className="flex flex-col gap-2">
            {evaluation.satisfied.map((m) => (
              <li key={m} className="text-base">
                <State shape="passed">{explainRequirement(m)}</State>
                <span className="mono block pl-[18px] text-ink-2 [overflow-wrap:anywhere]">{m}</span>
              </li>
            ))}
          </ul>
        </div>
      ) : null}
      {!missing.length && !evaluation.satisfied.length && !refusesMerge(evaluation.missing) ? <p className="text-base text-ink-2">The rules ask for nothing here.</p> : null}
    </div>
  );
}
