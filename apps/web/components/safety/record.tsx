import Link from "next/link";
import type { ReactNode } from "react";
import { PageFacts, type FactGroup } from "@/components/bee/page-facts";
import { Quiet, Rows, State } from "@/components/parts";
import { Time } from "@/components/time";
import { PageHeader, Panel } from "@/components/ui";
import type { SafetyRecord, SafetyRef, SafetyReview } from "@/lib/api/types";
import type { Mode } from "@/lib/mode";
import { toolFor } from "@/lib/tools";
import { ReviewAction, VerdictAction, type Reviewer } from "./actions";
import { Answerable, EvidenceList, EvidenceSubject, Finding, Verdict } from "./evidence";
import { CONCERN, INTRO, OPEN, findingSource, safetyHref, signalName, verdictReason, workLabel } from "./model";
import { repoHref } from "@/lib/routes";
import { SafetyLive, StopNotice } from "./repository";

const NOT_RECORDED = "Not recorded by the runner";

interface RecordProps {
  repo: string;
  repoId: string;
  record: SafetyRecord;
  work: SafetyRef;
  mode: Mode;
  frozen?: boolean;
  canDecide: boolean;
  reviewers?: Reviewer[];
}

function Review({ review }: { review: SafetyReview }) {
  const reviewer = review.reviewer?.display || review.reviewer?.id;
  return <>
    <State shape={review.submittedAt ? "passed" : "waiting"}>{review.submittedAt ? "Review submitted" : "Awaiting review"}</State>
    {reviewer ? <> · {reviewer}</> : null}{review.model ? <> · {review.model}</> : null}
    {review.submittedAt ? <> · Proposed verdict <Verdict value={review.proposedVerdict} /></> : null}
    {review.submittedAt || review.requestedAt ? <> · <Time iso={review.submittedAt || review.requestedAt} /></> : null}
    {review.note ? <span className="block whitespace-pre-wrap">{review.note}</span> : null}
    {review.findingIds.length ? <span className="flex flex-wrap gap-x-3">
      {review.findingIds.map((id, i) => <Link key={id} className="link inline-block min-h-6" href={"#finding-" + encodeURIComponent(id)}>Finding {i + 1}</Link>)}
    </span> : null}
  </>;
}

function doneEvidence(repo: string, record: SafetyRecord): { label: string; items: ReactNode[]; empty: string }[] {
  return [
    { label: "Files touched", items: (record.done?.paths ?? []).map((path) => <code key={path}>{path}</code>), empty: "No files in the recorded evidence." },
    { label: "Merged pull requests", items: (record.done?.mergedChangeRequestIds ?? []).map((id) => <Link key={id} className="link inline-block min-h-6" href={repoHref(repo, "changes/" + encodeURIComponent(id))}>Pull request <code>{id}</code></Link>), empty: "No merges in the recorded evidence." },
    { label: "Commands", items: (record.done?.commands ?? []).map((command) => <code key={command} className="whitespace-pre-wrap">{command}</code>), empty: NOT_RECORDED },
    { label: "Network destinations", items: (record.done?.networkDestinations ?? []).map((destination) => <code key={destination}>{destination}</code>), empty: NOT_RECORDED },
  ];
}

