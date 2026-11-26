import type { Metadata } from "next";
import { requestMode } from "@/app/request-mode";
import { AgentsWorkList } from "@/components/agents/work-list";
import { changeFacts, repositoryWork } from "@/components/agents/work";
import { SignInToSee } from "@/components/error-state";
import { PageHeader } from "@/components/ui";
import { type RepoParams, scopeFromParams, serverClient, sessionState } from "@/lib/api/server";
import { BeeError } from "@/lib/api/transport";
import { guard } from "@/lib/page";
import { repoHref } from "@/lib/routes";

export const metadata: Metadata = { title: "Agents" };

export default async function AgentsPage({ params }: { params: Promise<RepoParams> }) {
  return guard(async () => {
    const { owner: org, ref, fullName } = await scopeFromParams(params);
    const [api, { me }, { mode }] = await Promise.all([serverClient(), sessionState(), requestMode()]);
    if (!me) {
      return (
        <>
          <PageHeader title="Agents" />
          <SignInToSee title="Sign in to see agents" next={repoHref(fullName, "agents")} icon="agents">
            See what agents are working on in this repository.
          </SignInToSee>
        </>
      );
    }
    // A repository reader can see work without being allowed to see the organisation directory.
    const directory = api.agent.listAgents({ org, repo: ref }).catch((error: unknown) => {
      if (error instanceof BeeError && ["permission_denied", "not_found", "unimplemented"].includes(error.code)) return { agents: [] };
      throw error;
    });
    const [items, { agents }] = await Promise.all([repositoryWork(api, ref), directory]);
    const changes = await changeFacts(api, ref, items.flatMap((item) => item.changeRequestIds), items.flatMap(({ run }) => run?.changeRequests ?? []));
    return <AgentsWorkList repo={fullName} mode={mode} items={items} agents={agents} changes={changes} now={Date.now()}
      viewerId={me.kind === "PRINCIPAL_KIND_HUMAN" ? me.id : undefined} />;
  });
}
