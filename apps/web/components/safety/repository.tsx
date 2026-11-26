import Link from "next/link";
import { PageFacts, type FactGroup } from "@/components/bee/page-facts";
import { buttonClass } from "@/components/kit";
import { LiveRefresh } from "@/components/live-refresh";
import { Quiet, Rows } from "@/components/parts";
import { PageHeader, Panel } from "@/components/ui";
import type { SafetyAttention, SafetyRecord } from "@/lib/api/types";
import { plural } from "@/lib/format";
import type { Mode } from "@/lib/mode";
import { toolFor } from "@/lib/tools";
import { AcknowledgeAction, VerdictAction } from "./actions";
import { Answerable, AttentionText, Disclosure, Finding, RecordLink, Verdict } from "./evidence";
import { AS_ASKED, CONCERN, DETECTED, INTRO, NEEDS_LOOK, OPEN, REPORTED, STOPPED, VERDICTS, findingSource, needsLook, safetyHref } from "./model";

const ON_RECORDS = [
  toolFor("SafetyService/ListSafetyRecords", "List work and its evidence"),
  toolFor("SafetyService/GetSafetyRecord", "Read what was asked and the evidence of what was done"),
];
export const RAISE_CONCERN = toolFor("SafetyService/RaiseConcern", CONCERN);
export const SET_VERDICT = toolFor("SafetyService/SetVerdict", "Set a verdict and record a note");
export const RESOLVE_FINDING = toolFor("SafetyService/ResolveFinding", "Confirm or dismiss a finding with a reason");

export function SafetyLive({ repo }: { repo: string }) {
  return <LiveRefresh repo={repo} kinds={["safety.*"]} />;
}

export function StopNotice({ frozen }: { frozen?: boolean }) {
  return frozen ? <p role="status" className="mb-4 border-b border-line pb-3 text-base font-medium text-ink">{STOPPED}</p> : null;
}

function SignalLines() {
  return <>
    <span className="block">Detected by the server · <code>{DETECTED.join(", ")}</code></span>
    <span className="block">Reported only by a reviewer or a person · <code>{REPORTED.join(", ")}</code></span>
    <span className="block">Runners do not record network destinations or mark instructions from untrusted content.</span>
  </>;
}

interface RepositorySafetyProps {
  repo: string;
  repoId: string;
  mode: Mode;
  records: SafetyRecord[];
  attention: SafetyAttention[];
  sample?: SafetyRecord[];
  sampleHref?: string;
  next?: string;
  frozen?: boolean;
  canSample: boolean;
  person: boolean;
  canDecideAll: boolean;
  deciderId?: string;
}

function canDecide(record: SafetyRecord, props: RepositorySafetyProps) {
  return props.canDecideAll || (!!props.deciderId && record.answerable?.id === props.deciderId);
}

function recordKey(record: SafetyRecord, fallback: number) {
  return record.work ? record.work.kind + "/" + record.work.id : fallback;
}

function rowLine(repo: string, record: SafetyRecord, showFindings = false) {
  const open = record.findings.filter((f) => f.state === OPEN);
  return <>
    <RecordLink repo={repo} record={record} /> · <Verdict value={record.verdict} />
    {record.answerable ? <> · <Answerable person={record.answerable} /></> : null}
    {showFindings ? open.map((finding) => <span key={finding.id} className="block">
      {finding.report?.evidence.observation} · {findingSource(finding)}{record.work ? <> · <Link className="link" href={safetyHref(repo, record.work) + "#finding-" + encodeURIComponent(finding.id)}>Evidence</Link></> : null}
    </span>) : null}
  </>;
}

function nextLink(next?: string) {
  return next ? <Link className="link inline-block min-h-6" href={next}>More records</Link> : null;
}

