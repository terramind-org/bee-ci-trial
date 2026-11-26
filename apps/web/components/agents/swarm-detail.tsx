import Link from "next/link";
import type { ReactNode } from "react";
import { PageFacts } from "@/components/bee/page-facts";
import { Mark, Quiet, Sect, State, type Shape } from "@/components/parts";
import { PageHeader } from "@/components/ui";
import { pathText } from "@/lib/api/ids";
import type { ChangeRequest, Principal, Swarm, SwarmAttemptOutcome, SwarmJoin, SwarmJoinState, SwarmRef, SwarmTask, SwarmTaskState } from "@/lib/api/types";
import { usd } from "@/lib/format";
import type { Mode } from "@/lib/mode";
import { repoHref } from "@/lib/routes";
import { toolFor } from "@/lib/tools";
import { RefreshWork } from "./refresh-work";
import { SwarmJoinDecision, SwarmJoinForm, SwarmMessageForm, SwarmStateControls, type JoinAgent } from "./swarm-actions";
import { acceptedJoins, budgetText, pullState, TaskCells, taskMotionKey, WorkFacts, swarmCells, swarmState, workAnchor, workMotionSnapshot } from "./work";
import { WorkCue, WorkMotion } from "./work-motion";

export interface SwarmMember {
  runId: string;
  agent?: { id?: string; display?: string; runtime?: string; ownerId?: string };
  person?: Principal;
  reason?: string;
}

interface Props {
  repo: string;
  target: SwarmRef;
  swarm: Swarm;
  mode: Mode;
  members: SwarmMember[];
  changes: ChangeRequest[];
  canChangeState: boolean;
  canDecideJoins: boolean;
  canSpeak: boolean;
  join?: { agents: JoinAgent[]; allowAgentId: boolean };
  viewerId?: string;
}

type LabelledState = { label: string; shape: Shape };

const TASK_STATE: Record<SwarmTaskState, LabelledState> = {
  SWARM_TASK_STATE_UNSPECIFIED: { label: "Unknown", shape: "open" },
  SWARM_TASK_STATE_OPEN: { label: "Open", shape: "open" },
  SWARM_TASK_STATE_WORKING: { label: "Being worked on", shape: "running" },
  SWARM_TASK_STATE_FINISHED: { label: "Finished", shape: "passed" },
  SWARM_TASK_STATE_EMPTY: { label: "Came back empty", shape: "closed" },
};

const ATTEMPT_STATE: Record<SwarmAttemptOutcome, LabelledState> = {
  SWARM_ATTEMPT_OUTCOME_UNSPECIFIED: { label: "In progress", shape: "running" },
  SWARM_ATTEMPT_OUTCOME_FINISHED: { label: "Finished", shape: "passed" },
  SWARM_ATTEMPT_OUTCOME_EMPTY: { label: "Came back empty", shape: "closed" },
  SWARM_ATTEMPT_OUTCOME_REFUSED: { label: "Refused", shape: "failed" },
};

const JOIN_STATE: Record<SwarmJoinState, LabelledState> = {
  SWARM_JOIN_STATE_UNSPECIFIED: { label: "Unknown", shape: "open" },
  SWARM_JOIN_STATE_PENDING: { label: "Pending", shape: "waiting" },
  SWARM_JOIN_STATE_ACCEPTED: { label: "Accepted", shape: "passed" },
  SWARM_JOIN_STATE_DECLINED: { label: "Declined", shape: "closed" },
  SWARM_JOIN_STATE_REFUSED: { label: "Refused", shape: "failed" },
};

export function swarmIsActive(swarm: Swarm): boolean {
  return swarm.state === "SWARM_STATE_PLANNING" || swarm.state === "SWARM_STATE_WORKING" || swarm.state === "SWARM_STATE_PAUSED";
}

function nameOf(p: { id?: string; display?: string } | undefined): string {
  return p?.display || p?.id || "";
}

function AgentName({ agent, repo }: { agent: SwarmMember["agent"]; repo: string }) {
  const label = nameOf(agent);
  if (!label) return null;
  return (
    <span className="inline-flex flex-wrap items-center gap-x-2">
      {agent?.id ? <Link className="link inline-flex min-h-6 items-center" href={repoHref(repo, "agents/" + encodeURIComponent(agent.id))}>{label}</Link> : label}
      {agent?.runtime ? <span className="text-sm text-ink-2">{agent.runtime}</span> : null}
    </span>
  );
}

