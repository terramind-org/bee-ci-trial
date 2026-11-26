/**
 * "Who did it" on a pull request: the agent, the person it answers to, the model and runtime,
 * what the work cost, and the prompt digest: the fingerprint of what the agent was asked, which
 * the server records with every commit and no screen showed before. A line the data does not
 * have is left out; nothing is filled in.
 */
import Link from "next/link";
import type { PullSafetyLink } from "@/components/safety/pull-record";
import { duration, usd } from "@/lib/format";
import type { WhoDidIt as Who } from "@/lib/pull-page";
import { Kv, Mark } from "../parts";
import { RepoLink } from "../repo-link";

export function WhoDidIt({ who, safety }: { who: Who; safety?: PullSafetyLink }) {
  return (
    <div data-testid="who-did-it">
      <Kv
        rows={[
          who.author
            ? [
                who.agent ? "Agent" : "Person",
                who.agent && who.agentId ? (
                  <RepoLink key="agent" sub={"agents/" + who.agentId} className="link">
                    {who.author}
                  </RepoLink>
                ) : who.person && who.person === who.author ? (
                  <Mark key="person">{who.author}</Mark>
                ) : (
                  who.author
                ),
              ]
            : null,
          who.agent && who.person ? ["For", <Mark key="for">{who.person}</Mark>] : null,
          who.models.length ? ["Model", who.models.join(", ")] : null,
          who.runtimes.length ? ["Runtime", who.runtimes.join(", ")] : null,
          who.costMicros ? ["Cost", usd(who.costMicros)] : null,
          who.sessions ? ["Sessions", String(who.sessions)] : null,
          ["Revisions", String(who.revisions)],
          who.promptDigests.length
            ? [
                "Prompt",
                <span key="prompt" className="flex min-w-0 flex-col" data-testid="prompt-digest">
                  {who.promptDigests.map((d) => (
                    <span key={d} className="mono [overflow-wrap:anywhere]" title="The digest of the prompt the agent was given, as recorded with its commits">
                      {d}
                    </span>
                  ))}
                </span>,
              ]
            : null,
          safety ? ["Safety", <Link key="safety" className="link" href={safety.href}>{safety.verdict}</Link>] : null,
          who.ownChecks.length ? ["Ran itself", who.ownChecks.join(", ")] : null,
          who.elapsedMs >= 60_000 ? ["Time", duration(who.elapsedMs) + " from opening to the latest step"] : null,
        ]}
      />
    </div>
  );
}
