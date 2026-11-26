import Link from "next/link";
import { PageFacts } from "@/components/bee/page-facts";
import { QuietRepository } from "@/components/bees/pieces";
import { Rows, Sep, State } from "@/components/parts";
import { PageHeader } from "@/components/ui";
import type { AgentSummary, ChangeRequest, RepoWorkItem } from "@/lib/api/types";
import type { Mode } from "@/lib/mode";
import { repoHref } from "@/lib/routes";
import { pageTools, toolFor } from "@/lib/tools";
import { RefreshWork } from "./refresh-work";
import { acceptedJoins, budgetText, cellLabels, finishedRun, pullState, runCells, runGoal, runState, swarmCells, swarmState, TaskCells, taskMotionKey, WorkFacts, workAnchor, workMotionSnapshot, workPath } from "./work";
import { FinishedWork, WorkCue, WorkMotion } from "./work-motion";

const WEEK = 7 * 24 * 60 * 60 * 1000;

export function groupWork(items: RepoWorkItem[], now: number) {
  const working: RepoWorkItem[] = [];
  const paused: RepoWorkItem[] = [];
  const finished: RepoWorkItem[] = [];
  for (const item of items) {
    const { swarm, run } = item;
    if (swarm?.state === "SWARM_STATE_PAUSED") {
      paused.push(item);
    } else if (swarm?.state === "SWARM_STATE_FINISHED" || swarm?.state === "SWARM_STATE_STOPPED" || (run && finishedRun(run))) {
      if (Date.parse(run?.finishedAt ?? item.activityAt ?? "") >= now - WEEK) finished.push(item);
    } else {
      working.push(item);
    }
  }
  return [
    { title: "Working", items: working, empty: "No work in progress." },
    { title: "Paused", items: paused, empty: "No paused work." },
    { title: "Finished this week", items: finished, collapsed: true },
  ];
}

function itemView(item: RepoWorkItem, changesById: Map<string, ChangeRequest>, changesByNumber: Map<string, ChangeRequest>, people: Map<string, NonNullable<AgentSummary["owner"]>>) {
  const { swarm, run } = item;
  const lead = swarm?.lead;
  const goal = swarm ? swarm.goal || swarm.name : runGoal(run!);
  const state = swarm ? swarmState(swarm) : runState(run!);
  const responsibleId = run?.agent?.ownerId;
  const owner = swarm?.owner ?? (responsibleId ? people.get(responsibleId) ?? { id: responsibleId } : run?.startedBy?.kind === "PRINCIPAL_KIND_HUMAN" ? run.startedBy : undefined);
  const linked = [...item.changeRequestIds.map((id) => changesById.get(id)), ...(run?.changeRequests ?? []).map((number) => changesByNumber.get(number))];
  const changes = [...new Map(linked.flatMap((change) => change ? [[change.id, change]] : [])).values()];
  const cells = swarm ? swarmCells(swarm) : runCells(run!);
  const size = swarm ? "Swarm · " + swarm.memberRunIds.length + (swarm.memberRunIds.length === 1 ? " agent" : " agents") : "One agent";
  const branch = swarm?.branch || run?.baseRef;
  const cost = swarm?.cost ?? run?.cost;
  const budget = swarm?.budget ?? run?.budget;
  return { lead, goal, state, owner, changes, cells, size, branch, cost, budget, status: swarm?.status || (run?.state === "AGENT_RUN_STATE_FAILED" ? run.failureReason : undefined) };
}

function DirectoryLinks({ repo, agents }: { repo: string; agents: AgentSummary[] }) {
  return (
    <ul className="divide-y divide-line">
      {agents.map(({ principal, owner }) => (
        <li key={principal.id} className="flex flex-wrap items-baseline gap-x-2 py-2.5 text-sm text-ink-2">
          <Link href={repoHref(repo, "agents/" + encodeURIComponent(principal.id))} className="link inline-flex min-h-6 items-center text-ink">
            {principal.display || principal.id}
          </Link>
          {principal.runtime ? <><Sep />{principal.runtime}</> : null}
          {owner?.display ? <><Sep />Answers to {owner.display}</> : null}
        </li>
      ))}
    </ul>
  );
}