function TaskDetail({ task, member, repo, mode, row }: { task: SwarmTask; member?: SwarmMember; repo: string; mode: Mode; row: string }) {
  const taskState = TASK_STATE[task.state];
  const current = member?.agent ?? task.attempts.findLast((attempt) => attempt.runId === task.runId)?.agent;
  const Title = mode === "keeper" ? "h3" : "p";
  return (
    <div className="min-w-0 [overflow-wrap:anywhere]">
      <Title className={mode === "keeper" ? "text-lg font-light text-ink" : "font-medium text-ink"}>{task.title}</Title>
      <p className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-0.5 text-ink-2">
        <State shape={taskState.shape}>{taskState.label}</State>
        {task.state === "SWARM_TASK_STATE_WORKING" ? (
          <WorkCue piece="claim" watch={taskMotionKey(row, task.id)} text={"Work started on " + task.title + "."} />
        ) : null}
        {mode === "bee" && task.state === "SWARM_TASK_STATE_FINISHED" ? (
          <span className="relative size-4 shrink-0">
            <WorkCue piece="cap" watch={taskMotionKey(row, task.id)} text={task.title + " finished."} />
          </span>
        ) : null}
        {task.runId ? (
          <span>Assigned to {current ? <AgentName agent={current} repo={repo} /> : <Link className="link inline-flex min-h-6 items-center" href={repoHref(repo, "agents/runs/" + encodeURIComponent(task.runId))}><code>{task.runId}</code></Link>}</span>
        ) : null}
      </p>
      {task.paths.length ? <p className="mt-1 text-sm text-ink-2">Claimed paths <code className="mono text-ink">{task.paths.map(pathText).join(", ")}</code></p> : null}
      {task.attempts.length ? (
        <div className="mt-3">
          <p className="text-xs font-medium text-ink-2">Attempts</p>
          <ol className="mt-1 flex flex-col gap-2 text-sm">
            {task.attempts.map((attempt, index) => {
              const status = ATTEMPT_STATE[attempt.outcome];
              const report = attempt.handBack;
              const fields = report ? ([
                ["Done", report.done], ["Deviations", report.deviations], ["Concerns", report.concerns], ["Follow-ups", report.followUps],
              ] as const).filter(([, entries]) => entries.length) : [];
              return (
                <li key={attempt.runId + "/" + index}>
                  <p className="flex flex-wrap items-center gap-x-2 text-ink-2">
                    <State shape={status.shape}>{status.label}</State>
                    <AgentName agent={attempt.agent} repo={repo} />
                  </p>
                  {attempt.reason ? <p className="mt-1 text-ink-2">{attempt.reason}</p> : null}
                  {fields.length ? (
                    <div className="mt-2 border-l border-line pl-3">
                      <p className="text-xs font-medium text-ink-2">Hand-back</p>
                      <dl className="mt-1 grid grid-cols-1 gap-x-3 gap-y-1 sm:grid-cols-[minmax(0,7rem)_minmax(0,1fr)]">
                        {fields.map(([label, entries]) => (
                          <div key={label} className="contents">
                            <dt className="text-ink-2">{label}</dt>
                            <dd>{entries.length === 1 ? entries[0] : <ul className="list-disc space-y-1 pl-4">{entries.map((text, entry) => <li key={entry}>{text}</li>)}</ul>}</dd>
                          </div>
                        ))}
                      </dl>
                    </div>
                  ) : null}
                </li>
              );
            })}
          </ol>
        </div>
      ) : null}
    </div>
  );
}

function MemberLine({ member, leadRunId, repo }: { member: SwarmMember; leadRunId: string; repo: string }) {
  return (
    <span className="flex flex-wrap items-center gap-x-3 gap-y-1 [overflow-wrap:anywhere]">
      {member.runId === leadRunId ? <span className="text-sm text-ink-2">Queen · lead</span> : null}
      {nameOf(member.agent) ? <AgentName agent={member.agent} repo={repo} /> : <code className="mono text-sm">{member.runId}</code>}
      {member.person ? <Mark>{nameOf(member.person)}</Mark> : member.agent?.ownerId ? <Mark><code className="mono text-sm">{member.agent.ownerId}</code></Mark> : null}
      {member.reason ? <span className="text-sm text-ink-2">Run details unavailable <code>{member.reason}</code></span> : (
        <Link className="link inline-flex min-h-6 items-center text-sm" href={repoHref(repo, "agents/runs/" + encodeURIComponent(member.runId))}>View run</Link>
      )}
    </span>
  );
}

