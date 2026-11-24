/** A test report in words: the counts as a sentence, then each failure by name with its message. */
import { CheckCircleIcon, XCircleIcon } from "@phosphor-icons/react/dist/ssr";
import type { TestReport as Report } from "@/lib/api/types";
import { testReportPassed, testReportWords } from "@/lib/evidence-view";

export function TestReport({ report }: { report: Report }) {
  const ok = testReportPassed(report);
  return (
    <div className="flex flex-col gap-3" data-test-report>
      <p className="flex items-center gap-2 text-lg text-ink">
        {ok ? <CheckCircleIcon size={22} weight="fill" aria-hidden="true" className="shrink-0 text-ok" /> : <XCircleIcon size={22} weight="fill" aria-hidden="true" className="shrink-0 text-bad" />}
        <span>
          <span className="sr-only">{ok ? "Passed: " : "Failed: "}</span>
          {testReportWords(report)}
        </span>
      </p>
      {report.summary ? <p className="text-base text-ink-2">{report.summary}</p> : null}
      {report.failures.length ? (
        <ul className="flex flex-col gap-2">
          {report.failures.map((f, i) => (
            <li key={f.name + i} className="flex flex-col gap-0.5">
              <span className="mono text-sm text-bad [overflow-wrap:anywhere]">{f.name}</span>
              {f.message ? <span className="text-sm whitespace-pre-wrap text-ink-2 [overflow-wrap:anywhere]">{f.message}</span> : null}
            </li>
          ))}
        </ul>
      ) : null}
    </div>
  );
}