export function RepositorySafety(props: RepositorySafetyProps) {
  const { repo, repoId, mode, records, attention, frozen, sample, sampleHref, next, canSample, person } = props;
  const needs = records.filter(needsLook);
  const groups = VERDICTS.filter(([key]) => key !== NEEDS_LOOK).map(([key, label]) => ({
    key, label, records: records.filter((r) => !needsLook(r) && r.verdict === key),
  }));
  const other = records.filter((r) => !needsLook(r) && !VERDICTS.some(([key]) => key === r.verdict));
  const wires = attention.filter((a) => a.tripwire);
  const concerns = attention.filter((a) => a.concern);
  const sampleEmpty = "No qualifying merges were found in this period.";
  const attentionGroups = [
    { title: "Tripwires", rows: wires, empty: "No tripwires are waiting for your attention." },
    { title: "Concerns agents raised", rows: concerns, empty: "No concerns are waiting for your attention." },
  ];

  if (mode === "bee") {
    const tools = [
      ...ON_RECORDS,
      ...(person ? [toolFor("SafetyService/ListSafetyAttention", "Read tripwires and concerns addressed to you")] : []),
      ...(canSample ? [toolFor("SafetyService/SpotCheck", "Sample verified agent merges without recorded human approval")] : []),
      RAISE_CONCERN,
      ...(person ? [toolFor("SafetyService/AcknowledgeSafetyAttention", "Acknowledge an item addressed to you")] : []),
      RESOLVE_FINDING, SET_VERDICT,
    ];
    const facts: FactGroup[] = [
      { title: "evidence", lines: [INTRO] },
      { title: "Needs a look", lines: needs.map((record) => rowLine(repo, record, true)), empty: "No records need a look on this page." },
      ...groups.map((group) => ({
        title: group.key === AS_ASKED ? group.label + " · " + group.records.length : group.label,
        collapsed: group.key === AS_ASKED,
        lines: group.records.map((record) => rowLine(repo, record)),
        empty: "No records on this page.",
      })),
      ...(other.length ? [{ title: "Verdict unavailable", lines: other.map((record) => rowLine(repo, record)) }] : []),
      ...attentionGroups.map(({ title, rows, empty }) => ({
        title, lines: rows.map((item) => <><AttentionText item={item} repo={repo} />{item.work ? <> · <Link className="link" href={safetyHref(repo, item.work)}>Safety record</Link></> : null}{item.answerable ? <> · <Answerable person={item.answerable} /></> : null}</>),
        empty: person ? empty : "Attention items are addressed to a person.",
      })),
      { title: "Spot check", lines: [
        ...(sampleHref ? [<Link key="sample" className="link" href={sampleHref}>Show me a sample of what merged without a person</Link>] : []),
        ...(sample === undefined ? [] : sample.length ? sample.map((record) => rowLine(repo, record)) : [sampleEmpty]),
      ], empty: "A person with repository write access can draw a sample." },
      { title: "Signals", lines: [<SignalLines key="signals" />] },
      ...(next ? [{ title: "records", lines: [nextLink(next)] }] : []),
    ];
    return <><SafetyLive repo={repo} /><StopNotice frozen={frozen} /><PageFacts page="repo-safety" title="Safety" groups={facts} tools={tools} /></>;
  }

  function rows(list: SafetyRecord[]) {
    return list.length ? <Rows>{list.map((record, i) => <li key={recordKey(record, i)} className="border-b border-line py-4 text-base">
      {rowLine(repo, record)}
    </li>)}</Rows> : <Quiet>No records on this page.</Quiet>;
  }

  return <>
    <SafetyLive repo={repo} />
    <StopNotice frozen={frozen} />
    <PageHeader title="Safety" lede={INTRO} />
    <div className="flex max-w-4xl flex-col gap-10">
      <Panel title="Needs a look" id="needs-a-look">
        {needs.length ? <Rows label="Work that needs a look">{needs.map((record, i) => <li key={recordKey(record, i)} className="space-y-3 border-b border-line py-5">
          <div className="text-lg font-light text-ink"><RecordLink repo={repo} record={record} /></div>
          <div className="flex flex-wrap items-center gap-x-4 gap-y-1 text-sm text-ink-2"><Verdict value={record.verdict} /><Answerable person={record.answerable} /></div>
          {record.findings.filter((f) => f.state === OPEN).map((finding) => <Finding key={finding.id} finding={finding} repo={repo} repoId={repoId} work={record.work} canDecide={canDecide(record, props)} />)}
        </li>)}</Rows> : <Quiet>No records need a look on this page.</Quiet>}
      </Panel>
      {groups.map((group) => <Panel key={group.key} title={group.label} id={group.key.toLowerCase()}>
        {group.key === AS_ASKED ? <Disclosure title={plural(group.records.length, "record")}>{rows(group.records)}</Disclosure> : rows(group.records)}
      </Panel>)}
      {other.length ? <Panel title="Verdict unavailable" id="verdict-unavailable">{rows(other)}</Panel> : null}
      {nextLink(next)}
      {attentionGroups.map(({ title, rows, empty }, i) => <Panel key={title} id={i === 0 ? "tripwires" : "concerns"} title={title}>
        {rows.length ? <Rows label={title}>{rows.map((item) => <li key={item.id} className="flex flex-col items-start gap-3 border-b border-line py-4 sm:flex-row sm:justify-between">
          <div className="min-w-0 space-y-2 text-base [overflow-wrap:anywhere]">
            <p><AttentionText item={item} repo={repo} /></p>
            <div className="flex flex-wrap items-center gap-x-4 gap-y-1 text-xs text-ink-2">
              {item.work ? <Link className="link inline-block min-h-6 pt-1" href={safetyHref(repo, item.work)}>Safety record</Link> : null}
              <Answerable person={item.answerable} />
            </div>
          </div>
          <AcknowledgeAction repo={{ fullName: repo }} id={item.id} />
        </li>)}</Rows> : <Quiet>{person ? empty : "Attention items are addressed to a person."}</Quiet>}
      </Panel>)}
      <Panel title="Spot check" id="spot-check">
        {sampleHref ? <Link href={sampleHref} prefetch={false} className={buttonClass("secondary", "sm", "h-auto min-h-[34px] whitespace-normal py-2 text-center")}>Show me a sample of what merged without a person</Link> : <Quiet>A person with repository write access can draw a sample.</Quiet>}
        {sample ? <div className="mt-4">
          <Quiet>Sample from the last 30 days.</Quiet>
          {sample.length ? <Rows label="Sampled work" className="mt-3">{sample.map((record, i) => <li key={recordKey(record, i)} className="space-y-3 border-b border-line py-4 text-base">
            <div>{rowLine(repo, record)}</div>
            {record.work && canDecide(record, props) ? <Disclosure title="Set the verdict yourself"><VerdictAction work={record.work} current={record.verdict} /></Disclosure> : null}
          </li>)}</Rows> : <Quiet className="mt-2">{sampleEmpty}</Quiet>}
        </div> : null}
      </Panel>
      <Panel title="Signals" id="safety-signals" bodyClassName="space-y-2 text-sm text-ink-2"><SignalLines /></Panel>
    </div>
  </>;
}