export function RecordSafety({ repo, repoId, record, work, mode, frozen, canDecide, reviewers }: RecordProps) {
  const asked = record.asked.map((entry, i) => <span key={i} className="block">
    <span className="block whitespace-pre-wrap">{entry.text}</span>
    {entry.source ? <span className="block text-sm text-ink-2">Source · <EvidenceSubject subject={entry.source} repo={repo} repoId={repoId} /></span> : null}
  </span>);
  const done = doneEvidence(repo, record);
  const back = <Link className="link inline-block min-h-6" href={safetyHref(repo)}>All Safety records</Link>;
  const note = record.override?.note;
  const lede = <>
    <Verdict value={record.verdict} /> <span className="text-ink-2">{verdictReason(record)}</span>
    {note ? <span className="mt-2 block whitespace-pre-wrap text-ink-2">{note}</span> : null}
  </>;
  const top = <>
    <SafetyLive repo={repo} /><StopNotice frozen={frozen} />
  </>;

  if (mode === "bee") {
    const groups: FactGroup[] = [
      { title: "evidence", lines: [INTRO, back] },
      { title: "Asked", lines: asked, empty: "No request was recorded." },
      { title: "Done", facts: done.map(({ label, items, empty }) => ({ label, value: items.length ? items.map((item, i) => <span key={i} className="block">{item}</span>) : empty })) },
      { title: "Verdict", lines: [lede, ...(record.answerable ? [<Answerable key="person" person={record.answerable} />] : [])] },
      { title: "Findings", lines: record.findings.map((finding) => <span key={finding.id} id={"finding-" + finding.id}>
        {finding.report?.evidence.observation || "No observation was recorded."} · {findingSource(finding)}
        {finding.report ? <> · <code>{signalName(finding.report.kind)}</code></> : null}
        {finding.report?.evidence.paths?.map((path) => <code key={path} className="block">{path}</code>)}
        {finding.report?.evidence.subjects?.map((subject, i) => <span key={i} className="block"><EvidenceSubject subject={subject} repo={repo} repoId={repoId} /></span>)}
        {finding.state !== OPEN ? <span className="block">
          {finding.state === "SAFETY_FINDING_STATE_CONFIRMED" ? "Confirmed" : finding.state === "SAFETY_FINDING_STATE_DISMISSED" ? "Dismissed" : "State unavailable"}
          {finding.resolvedBy ? " by " + (finding.resolvedBy.display || finding.resolvedBy.id) : ""}
          {finding.resolutionReason ? " · " + finding.resolutionReason : ""}
        </span> : null}
      </span>), empty: "No findings were recorded." },
      { title: "Independent review", lines: record.reviews.map((review) => <Review key={review.id} review={review} />), empty: "No independent review was requested." },
    ];
    return <>{top}<PageFacts page="safety-record" title={workLabel(record)} groups={groups} tools={[
      toolFor("SafetyService/GetSafetyRecord", "Read this record and its evidence"),
      toolFor("SafetyService/RaiseConcern", CONCERN),
      toolFor("SafetyService/RequestSafetyReview", "Ask for an independent review"),
      toolFor("SafetyService/ResolveFinding", "Confirm or dismiss a finding with a reason"),
      toolFor("SafetyService/SetVerdict", "Set a verdict and record a note"),
    ]} /></>;
  }
  return <>
    {top}
    <PageHeader title={workLabel(record)} eyebrow={back} lede={INTRO} meta={<Answerable person={record.answerable} />} />
    <div className="flex max-w-4xl flex-col gap-10">
      <div className="grid gap-8 md:grid-cols-2">
        <Panel title="Asked" id="safety-asked" bodyClassName="text-base [overflow-wrap:anywhere]">
          <EvidenceList label="Request sources" items={asked} empty="No request was recorded." />
        </Panel>
        <Panel title="Done" id="safety-done">
          <dl className="grid gap-3 text-base">
            {done.map(({ label, items, empty }) => <div key={label} className="border-t border-line pt-3">
              <dt className="mb-1 text-xs font-medium text-ink-2">{label}</dt>
              <dd><EvidenceList label={label} items={items} empty={empty} /></dd>
            </div>)}
          </dl>
        </Panel>
      </div>
      <Panel title="Verdict" id="safety-current-verdict" bodyClassName="text-base [overflow-wrap:anywhere]">{lede}</Panel>
      <Panel title="Findings" id="safety-findings">
        {record.findings.length ? <Rows>{record.findings.map((finding) => <li key={finding.id} className="border-b border-line py-4">
          <Finding id={"finding-" + finding.id} finding={finding} repo={repo} repoId={repoId} work={work} canDecide={canDecide} />
        </li>)}</Rows> : <Quiet>No findings were recorded.</Quiet>}
      </Panel>
      <Panel title="Independent review" id="safety-review" bodyClassName="space-y-4">
        {record.reviews.length ? <Rows>{record.reviews.map((review) => <li key={review.id} className="border-b border-line py-4 text-base [overflow-wrap:anywhere]"><Review review={review} /></li>)}</Rows> : <Quiet>No independent review was requested.</Quiet>}
        {canDecide && reviewers ? <ReviewAction work={work} reviewers={reviewers} /> : null}
      </Panel>
      {canDecide ? <Panel title="Set the verdict yourself" id="safety-override"><VerdictAction key={record.verdict} work={work} current={record.verdict} /></Panel> : null}
    </div>
  </>;
}
