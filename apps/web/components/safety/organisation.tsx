import Link from "next/link";
import type { ReactNode } from "react";
import { PageFacts, type FactGroup } from "@/components/bee/page-facts";
import { Quiet, Rows } from "@/components/parts";
import { PageHeader, Panel } from "@/components/ui";
import type { SafetySummary } from "@/lib/api/types";
import { duration } from "@/lib/format";
import type { Mode } from "@/lib/mode";
import { toolFor } from "@/lib/tools";
import { StopAction } from "./actions";
import { AttentionText } from "./evidence";
import { INTRO, RESUME_DESCRIPTION, STOP_DESCRIPTION, SWARM, safetyHref, signalName } from "./model";
import { RAISE_CONCERN, SafetyLive, StopNotice } from "./repository";

type OrganisationProps = {
  org: string;
  mode: Mode;
  frozen?: boolean;
  canStop: boolean;
  summary?: SafetySummary;
  summaryMissing?: string;
  agents: Record<string, string>;
  repos: Record<string, string>;
  tabs?: ReactNode;
};

const WINDOW = "Findings and tripwires from the last 30 days.";
const WAIT_LABEL = "Median time to first human resolution";

export function OrganisationSafety({ org, mode, frozen, canStop, summary, summaryMissing, agents, repos, tabs }: OrganisationProps) {
  const wait = summary?.medianWaitMs;
  const waitText = wait === undefined ? "No resolved findings to time in this period." : Number(wait) < 1000 ? wait + " ms" : duration(Number(wait));
  const signals = (summary?.signals ?? []).map((signal) => ({
    key: signal.kind, label: <code>{signalName(signal.kind)}</code>, value: <span className="num">{signal.count}</span>,
  }));
  const work = (summary?.work ?? []).map((row) => ({
    key: row.kind + "/" + row.id,
    label: row.kind === SWARM ? <>Swarm <code>{row.id}</code></> : <>Agent {agents[row.id] || <code>{row.id}</code>}</>,
    value: row.signals.map((signal) => <span key={signal.kind} className="inline-block"><code>{signalName(signal.kind)}</code> <span className="num">{signal.count}</span></span>),
  }));
  const wires = (summary?.tripwires ?? []).map((item) => {
    const fullName = item.work?.repo.fullName || (item.work?.repo.id ? repos[item.work.repo.id] : undefined);
    return { id: item.id, line: <>
      {fullName && item.work ? <Link href={safetyHref(fullName, item.work)} className="link mr-2 inline-block min-h-6">{fullName}</Link> : null}
      <AttentionText item={item} repo={fullName} />
    </> };
  });
  const top = <><SafetyLive repo={org + "/*"} /><StopNotice frozen={frozen} /></>;
  const stopText = frozen ? RESUME_DESCRIPTION : STOP_DESCRIPTION;
  const missing = summaryMissing || "The organisation summary could not be read.";

  if (mode === "bee") {
    const groups: FactGroup[] = [
      { title: "evidence", lines: [INTRO, WINDOW] },
      ...(summary ? [
        { title: "By signal", facts: signals.map((r) => ({ label: signalName(r.key), value: r.value })), empty: "No findings in this period." },
        { title: "By agent and swarm", lines: work.map((r) => <span key={r.key}>{r.label} · {r.value.map((v) => <span key={v.key} className="mr-3">{v}</span>)}</span>), empty: "No findings in this period." },
        { title: "Tripwires", lines: wires.map(({ line, id }) => <span key={id}>{line}</span>), empty: "No tripwires fired in this period." },
        { title: "Finding wait", facts: [{ label: WAIT_LABEL, value: waitText }] },
      ] : [{ title: "Summary", lines: [missing] }]),
      { title: "Organisation controls", lines: [stopText] },
    ];
    return <>{top}<PageFacts page="org-safety" title={org + " · Safety"} groups={groups} tools={[
      toolFor("SafetyService/GetSafetySummary", "Read the findings and tripwires for a period"),
      RAISE_CONCERN,
      toolFor("SafetyService/StopEverything", STOP_DESCRIPTION),
      toolFor("SafetyService/ResumeEverything", RESUME_DESCRIPTION),
    ]} /></>;
  }

  return <>
    {top}
    <PageHeader title="Safety" eyebrow={org} lede={INTRO} />
    {tabs}
    <div className="flex max-w-4xl flex-col gap-10">
      <div><Quiet>{WINDOW}</Quiet></div>
      {summary ? <>
        <Panel title="By signal" id="safety-by-signal">
          {signals.length ? <dl className="border-t border-line">{signals.map((row) => <div key={row.key} className="flex items-baseline justify-between gap-4 border-b border-line py-3 text-base">
            <dt>{row.label}</dt><dd>{row.value}</dd>
          </div>)}</dl> : <Quiet>No findings in this period.</Quiet>}
        </Panel>
        <Panel title="By agent and swarm" id="safety-by-work">
          {work.length ? <dl className="border-t border-line">{work.map((row) => <div key={row.key} className="grid gap-1 border-b border-line py-3 text-base [overflow-wrap:anywhere] sm:grid-cols-2">
            <dt>{row.label}</dt><dd className="flex flex-wrap gap-x-4 gap-y-1 text-ink-2">{row.value}</dd>
          </div>)}</dl> : <Quiet>No findings in this period.</Quiet>}
        </Panel>
        <Panel title="Tripwires" id="safety-org-tripwires">
          {wires.length ? <Rows>{wires.map(({ id, line }) => <li key={id} className="border-b border-line py-4 text-base [overflow-wrap:anywhere]">{line}</li>)}</Rows> : <Quiet>No tripwires fired in this period.</Quiet>}
        </Panel>
        <Panel title="Finding wait" id="safety-finding-wait">
          <dl className="flex flex-wrap items-baseline gap-x-4 gap-y-1 text-base"><dt>{WAIT_LABEL}</dt><dd className="num text-ink-2">{waitText}</dd></dl>
        </Panel>
      </> : <Quiet>{missing}</Quiet>}
      <Panel title="Organisation controls" id="safety-org-controls" bodyClassName="space-y-3">
        {canStop ? <><Quiet>{stopText}</Quiet><StopAction key={String(frozen)} org={org} frozen={!!frozen} /></>
          : <Quiet>Only an organisation owner can stop or resume agent work.</Quiet>}
      </Panel>
    </div>
  </>;
}