function JoinLine({ request, children }: { request: SwarmJoin; children?: ReactNode }) {
  const status = JOIN_STATE[request.state];
  const limit = request.budget?.maxUsdMicros;
  return (
    <div className="flex flex-wrap items-center justify-between gap-3">
      <div className="min-w-0 [overflow-wrap:anywhere]">
        <p>{nameOf(request.agent) || "Agent"}{nameOf(request.requester) ? <span className="text-ink-2"> offered by {nameOf(request.requester)}</span> : null}</p>
        <p className="mt-1 flex flex-wrap gap-x-3 gap-y-1 text-sm text-ink-2">
          <State shape={status.shape}>{status.label}</State>
          {limit ? <span>{usd(limit)} spending limit</span> : null}
        </p>
        {request.reason ? <p className="mt-1 text-sm text-ink-2">{request.reason}</p> : null}
      </div>
      {children}
    </div>
  );
}

function swarmTools(active: boolean, nextState: Swarm["state"]) {
  const tools = [
    toolFor("SwarmService/GetSwarm", "Read the goal, tasks, attempts and hand-backs"),
    toolFor("AgentService/GetAgentRun", "Read a member’s run"),
    toolFor("ChangeService/GetChange", "Read a linked pull request"),
    toolFor("ChangeService/GetChangeByNumber", "Read a pull request reported by a live run"),
    toolFor("EventService/Subscribe", "Follow changes to this swarm's work"),
  ];
  if (active) {
    tools.push(
      toolFor("SwarmService/RequestSwarmJoin", "Offer an agent you own with its spending limit"),
      toolFor("SwarmService/SendSwarmMessage", "Send a message to the lead only (people with write access)"),
      toolFor("SwarmService/DecideSwarmJoin", "Accept or decline a request to join"),
      toolFor("SwarmService/SetSwarmState", nextState === "SWARM_STATE_WORKING" ? "Pause or stop the swarm" : nextState === "SWARM_STATE_PAUSED" ? "Resume or stop the swarm" : "Stop the swarm", { state: "SWARM_STATE_STOPPED" }),
    );
  }
  return tools;
}

