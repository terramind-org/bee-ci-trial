import Link from "next/link";
import { PageFacts } from "@/components/bee/page-facts";
import { ButtonLink } from "@/components/kit";
import { Markdown } from "@/components/markdown";
import { Kv, Mark, Sect, State } from "@/components/parts";
import { Time } from "@/components/time";
import { PageHeader } from "@/components/ui";
import type { AgentRun, AgentRunEvent, ChangeRequest, Swarm } from "@/lib/api/types";
import type { Mode } from "@/lib/mode";
import { repoHref } from "@/lib/routes";
import { pageTools, toolFor } from "@/lib/tools";
import { RefreshWork } from "./refresh-work";
import { AgentRunActions, RunMessageForm } from "./run-actions";
import { budgetText, cellLabels, finishedRun, pullState, runCells, runGoal, runState, TaskCells, taskMotionKey, workAnchor, workMotionSnapshot } from "./work";
import { WorkCue, WorkMotion } from "./work-motion";

const EVENT_LABEL: Record<NonNullable<AgentRunEvent["kind"]>, string> = {
  AGENT_RUN_EVENT_KIND_UNSPECIFIED: "Session event",
  AGENT_RUN_EVENT_KIND_STATE_CHANGED: "State changed",
  AGENT_RUN_EVENT_KIND_LOG: "Log",
  AGENT_RUN_EVENT_KIND_THOUGHT: "Runtime note",
  AGENT_RUN_EVENT_KIND_TOOL_CALL: "Tool call",
  AGENT_RUN_EVENT_KIND_TOOL_RESULT: "Tool result",
  AGENT_RUN_EVENT_KIND_CHANGESET_CREATED: "Commit created",
  AGENT_RUN_EVENT_KIND_CHANGE_REQUEST_OPENED: "Pull request opened",
  AGENT_RUN_EVENT_KIND_LEASE_ACQUIRED: "File claim acquired",
  AGENT_RUN_EVENT_KIND_CHECK_RESULT: "Check reported",
  AGENT_RUN_EVENT_KIND_STEERING_MESSAGE: "Message",
  AGENT_RUN_EVENT_KIND_COST_UPDATE: "Cost update",
  AGENT_RUN_EVENT_KIND_ERROR: "Error",
};

function eventText(event: AgentRunEvent) {
  return [event.author?.display, event.kind === "AGENT_RUN_EVENT_KIND_STATE_CHANGED" && runState({ state: event.state }).label, event.text, event.toolName, event.toolPayload, event.subject?.id, budgetText(event.cost)].filter(Boolean).join(" · ");
}

function EventRows({ events }: { events: AgentRunEvent[] }) {
  return (
    <ol className="divide-y divide-line">
      {events.map((event) => (
        <li key={event.seq} className="py-3 first:pt-0">
          <p className="flex flex-wrap gap-x-2 text-sm text-ink-2">
            <span className="font-medium text-ink">{EVENT_LABEL[event.kind ?? "AGENT_RUN_EVENT_KIND_UNSPECIFIED"]}</span>
            <Time iso={event.time} />
          </p>
          <p className="mt-1 whitespace-pre-wrap [overflow-wrap:anywhere]">{eventText(event)}</p>
        </li>
      ))}
    </ol>
  );
}

