import type { Metadata } from "next";
import { notFound } from "next/navigation";
import { requestMode } from "@/app/request-mode";
import { SwarmDetail, swarmIsActive, type SwarmMember } from "@/components/agents/swarm-detail";
import { changeFacts, finishedRun } from "@/components/agents/work";
import { principals } from "@/lib/api/ops/agents";
import { mapLimit } from "@/lib/api/ops/shared";
import { canManageRepo, canWriteRepo, currentPrincipal, loadRepo, scopeFromParams, serverClient, type RepoParams } from "@/lib/api/server";
import { BeeError } from "@/lib/api/transport";
import type { AgentRun } from "@/lib/api/types";
import { guard } from "@/lib/page";

export const metadata: Metadata = { title: "Swarm" };

function agentLabel(agent: NonNullable<AgentRun["agent"]>): string {
  return [agent.display || agent.id, agent.runtime].filter(Boolean).join(" · ");
}

export default async function SwarmPage({ params }: { params: Promise<RepoParams & { swarmId: string }> }) {
  const { swarmId } = await params;
  const { fullName, ref, owner } = await scopeFromParams(params);
  return guard(async () => {
    const [api, me, { mode }, repo] = await Promise.all([serverClient(), currentPrincipal(), requestMode(), loadRepo(fullName)]);
    if (!repo) notFound();
    const target = { repo: ref, id: swarmId };
    const swarm = await api.swarm.getSwarm(target);
    const open = swarmIsActive(swarm) && !repo.archived;
    const person = me?.kind === "PRINCIPAL_KIND_HUMAN";
    const answersForSwarm = person && swarm.owner?.id === me?.id;
    const knownAgents = new Map(swarm.tasks.flatMap((task) => task.attempts.flatMap((attempt) => attempt.agent ? [[attempt.runId, attempt.agent] as const] : [])));
    if (swarm.lead) knownAgents.set(swarm.leadRunId, swarm.lead);

    const [runs, writable, mayManage] = await Promise.all([
      mapLimit(swarm.memberRunIds, 8, async (id): Promise<{ id: string; run?: AgentRun; reason?: string }> => {
        try {
          return { id, run: await api.agent.getAgentRun({ id }) };
        } catch (err) {
          if (!(err instanceof BeeError)) throw err;
          return { id, reason: err.reason };
        }
      }),
      open && person ? canWriteRepo(fullName) : false,
      open && person && !answersForSwarm ? canManageRepo(fullName) : false,
    ]);

    const agents = runs.map(({ id, run }) => run?.agent ?? knownAgents.get(id));
    const [people, changes] = await Promise.all([
      principals(api.rpc, agents.flatMap((agent) => agent?.ownerId ? [agent.ownerId] : [])),
      changeFacts(api, ref, swarm.changeRequestIds, runs.flatMap(({ run }) => run?.changeRequests ?? [])),
    ]);
    const members: SwarmMember[] = runs.map(({ id, reason }, index) => {
      const agent = agents[index];
      return {
        runId: id,
        agent,
        person: id === swarm.leadRunId && swarm.owner ? swarm.owner : agent?.ownerId ? people.get(agent.ownerId) : undefined,
        reason,
      };
    });
    const lead = runs.find((row) => row.id === swarm.leadRunId)?.run;
    const canSpeak = open && person && writable && !!lead && !finishedRun(lead);
    const canDecideJoins = open && answersForSwarm && writable;
    const canChangeState = canDecideJoins || mayManage;

    let join: { agents: { id: string; label: string }[]; allowAgentId: boolean } | undefined;
    const alreadyIn = answersForSwarm || members.some((member) => member.agent?.id === me?.id || member.agent?.ownerId === me?.id);
    if (mode === "keeper" && open && person && me && !alreadyIn) {
      try {
        const { agents: directory } = await api.agent.listAgents({ org: owner });
        const requested = new Set(swarm.joins.filter((request) => request.state === "SWARM_JOIN_STATE_PENDING" || request.state === "SWARM_JOIN_STATE_ACCEPTED").map((request) => request.agent?.id));
        const eligible = directory
          .filter(({ principal }) => principal.ownerId === me.id && !requested.has(principal.id))
          .map(({ principal }) => ({ id: principal.id, label: agentLabel(principal) }))
          .sort((a, b) => a.label.localeCompare(b.label));
        if (eligible.length) join = { agents: eligible, allowAgentId: false };
      } catch (err) {
        if (!(err instanceof BeeError)) throw err;
        // A signed-in public reader can still offer an agent they own if this org hides its directory.
        join = { agents: [], allowAgentId: true };
      }
    }
    return <SwarmDetail repo={fullName} target={target} swarm={swarm} mode={mode} members={members} changes={changes}
      canSpeak={canSpeak} canDecideJoins={canDecideJoins} canChangeState={canChangeState} join={join} viewerId={person ? me.id : undefined} />;
  });
}