export function SwarmDetail({ repo, target, swarm, mode, members, changes, canChangeState, canDecideJoins, canSpeak, join, viewerId }: Props) {
  const state = swarmState(swarm);
  const tasks = swarmCells(swarm);
  const active = swarmIsActive(swarm);
  const issue = swarm.issue ? <Link href={repoHref(repo, "work/" + encodeURIComponent(swarm.issue))} className="link inline-flex min-h-6 items-center">Issue #{swarm.issue}</Link> : null;
  const crumb = <Link href={repoHref(repo, "agents")} className="link inline-flex min-h-6 items-center">Agents</Link>;
  const facts = <WorkFacts repo={repo} branch={swarm.branch} changes={changes} owner={swarm.owner} cost={swarm.cost} budget={swarm.budget} />;
  const item = { swarm, changeRequestIds: swarm.changeRequestIds };
  const row = workAnchor(item);
  const scope = row + ":" + mode + ":" + (viewerId ?? "");
  const ids = [swarm.id, ...swarm.memberRunIds];
  const snapshot = workMotionSnapshot([item], viewerId);
  const paused = swarm.state === "SWARM_STATE_PAUSED" ? <WorkCue piece="pause" watch={row + "/pause"} text={swarm.goal + " was paused."} /> : null;
  const joined = acceptedJoins(swarm, viewerId).length ? (
    <span className="inline-flex items-center gap-2">
      <WorkCue piece="join" watch={row + "/join"} text="Your request to join was accepted." />
      Your request to join was accepted.
    </span>
  ) : null;
  const taskLines = swarm.tasks.map((task) => <TaskDetail key={task.id} task={task} member={members.find((member) => member.runId === task.runId)} repo={repo} mode={mode} row={row} />);
  const memberLines = members.map((member) => <MemberLine key={member.runId} member={member} leadRunId={swarm.leadRunId} repo={repo} />);

  if (mode === "bee") {
    const spend = budgetText(swarm.cost, swarm.budget);
    const accountable = nameOf(swarm.owner);
    return (
      <WorkMotion repo={repo} scope={scope} snapshot={snapshot} ids={ids}>
        <div className="mb-4 flex flex-wrap items-center justify-between gap-3">{crumb}<RefreshWork /></div>
        <PageFacts
          page="swarm"
          title={swarm.goal}
          groups={[
            { title: "swarm", facts: [
              { label: "name", value: swarm.name },
              { label: "state", value: <span className="inline-flex items-center gap-2">{state.label}{paused}</span> },
              joined ? { label: "join request", value: joined } : null,
              swarm.status ? { label: "lead status", value: swarm.status } : null,
              swarm.branch ? { label: "branch", value: <code>{swarm.branch}</code> } : null,
              spend ? { label: "cost and budget", value: spend } : null,
              accountable ? { label: "answers for it", value: accountable } : null,
              issue ? { label: "issue", value: issue } : null,
            ] },
            ...(changes.length ? [{
              title: "pull requests",
              lines: changes.map((change) => <span key={change.id}><Link href={repoHref(repo, "changes/" + encodeURIComponent(change.id))} className="link inline-flex min-h-6 items-center">#{change.number} {change.title}</Link> · {pullState(change).label}</span>),
            }] : []),
            { title: "tasks", facts: swarm.tasks.map((task, index) => ({ label: "Task " + (index + 1), value: taskLines[index] })), empty: "No tasks yet." },
            { title: "members", lines: memberLines },
            ...(swarm.joins.length ? [{ title: "join requests", facts: swarm.joins.map((request, index) => ({ label: "Request " + (index + 1), value: <JoinLine key={request.id} request={request} /> })) }] : []),
          ]}
          tools={swarmTools(active, swarm.state)}
        />
      </WorkMotion>
    );
  }

  return (
    <WorkMotion repo={repo} scope={scope} snapshot={snapshot} ids={ids}>
      <PageHeader
        eyebrow={<span className="inline-flex flex-wrap items-center gap-2">{crumb}<span aria-hidden="true">/</span><span>Swarm · {swarm.name}</span></span>}
        title={swarm.goal}
        lede={swarm.status || undefined}
        meta={<><State shape={state.shape}>{state.label}</State>{paused}{joined}{facts}{issue}</>}
        actions={<><RefreshWork />{canChangeState ? <SwarmStateControls swarm={target} state={swarm.state} /> : null}</>}
      />
      <div className="flex max-w-[920px] flex-col gap-10">
        <Sect id="swarm-tasks" title="Tasks">
          {tasks.length ? (
            <>
              <TaskCells cells={tasks} large row={row} />
              <ol className="mt-4 divide-y divide-line">
                {taskLines.map((line) => <li key={line.key} className="py-4">{line}</li>)}
              </ol>
            </>
          ) : <Quiet>No tasks yet.</Quiet>}
        </Sect>
        <Sect id="swarm-members" title="Members">
          <ul className="divide-y divide-line">{memberLines.map((line) => <li key={line.key} className="py-2">{line}</li>)}</ul>
        </Sect>
        {swarm.joins.length ? (
          <Sect id="swarm-join-requests" title="Join requests">
            <ul className="divide-y divide-line">
              {swarm.joins.map((request) => (
                <li key={request.id} className="py-3">
                  <JoinLine request={request}>
                    {canDecideJoins && request.state === "SWARM_JOIN_STATE_PENDING" ? (
                      <SwarmJoinDecision swarm={target} joinId={request.id} applicant={nameOf(request.requester) || nameOf(request.agent) || "this applicant"} />
                    ) : null}
                  </JoinLine>
                </li>
              ))}
            </ul>
          </Sect>
        ) : null}
        {canSpeak ? <Sect id="swarm-speak" title="Speak to the lead"><SwarmMessageForm swarm={target} /></Sect> : null}
        {join ? <Sect id="swarm-join" title="Join"><SwarmJoinForm swarm={target} agents={join.agents} allowAgentId={join.allowAgentId} /></Sect> : null}
      </div>
    </WorkMotion>
  );
}
