import { notFound } from "next/navigation";
import { requestMode } from "@/app/request-mode";
import { AgentRunDetails } from "@/components/agents/run-details";
import { changeFacts, repositoryWork } from "@/components/agents/work";
import { canManageRepo, canWriteRepo, loadRepo, scopeFromParams, serverClient, sessionState, type RepoParams } from "@/lib/api/server";
import { BeeError } from "@/lib/api/transport";
import { guard, param, type SearchParams } from "@/lib/page";

export const metadata = { title: "Agent run" };

export default async function StandaloneRunPage({ params, searchParams }: { params: Promise<RepoParams & { runId: string }>; searchParams: SearchParams }) {
  const { runId } = await params;
  return guard(async () => {
    const { ref, fullName, owner: org } = await scopeFromParams(params);
    const [api, repo, { me }, { mode }, query] = await Promise.all([serverClient(), loadRepo(fullName), sessionState(), requestMode(), searchParams]);
    if (!repo) notFound();
    const run = await api.agent.getAgentRun({ id: runId });
    if (run.repo?.id !== repo.id && run.repo?.fullName !== fullName) notFound();
    const items = await repositoryWork(api, ref);
    const standalone = items.find((item) => item.run?.id === runId);
    const swarm = items.find((item) => item.swarm?.memberRunIds.includes(runId))?.swarm;
    const canWrite = !!me && !repo.archived && !!standalone && await canWriteRepo(fullName);
    const ownerId = run.agent?.ownerId;
    const canManage = canWrite && (
      me.id === run.startedBy?.id || me.id === ownerId || me.id === run.agent?.id ||
      await canManageRepo(fullName)
    );
    const eventsToken = param(query, "events");
    const messagesToken = param(query, "messages");
    const pageSize = 50;
    const [eventPage, messagePage, changes, directory] = await Promise.all([
      api.agent.listAgentRunEvents({ runId, page: { pageToken: eventsToken, pageSize } }),
      api.agent.listAgentRunEvents({ runId, kinds: ["AGENT_RUN_EVENT_KIND_STEERING_MESSAGE"], page: { pageToken: messagesToken, pageSize } }),
      changeFacts(api, ref, standalone?.changeRequestIds ?? [], run.changeRequests),
      ownerId && run.startedBy?.id !== ownerId
        ? api.agent.listAgents({ org, repo: ref }).catch((error: unknown) => {
            if (error instanceof BeeError && ["permission_denied", "not_found", "unimplemented"].includes(error.code)) return null;
            throw error;
          })
        : null,
    ]);
    const owner = ownerId
      ? (run.startedBy?.id === ownerId ? run.startedBy.display : directory?.agents.find((agent) => agent.owner?.id === ownerId)?.owner?.display) || ownerId
      : run.startedBy?.kind === "PRINCIPAL_KIND_HUMAN" ? run.startedBy.display : undefined;
    // The repository work feed supplies the finance fields filtered for this reader.
    const visibleRun = { ...run, cost: standalone?.run?.cost, budget: standalone?.run?.budget };
    const events = eventPage.events
      .filter((event) => event.kind !== "AGENT_RUN_EVENT_KIND_STEERING_MESSAGE")
      .map((event) => standalone?.run?.cost ? event : { ...event, cost: undefined });
    return <AgentRunDetails repo={fullName} mode={mode} run={visibleRun} swarm={swarm} owner={owner} changes={changes}
      events={events} eventsToken={eventsToken} nextEvents={eventPage.page?.nextPageToken}
      messages={messagePage.events} messagesToken={messagesToken} nextMessages={messagePage.page?.nextPageToken} canManage={!!canManage} />;
  });
}
