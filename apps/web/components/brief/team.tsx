/**
 * "Who did it, what did it cost?" (docs/spec/web-ui.md §3): the agents and people behind a change
 * with their roles, the models the agents ran, what their sessions cost and how long the work took.
 */
import type { AgentSession, ChangeRequest, Comment, Principal, Review } from "@/lib/api/types";
import { listing } from "@/lib/checkwords";
import { duration, usd } from "@/lib/format";
import { runtimeName, teamOf, type Member } from "@/lib/team";
import { Avatar, toAvatar } from "../avatar";
import { RepoLink } from "../repo-link";

function runtimeOf(m: Member): string {
  return runtimeName(m.principal?.runtime ?? "");
}

function MemberRow({ m }: { m: Member }) {
  const name = m.principal?.display || m.id;
  const facts: string[] = [];
  if (m.models.length) facts.push(listing(m.models));
  if (m.sessions) facts.push(m.sessions + (m.sessions === 1 ? " session" : " sessions"));
  if (m.costMicros) facts.push(usd(m.costMicros));
  if (m.elapsedMs >= 60_000) facts.push(duration(m.elapsedMs) + " from first to last step");
  return (
    <li className="flex items-start gap-3 py-2.5" data-member={m.agent ? "agent" : "person"}>
      <Avatar principal={m.principal ? toAvatar(m.principal) : { id: m.id, name, kind: m.agent ? "agent" : "human" }} size={32} />
      <div className="flex min-w-0 flex-col">
        <span className="flex flex-wrap items-baseline gap-x-2">
          {m.agent ? (
            <RepoLink sub={"agents/" + m.id} className="text-base font-medium text-ink hover:underline">
              {name}
            </RepoLink>
          ) : (
            <span className="text-base font-medium text-ink">{name}</span>
          )}
          <span className="text-sm text-ink-3">{m.agent ? "Agent" + (runtimeOf(m) ? " on " + runtimeOf(m) : "") : "Person"}</span>
        </span>
        <span className="text-base text-ink-2">{m.rolesText}</span>
        {facts.length ? <span className="text-sm text-ink-3">{facts.join(" · ")}</span> : null}
      </div>
    </li>
  );
}

export function Team({ change, reviews, comments, sessions, directory }: { change: Pick<ChangeRequest, "author" | "createdAt" | "revisions" | "brief">; reviews?: Review[]; comments?: Comment[]; sessions?: AgentSession[]; directory?: Principal[] }) {
  const team = teamOf({ change, reviews, comments, sessions, directory });
  const agents = team.members.filter((m) => m.agent).length;
  const people = team.members.length - agents;
  const who = [agents ? agents + (agents === 1 ? " agent" : " agents") : "", people ? people + (people === 1 ? " person" : " people") : ""].filter(Boolean);
  const totals = [listing(who), team.costMicros ? usd(team.costMicros) + " in agent sessions" : "", team.elapsedMs >= 60_000 ? duration(team.elapsedMs) + " from opening to the latest step" : ""].filter(Boolean);
  return (
    <div className="flex flex-col gap-1" data-team>
      {totals.length ? <p className="text-base text-ink-2">{totals.join(" · ")}</p> : null}
      <ul className="flex flex-col">
        {team.members.map((m) => (
          <MemberRow key={m.id} m={m} />
        ))}
      </ul>
    </div>
  );
}
