import type { Metadata } from "next";
import { PageMenu } from "@/components/menu";
import { PrincipalName } from "@/components/domain";
import { Icon } from "@/components/icons";
import { Time } from "@/components/time";
import { Badge, EmptyState, PageHeader, buttonClass, cx } from "@/components/ui";
import { serverClient } from "@/lib/api/server";
import type { Principal } from "@/lib/api/types";
import { uniqueById } from "@/lib/format";
import { guard, param, type SearchParams } from "@/lib/page";
import { orgSettingsHref } from "@/lib/routes";
import { SettingsTabs } from "../tabs";

export const metadata: Metadata = { title: "Audit log" };

const ACTIONS = ["change.", "token.", "policy.", "lease.", "review.", "import.", "security.", "issue.", "member.", "run.", "queue."];

export default async function AuditPage({ params, searchParams }: { params: Promise<{ org: string }>; searchParams: SearchParams }) {
  const sp = await searchParams;
  return guard(async () => {
    const api = await serverClient();
    const org = decodeURIComponent((await params).org);
    const actorId = param(sp, "actor");
    const action = param(sp, "action");
    const deniedOnly = param(sp, "denied") === "1";
    const req = { org, actorId: actorId || undefined, action: action || undefined, deniedOnly: deniedOnly || undefined };
    const [audit, agents, members] = await Promise.all([api.audit.listAudit(req), api.agent.listAgents({ org }), api.org.listMembers({ org })]);
    const people: Principal[] = uniqueById([...members.members.map((m) => m.principal).filter((p): p is Principal => !!p), ...agents.agents.map((a) => a.principal)]);
    return (
      <>
        <PageHeader title="Settings" lede={<>Every state change, who made it, on whose behalf, and the rule that allowed it.</>} actions={<PageMenu op={{ service: "AuditService", method: "ListAudit", request: req, label: "Query the audit log" }} />} />
        <SettingsTabs org={org} active="audit" />
        <form action={orgSettingsHref(org, "audit")} className="mb-6 flex flex-wrap items-end gap-3" aria-label="Filter the audit log">
          <label className="flex flex-col gap-1 text-base">
            Actor
            <select name="actor" defaultValue={actorId} className="h-9 w-48 rounded-md border border-transparent bg-input px-2 text-base">
              <option value="">Anyone</option>
              {people.map((p) => (
                <option key={p.id} value={p.id}>
                  {p.display}
                  {p.kind === "PRINCIPAL_KIND_AGENT" ? " (agent)" : ""}
                </option>
              ))}
            </select>
          </label>
          <label className="flex flex-col gap-1 text-base">
            Action
            <select name="action" defaultValue={action} className="mono h-9 w-40 rounded-md border border-transparent bg-input px-2 text-base">
              <option value="">any</option>
              {ACTIONS.map((a) => (
                <option key={a} value={a}>
                  {a}*
                </option>
              ))}
            </select>
          </label>
          <label className="flex h-9 items-center gap-1.5 text-base">
            <input type="checkbox" name="denied" value="1" defaultChecked={deniedOnly} className="accent-[var(--accent)]" />
            Denied only
          </label>
          <button type="submit" className={buttonClass("primary", "md")}>
            Apply
          </button>
          <span className={cx("ml-auto flex items-center gap-1.5 text-base", audit.chainVerified ? "text-ok" : "text-bad")} data-testid="chain-status">
            <Icon name={audit.chainVerified ? "check" : "alert"} />
            {audit.chainVerified ? "Hash chain verified" : "Hash chain broken"}
          </span>
        </form>
        <div>
          {audit.entries.length === 0 ? (
            <EmptyState title="No entries match" icon="search">Try a different person or action, or clear the filters.</EmptyState>
          ) : (
            <div /* the gradients are edge fades: they show on a side only while there is more to scroll to */
              className="overflow-x-auto scrollbar-thin [background:linear-gradient(to_right,var(--bg)_30%,transparent)_0_0/32px_100%_no-repeat_local,linear-gradient(to_left,var(--bg)_30%,transparent)_100%_0/32px_100%_no-repeat_local,linear-gradient(to_right,var(--line-strong),transparent)_0_0/12px_100%_no-repeat_scroll,linear-gradient(to_left,var(--line-strong),transparent)_100%_0/12px_100%_no-repeat_scroll]" tabIndex={0} role="region" aria-label="Audit entries">
              <table className="w-full min-w-[760px] text-base">
                <thead className="text-left text-sm text-ink-3">
                  <tr className="border-b border-line">
                    <th scope="col" className="py-3 font-medium">When</th>
                    <th scope="col" className="px-2 py-2 font-medium">Actor</th>
                    <th scope="col" className="px-2 py-2 font-medium">Action</th>
                    <th scope="col" className="px-2 py-2 font-medium">Target</th>
                    <th scope="col" className="px-2 py-2 font-medium">Decision</th>
                    <th scope="col" className="py-3 font-medium">Hash</th>
                  </tr>
                </thead>
                <tbody className="divide-y divide-line">
                  {audit.entries.map((e) => (
                    <tr key={e.seq} className="align-top" data-testid="audit-row">
                      <td className="py-3 whitespace-nowrap text-ink-2">
                        <Time iso={e.time} />
                        <span className="num block text-sm text-ink-3">#{e.seq}</span>
                      </td>
                      <td className="px-2 py-2">
                        <PrincipalName principal={e.actor} showRuntime={false} />
                        {e.onBehalfOf.length ? <span className="block text-sm text-ink-3">on behalf of {e.onBehalfOf.join(" → ")}</span> : null}
                        <span className="block text-sm text-ink-3">{e.source}</span>
                      </td>
                      <td className="mono px-2 py-2 text-sm">{e.action}</td>
                      <td className="min-w-40 px-2 py-2 [overflow-wrap:anywhere]">
                        {e.target}
                        {e.repo ? <span className="block text-sm text-ink-3">{e.repo}</span> : null}
                      </td>
                      <td className="px-2 py-2">
                        <Badge tone={e.allowed ? "ok" : "bad"}>{e.allowed ? "allowed" : "denied"}</Badge>
                        {e.rule ? <span className="block max-w-64 text-sm text-ink-3">{e.rule}</span> : null}
                      </td>
                      <td className="mono py-3 text-sm text-ink-3" title={"prev " + e.prevHash}>
                        {e.hash}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </div>
      </>
    );
  });
}