export function AgentRunDetails({ repo, mode, run, swarm, owner, changes, events, messages, eventsToken, nextEvents, nextMessages, messagesToken, canManage }: {
  repo: string;
  mode: Mode;
  run: AgentRun;
  swarm?: Swarm;
  owner?: string;
  changes: ChangeRequest[];
  events: AgentRunEvent[];
  messages: AgentRunEvent[];
  eventsToken: string;
  nextEvents?: string;
  messagesToken: string;
  nextMessages?: string;
  canManage: boolean;
}) {
  const goal = runGoal(run);
  const state = runState(run);
  const cells = runCells(run);
  const finished = finishedRun(run);
  const failureReason = run.state === "AGENT_RUN_STATE_FAILED" ? run.failureReason : undefined;
  const spend = budgetText(run.cost, run.budget);
  const agent = run.agent?.display || run.agent?.id;
  const profile = run.agent?.id && repoHref(repo, "agents/" + encodeURIComponent(run.agent.id));
  const swarmLink = swarm && <Link className="link inline-flex min-h-6 items-center" href={repoHref(repo, "agents/swarms/" + encodeURIComponent(swarm.id))}>{swarm.name || swarm.goal}</Link>;
  const runId = run.id ?? "";
  const item = { run, changeRequestIds: [] };
  const row = workAnchor(item);
  const snapshot = workMotionSnapshot([item]);
  const scope = row + ":" + mode;
  const taskKey = taskMotionKey(row, undefined);
  const claiming = state.cell === "working" ? (
    <WorkCue piece="claim" watch={taskKey} text={"Work started on " + goal + "."} />
  ) : null;
  const runHref = repoHref(repo, "agents/runs/" + encodeURIComponent(runId));
  const pageHref = (eventsPage: string, messagesPage: string, anchor: string) => {
    const params = new URLSearchParams();
    if (mode === "bee") params.set("mode", "bee");
    if (eventsPage) params.set("events", eventsPage);
    if (messagesPage) params.set("messages", messagesPage);
    const query = params.toString();
    return runHref + (query ? "?" + query : "") + (anchor ? "#" + anchor : "");
  };
  const pager = (section: "events" | "messages", token: string, next?: string) => (
    <div className="mt-3 flex flex-wrap gap-2">
      {token ? <ButtonLink size="sm" href={pageHref(section === "events" ? "" : eventsToken, section === "messages" ? "" : messagesToken, mode === "bee" ? "" : section)}>First {section}</ButtonLink> : null}
      {next ? <ButtonLink size="sm" href={pageHref(section === "events" ? next : eventsToken, section === "messages" ? next : messagesToken, mode === "bee" ? "" : section)}>More {section}</ButtonLink> : null}
    </div>
  );
  const pullRequests = changes.map((change) => {
    const pull = pullState(change);
    return (
      <span key={change.id} className="inline-flex flex-wrap items-center gap-2">
        <Link href={repoHref(repo, "changes/" + encodeURIComponent(change.id))} className="link inline-flex min-h-6 items-center">Pull request #{change.number}</Link>
        <State shape={pull.shape}>{pull.label}</State>
      </span>
    );
  });
  const agentLink = agent && profile ? <Link className="link inline-flex min-h-6 items-center" href={profile}>{agent}</Link> : agent;
  const facts = [
    { label: "State", value: state.label },
    agent ? { label: "Agent", value: agentLink } : null,
    run.agent?.runtime ? { label: "Runtime", value: run.agent.runtime } : null,
    swarmLink ? { label: "Swarm", value: swarmLink } : null,
    owner ? { label: "Answers for it", value: owner } : null,
    run.baseRef ? { label: "Branch", value: run.baseRef } : null,
    run.workRef ? { label: "Workspace", value: run.workRef } : null,
    run.session ? { label: "Session", value: run.session } : null,
    run.model ? { label: "Model", value: run.model } : null,
    spend ? { label: "Cost and budget", value: spend } : null,
    run.startedAt ? { label: "Started", value: <Time iso={run.startedAt} /> } : null,
    run.finishedAt ? { label: "Ended", value: <Time iso={run.finishedAt} /> } : null,
    failureReason ? { label: "Failure reason", value: failureReason } : null,
  ];

  if (mode === "bee") {
    return (
      <WorkMotion repo={repo} scope={scope} snapshot={snapshot} ids={[runId]}>
        <div className="mb-4 flex flex-wrap items-center gap-3">
          <Link href={repoHref(repo, "agents")} className="link">Agents</Link>
          <RefreshWork />
        </div>
        <PageFacts page="agent-run" title={goal} tools={[...pageTools("agent-run"), toolFor("EventService/Subscribe", "Follow changes to this run")].filter((tool) => {
          if (tool.op === "AgentService/RetryAgentRun") return canManage && finished;
          if (tool.op === "AgentService/CancelAgentRun" || tool.op === "AgentService/SendAgentRunMessage") return canManage && !finished;
          return true;
        })} groups={[
          { title: "run", facts, lines: pullRequests },
          { title: "task", lines: [
            <span key="task" className="flex items-center gap-2">
              {goal + " · " + cellLabels[state.cell]}{claiming}
              {state.cell === "finished" ? (
                <span className="relative size-4 shrink-0"><WorkCue piece="cap" watch={taskKey} text={goal + " finished."} /></span>
              ) : null}
            </span>,
          ] },
          ...(run.summary ? [{ title: "what it reported", facts: [{ label: "Summary", value: <div className="whitespace-pre-wrap">{run.summary}</div> }] }] : []),
          { title: "messages", lines: messages.map((event) => [event.time, eventText(event)].filter(Boolean).join(" · ")) },
          { title: "logs and events", lines: events.map((event) => [event.time, EVENT_LABEL[event.kind ?? "AGENT_RUN_EVENT_KIND_UNSPECIFIED"], eventText(event)].filter(Boolean).join(" · ")) },
        ]} />
        {messagesToken || nextMessages ? pager("messages", messagesToken, nextMessages) : null}
        {eventsToken || nextEvents ? pager("events", eventsToken, nextEvents) : null}
      </WorkMotion>
    );
  }

  return (
    <WorkMotion repo={repo} scope={scope} snapshot={snapshot} ids={[runId]}>
      <PageHeader
        title={goal}
        eyebrow={<Link href={repoHref(repo, "agents")} className="link inline-flex min-h-6 items-center">Agents</Link>}
        meta={<><State shape={state.shape}>{state.label}</State>{swarmLink ?? <span>One agent</span>}{agentLink}{run.agent?.runtime ? <span>{run.agent.runtime}</span> : null}{owner ? <Mark>{owner}</Mark> : null}</>}
        actions={<><RefreshWork />{canManage ? <AgentRunActions repo={repo} runId={runId} finished={finished} /> : null}</>}
      />
      <div className="flex max-w-[920px] flex-col gap-10">
        <Sect id="run-task" title="Task">
          <div className="flex items-center gap-2"><TaskCells cells={cells} row={row} large />{claiming}</div>
        </Sect>
        {run.summary ? (
          <Sect id="run-report" title="What it reported">
            <Markdown source={run.summary} repo={repo} headingOffset={2} />
          </Sect>
        ) : null}
        <Sect id="run-details" title="Run details">
          <Kv rows={[
            failureReason ? ["Failure reason", failureReason] : null,
            run.baseRef ? ["Branch", <code key="branch" className="mono">{run.baseRef}</code>] : null,
            run.workRef ? ["Workspace", <code key="workspace" className="mono">{run.workRef}</code>] : null,
            run.session ? ["Session", <code key="session" className="mono">{run.session}</code>] : null,
            run.model ? ["Model", run.model] : null,
            spend ? ["Cost and budget", spend] : null,
            run.startedAt ? ["Started", <Time key="start" iso={run.startedAt} />] : null,
            run.finishedAt ? ["Ended", <Time key="end" iso={run.finishedAt} />] : null,
            pullRequests.length ? ["Pull requests", pullRequests] : null,
          ]} />
        </Sect>
        <Sect id="messages" title="Messages">
          {messages.length ? <EventRows events={messages} /> : <p className="text-sm text-ink-2">No messages yet.</p>}
          {messagesToken || nextMessages ? pager("messages", messagesToken, nextMessages) : null}
          {canManage && !finished ? <div className="mt-4"><RunMessageForm runId={runId} /></div> : null}
        </Sect>
        <Sect id="events" title="Logs and events">
          {events.length ? <EventRows events={events} /> : <p className="text-sm text-ink-2">No session events yet.</p>}
          {eventsToken || nextEvents ? pager("events", eventsToken, nextEvents) : null}
        </Sect>
      </div>
    </WorkMotion>
  );
}
