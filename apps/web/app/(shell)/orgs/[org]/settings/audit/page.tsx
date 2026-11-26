import type { Metadata } from "next";
import Link from "next/link";
import { requestMode } from "@/app/request-mode";
import { PageFacts } from "@/components/bee/page-facts";
import { Icon } from "@/components/icons";
import { Button, ButtonLink, EmptyState, Field, Input, Select } from "@/components/kit";
import { PageMenu } from "@/components/menu";
import { RecordEntry, verificationLine } from "@/components/org/record";
import { PageHeader } from "@/components/ui";
import { serverClient } from "@/lib/api/server";
import { BeeError } from "@/lib/api/transport";
import type { ListAuditRequest, Principal } from "@/lib/api/types";
import { uniqueById } from "@/lib/format";
import { guard, param, type SearchParams } from "@/lib/page";
import { orgSettingsHref } from "@/lib/routes";
import { toolFor } from "@/lib/tools";
import { SettingsTabs } from "../tabs";

export const metadata: Metadata = { title: "Record" };

/** A reader with audit access may lack access to a directory. The audit entries still name actors. */
async function directoryEntries<T>(request: Promise<T[]>): Promise<T[]> {
  try {
    return await request;
  } catch (error) {
    if (error instanceof BeeError && (error.code === "permission_denied" || error.code === "unimplemented")) return [];
    throw error;
  }
}

export default async function AuditPage({ params, searchParams }: { params: Promise<{ org: string }>; searchParams: SearchParams }) {
  const sp = await searchParams;
  return guard(async () => {
    const api = await serverClient();
    const org = decodeURIComponent((await params).org);
    const path = orgSettingsHref(org, "audit");
    const actorId = param(sp, "actor");
    const action = param(sp, "action").trim();
    const rawOutcome = param(sp, "outcome");
    const outcome = rawOutcome === "allowed" ? "allowed" : rawOutcome === "refused" || param(sp, "denied") === "1" ? "refused" : "";
    const pageToken = param(sp, "page");
    const req: ListAuditRequest = {
      org,
      actorId: actorId || undefined,
      action: action || undefined,
      deniedOnly: outcome === "refused" || undefined,
      allowedOnly: outcome === "allowed" || undefined,
      page: { pageSize: 50, pageToken: pageToken || undefined },
    };
    const [{ mode }, audit, agents, members] = await Promise.all([
      requestMode(),
      api.audit.listAudit(req),
      directoryEntries(api.agent.listAgents({ org }).then((r) => r.agents.map((a) => a.principal))),
      directoryEntries(api.org.listMembers({ org, page: { pageSize: 200 } }).then((r) => r.members.map((m) => m.principal))),
    ]);
    const people = uniqueById([...members, ...agents, ...audit.entries.map((e) => e.actor)].filter((p): p is Principal => !!p?.id));
    const selectedActor = people.find((p) => p.id === actorId);
    const verification = verificationLine(audit);
    const filter = new URLSearchParams();
    if (actorId) filter.set("actor", actorId);
    if (action) filter.set("action", action);
    if (outcome) filter.set("outcome", outcome);
    const nextPageToken = audit.page?.nextPageToken;
    if (nextPageToken) filter.set("page", nextPageToken);
    const next = nextPageToken ? path + "?" + filter.toString() : undefined;
    const empty = pageToken || next ? "No entries match on this page." : "No entries match.";

    if (mode === "bee") {
      return (
        <PageFacts
          page="org-record"
          title={org + " · Record"}
          groups={[
            {
              title: "facts",
              facts: [
                { label: "verification", value: verification },
                actorId ? { label: "actor", value: selectedActor?.display || actorId } : null,
                action ? { label: "action prefix", value: action } : null,
                outcome ? { label: "outcome", value: outcome } : null,
                next ? { label: "older entries", value: <Link className="link" href={next}>View older entries</Link> } : null,
              ],
            },
            { title: "entries", lines: audit.entries.map((entry) => <RecordEntry key={entry.seq} entry={entry} />), empty },
          ]}
          tools={[
            toolFor("AuditService/ListAudit", "Find entries by actor, action prefix or refusal"),
            toolFor("AuditService/VerifyAuditChain", "Verify the sealed record and find any broken link"),
            toolFor("OrgService/ListMembers", "Names of people in the organisation"),
            toolFor("AgentService/ListAgents", "Names of agents in the organisation"),
          ]}
        />
      );
    }

    return (
      <>
        <PageHeader title="Record" eyebrow={org} actions={<PageMenu op={{ service: "AuditService", method: "ListAudit", request: req, label: "Query the record" }} />} />
        <SettingsTabs org={org} active="audit" />
        <div className="max-w-4xl">
          <p className="mb-6 flex items-center gap-2 text-base text-ink" data-testid="chain-status">
            <Icon name={audit.chainVerified ? "check" : "alert"} />
            {verification}
          </p>
          <form action={path} className="mb-6 grid items-end gap-3 sm:grid-cols-2 xl:grid-cols-[minmax(0,1fr)_minmax(0,1fr)_minmax(0,1fr)_auto]" aria-label="Filter the record">
            <Field label="Actor" htmlFor="record-actor">
              <Select id="record-actor" name="actor" defaultValue={actorId}>
                <option value="">Anyone</option>
                {actorId && !selectedActor ? <option value={actorId}>{actorId}</option> : null}
                {people.map((p) => (
                  <option key={p.id} value={p.id}>{p.display || p.id}{p.kind === "PRINCIPAL_KIND_AGENT" ? " (agent)" : ""}</option>
                ))}
              </Select>
            </Field>
            <Field label="Action prefix" htmlFor="record-action">
              <Input id="record-action" name="action" defaultValue={action} placeholder="Any action" />
            </Field>
            <Field label="Outcome" htmlFor="record-outcome">
              <Select id="record-outcome" name="outcome" defaultValue={outcome}>
                <option value="">Any outcome</option>
                <option value="allowed">Allowed</option>
                <option value="refused">Refused</option>
              </Select>
            </Field>
            <div className="flex flex-wrap gap-2">
              <Button type="submit" variant="primary">Apply</Button>
              {actorId || action || outcome ? <ButtonLink href={path} variant="ghost">Clear</ButtonLink> : null}
            </div>
          </form>
          {audit.entries.length ? (
            <ol className="divide-y divide-line border-y border-line" aria-label="Record entries">
              {audit.entries.map((entry) => (
                <li key={entry.seq} className="py-4 [overflow-wrap:anywhere]" data-testid="audit-row">
                  <RecordEntry entry={entry} personMode />
                </li>
              ))}
            </ol>
          ) : (
            <EmptyState title={empty} />
          )}
          {next ? <div className="mt-4"><ButtonLink href={next}>Older entries</ButtonLink></div> : null}
        </div>
      </>
    );
  });
}