export function AgentsWorkList({ repo, mode, items, agents, changes, now, viewerId }: {
  repo: string;
  mode: Mode;
  items: RepoWorkItem[];
  agents: AgentSummary[];
  changes: ChangeRequest[];
  now: number;
  viewerId?: string;
}) {
  const directory = new Map(agents.flatMap(({ owner }) => owner ? [owner] : []).map((person) => [person.id, person]));
  const changesById = new Map(changes.map((change) => [change.id, change]));
  const changesByNumber = new Map(changes.map((change) => [change.number, change]));
  const groups = groupWork(items, now).map((group) => ({
    ...group,
    rows: group.items.map((item) => ({
      ...itemView(item, changesById, changesByNumber, directory),
      href: repoHref(repo, workPath(item)),
      anchor: workAnchor(item),
      runAgent: item.run?.agent,
      paused: item.swarm?.state === "SWARM_STATE_PAUSED",
      joined: acceptedJoins(item.swarm, viewerId).length > 0,
    })),
  }));
  const snapshot = workMotionSnapshot(items, viewerId);
  const scope = "agents:" + mode + ":" + (viewerId ?? "");

  if (mode === "bee") {
    return (
      <WorkMotion repo={repo} scope={scope} snapshot={snapshot}>
        <div className="mb-4"><RefreshWork /></div>
        <PageFacts page="agents" title={repo + " · agents"} tools={[
          ...pageTools("agents"), toolFor("EventService/Subscribe", "Follow changes to repository work"),
        ]} groups={[
          ...groups.map(({ title, empty, collapsed, rows }) => ({
            title: collapsed ? title + " · " + rows.length : title,
            collapsed,
            empty,
            lines: title === "Working" && !rows.length && empty ? [<QuietRepository key="quiet" text={empty} />] : rows.map((row) => (
              <span key={row.anchor} id={row.anchor}>
                <Link className="link" href={row.href}>{row.goal}</Link>
                <span className="flex items-center gap-2">
                  {row.state.label}
                  {row.paused ? <WorkCue piece="pause" watch={row.anchor + "/pause"} text={row.goal + " was paused."} /> : null}
                </span>
                {[
                  row.status, row.size,
                  row.lead && "Queen · lead " + (row.lead.display || row.lead.id),
                  row.runAgent?.display, row.branch && "branch " + row.branch,
                  ...row.changes.map((change) => "pull request #" + change.number + " " + pullState(change).label),
                  budgetText(row.cost, row.budget),
                  row.owner && "answers for it: " + (row.owner.display || row.owner.id),
                ].filter(Boolean).map((fact, i) => <span key={i} className="block">{fact}</span>)}
                {row.cells.map(({ id, title, state }) => (
                  <span key={id ?? title} className="flex items-center gap-2">
                    {title + " · " + cellLabels[state]}
                    {state === "working" ? <WorkCue piece="claim" watch={taskMotionKey(row.anchor, id)} text={"Work started on " + title + "."} /> : null}
                    {state === "finished" ? (
                      <span className="relative size-4 shrink-0">
                        <WorkCue piece="cap" watch={taskMotionKey(row.anchor, id)} text={title + " finished."} />
                      </span>
                    ) : null}
                  </span>
                ))}
                {row.joined ? (
                  <span className="flex items-center gap-2">
                    <WorkCue piece="join" watch={row.anchor + "/join"} text="Your request to join was accepted." />
                    Your request to join was accepted.
                  </span>
                ) : null}
              </span>
            )),
          })),
          ...(agents.length ? [{ title: "Agent directory", collapsed: true, lines: agents.map(({ principal }) => (
            <Link key={principal.id} href={repoHref(repo, "agents/" + encodeURIComponent(principal.id))} className="link">
              {principal.display || principal.id}{principal.runtime ? " · " + principal.runtime : ""}
            </Link>
          )) }] : []),
        ]} />
      </WorkMotion>
    );
  }

  return (
    <WorkMotion repo={repo} scope={scope} snapshot={snapshot}>
      <PageHeader title="Agents" actions={<RefreshWork />} />
      <div className="flex max-w-[920px] flex-col gap-10">
        {groups.map(({ title, empty, collapsed, rows }) => {
          const list = rows.length ? (
            <Rows label={title}>
              {rows.map((row) => (
                <li key={row.anchor} id={row.anchor} className="flex scroll-mt-24 flex-col gap-2 border-b border-line py-5 [overflow-wrap:anywhere]">
                  <Link href={row.href} data-nav-item className="link w-fit text-lg font-light">{row.goal}</Link>
                  <p className="flex flex-wrap items-center gap-x-2 text-sm text-ink-2">
                    <State shape={row.state.shape}>{row.state.label}</State>
                    {row.paused ? <WorkCue piece="pause" watch={row.anchor + "/pause"} text={row.goal + " was paused."} /> : null}
                    {row.status ? <><Sep /><span>{row.status}</span></> : null}
                  </p>
                  <div className="flex flex-wrap items-center gap-2">
                    <TaskCells cells={row.cells} row={row.anchor} />
                    {row.cells.filter((cell) => cell.state === "working").map((cell) => {
                      const watch = taskMotionKey(row.anchor, cell.id);
                      return <WorkCue key={watch} piece="claim" watch={watch} text={"Work started on " + cell.title + "."} />;
                    })}
                  </div>
                  {row.joined ? (
                    <p className="flex items-center gap-2 text-sm text-ink-2">
                      <WorkCue piece="join" watch={row.anchor + "/join"} text="Your request to join was accepted." />
                      Your request to join was accepted.
                    </p>
                  ) : null}
                  <p className="flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-ink-2">
                    <span>{row.size}</span>
                    {row.lead ? <><Sep /><span>Queen · lead {row.lead.display || row.lead.id}{row.lead.runtime ? " · " + row.lead.runtime : ""}</span></> : null}
                    {row.runAgent?.display ? <><Sep /><span>{row.runAgent.display}{row.runAgent.runtime ? " · " + row.runAgent.runtime : ""}</span></> : null}
                    <WorkFacts {...row} repo={repo} />
                  </p>
                </li>
              ))}
            </Rows>
          ) : empty ? (
            <div className="border-t border-line pt-3 text-sm text-ink-2">
              {title === "Working" ? <QuietRepository text={empty} /> : <p>{empty}</p>}
            </div>
          ) : null;

          return collapsed ? (
            <FinishedWork key={title} heading={title + " · " + rows.length} completions={rows.flatMap((row) =>
              row.cells.filter((cell) => cell.state === "finished").map((cell) => ({
                watch: taskMotionKey(row.anchor, cell.id),
                text: cell.title + " finished.",
              })),
            )}>
              {list}
            </FinishedWork>
          ) : (
            <section key={title} aria-label={title}>
              <h2 className="mb-3 text-sm font-medium text-ink-2">{title}</h2>
              {list}
            </section>
          );
        })}
        {agents.length ? (
          <details className="border-t border-line pt-2">
            <summary className="link min-h-6 cursor-pointer text-sm text-ink-2"><h2 className="inline font-medium">Agent directory</h2></summary>
            <DirectoryLinks repo={repo} agents={agents} />
          </details>
        ) : null}
      </div>
    </WorkMotion>
  );
}
