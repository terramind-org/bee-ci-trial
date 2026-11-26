import Link from "next/link";
import type { ReactNode } from "react";
import { Mark, State } from "@/components/parts";
import type { Principal, SafetyAsk, SafetyAttention, SafetyFinding, SafetyRecord, SafetyRef, SafetyVerdict } from "@/lib/api/types";
import { plural } from "@/lib/format";
import { repoHref } from "@/lib/routes";
import { FindingActions } from "./actions";
import { findingSource, OPEN, runHref, safetyHref, signalName, verdictOf, workLabel } from "./model";

const LINK = "link inline-flex min-h-6 items-center [overflow-wrap:anywhere]";

export function Verdict({ value }: { value: SafetyVerdict }) {
  const verdict = verdictOf(value);
  return verdict ? <State shape={verdict[2]} testId="safety-verdict">{verdict[1]}</State> : <span>Verdict unavailable</span>;
}

export function Answerable({ person }: { person?: Principal }) {
  return person ? <Mark>{person.display || person.id} answers for it</Mark> : null;
}

export function RecordLink({ repo, record }: { repo: string; record: SafetyRecord }) {
  return record.work ? <Link href={safetyHref(repo, record.work)} className={LINK}>{workLabel(record)}</Link> : workLabel(record);
}

export function Disclosure({ title, children }: { title: ReactNode; children: ReactNode }) {
  return <details>
    <summary className="link min-h-6 cursor-pointer text-sm text-ink-2">{title}</summary>
    <div className="mt-2">{children}</div>
  </details>;
}

export function EvidenceList({ label, items, empty }: { label: string; items: ReactNode[]; empty?: string }) {
  const list = (lines: ReactNode[]) => <ul aria-label={label} className="space-y-1 [overflow-wrap:anywhere]">{lines.map((line, i) => <li key={i}>{line}</li>)}</ul>;
  if (!items.length) return empty ? <span className="text-ink-2">{empty}</span> : null;
  return <div className="space-y-2">
    {list(items.slice(0, 3))}
    {items.length > 3 ? <Disclosure title={"Show " + (items.length - 3) + " more"}>{list(items.slice(3))}</Disclosure> : null}
  </div>;
}

const SUBJECTS: Record<string, [string, string?]> = {
  SUBJECT_KIND_AGENT_RUN: ["Agent run", "agents/runs"],
  SUBJECT_KIND_SWARM: ["Swarm", "agents/swarms"],
  SUBJECT_KIND_ISSUE: ["Issue", "work"],
  SUBJECT_KIND_CHANGE_REQUEST: ["Pull request", "changes"],
  SUBJECT_KIND_COMMENT: ["Comment"],
};

export function EvidenceSubject({ subject, repo, repoId }: { subject: NonNullable<SafetyAsk["source"]>; repo: string; repoId: string }) {
  const [label, path] = SUBJECTS[subject.kind ?? ""] ?? ["Recorded source"];
  const fullName = subject.repo?.fullName || (!subject.repo?.id || subject.repo.id === repoId ? repo : undefined);
  return path && subject.id && fullName
    ? <Link className={LINK} href={repoHref(fullName, path + "/" + encodeURIComponent(subject.id))}>{label} <code className="ml-1">{subject.id}</code></Link>
    : <span>{label}{subject.id ? <> <code>{subject.id}</code></> : null}</span>;
}

export function Finding({ finding, repo, repoId, work, canDecide, id }: { finding: SafetyFinding; repo: string; repoId: string; work?: SafetyRef; canDecide?: boolean; id?: string }) {
  const evidence = finding.report?.evidence;
  const resolved = finding.state === "SAFETY_FINDING_STATE_CONFIRMED" ? "Confirmed" : finding.state === "SAFETY_FINDING_STATE_DISMISSED" ? "Dismissed" : null;
  return <div id={id} className="space-y-2 text-base [overflow-wrap:anywhere]">
    <p>{evidence?.observation || "No observation was recorded."}</p>
    <p className="text-xs text-ink-2">{findingSource(finding)}{finding.report ? <> · <code>{signalName(finding.report.kind)}</code></> : null}</p>
    <Disclosure title="Evidence">
      <div className="space-y-2">
        {evidence?.paths?.length ? <EvidenceList label="Evidence paths" items={evidence.paths.map((path) => <code key={path}>{path}</code>)} /> : null}
        {evidence?.subjects?.length ? <EvidenceList label="Evidence sources" items={evidence.subjects.map((subject, i) => <EvidenceSubject key={i} subject={subject} repo={repo} repoId={repoId} />)} /> : null}
        {!(evidence?.paths?.length || evidence?.subjects?.length) ? <p>The observation is the only evidence on this finding.</p> : null}
      </div>
    </Disclosure>
    {resolved ? <p className="text-sm text-ink-2">
      <State shape={resolved === "Confirmed" ? "passed" : "closed"}>{resolved}</State>
      {finding.resolvedBy ? " by " + (finding.resolvedBy.display || finding.resolvedBy.id) : ""}
      {finding.resolutionReason ? " · " + finding.resolutionReason : ""}
    </p> : null}
    {canDecide && work && finding.state === OPEN ? <Disclosure title="Confirm or dismiss this finding">
      <FindingActions work={work} findingId={finding.id} />
    </Disclosure> : null}
  </div>;
}

export function TripwireEffect({ item, repo }: { item: SafetyAttention; repo?: string }) {
  const wire = item.tripwire;
  if (!wire) return null;
  const effects = [wire.pausedSwarm ? "Paused the swarm" : "", wire.stoppedRunIds.length ? "Stopped " + plural(wire.stoppedRunIds.length, "run") : ""].filter(Boolean);
  return <>
    <span>{effects.join(" · ") || "No running work was stopped"}. {wire.reason}</span>
    {wire.stoppedRunIds.map((id) => <span key={id} className="ml-2"> {repo ? <Link className={LINK} href={runHref(repo, id)}>Run <code className="ml-1">{id}</code></Link> : <code>{id}</code>}</span>)}
  </>;
}

export function AttentionText({ item, repo }: { item: SafetyAttention; repo?: string }) {
  const asker = item.concern?.raisedBy?.display || item.concern?.raisedBy?.id || "An agent";
  return item.concern
    ? <>{asker} asked for help. “{item.concern.text}”</>
    : <TripwireEffect item={item} repo={repo} />;
}
