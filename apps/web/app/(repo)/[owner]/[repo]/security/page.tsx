import Link from "next/link";
import type { Metadata } from "next";
import { DismissAlert } from "@/components/alert-actions";
import { PageMenu } from "@/components/menu";
import { SignInToSee } from "@/components/error-state";
import { Time } from "@/components/time";
import { Badge, EmptyState, PageHeader, Stat, TabLinks } from "@/components/ui";
import { type RepoParams, scopeFromParams, serverClient, sessionState } from "@/lib/api/server";
import type { SecurityAlert } from "@/lib/api/types";
import { repoHref } from "@/lib/routes";
import { guard, param, type SearchParams } from "@/lib/page";

export const metadata: Metadata = { title: "Security" };

const KIND: Record<SecurityAlert["kind"], string> = { dependency: "Dependency", secret: "Secret", code_scanning: "Code scanning" };
const SEV_TONE: Record<SecurityAlert["severity"], "bad" | "warn" | "neutral" | "outline"> = { critical: "bad", high: "warn", medium: "outline", low: "neutral" };

export default async function SecurityPage({ params, searchParams }: { params: Promise<RepoParams>; searchParams: SearchParams }) {
  const sp = await searchParams;
  const state = (["open", "fixed", "dismissed"].includes(param(sp, "state")) ? param(sp, "state") : "open") as SecurityAlert["state"];
  return guard(async () => {
    const api = await serverClient();
    const { ref, fullName } = await scopeFromParams(params);
    const req = { repo: ref, state };
    // Alerts need write access to the repo, so a signed-out visitor is asked to sign in.
    if (!(await sessionState()).me) {
      return (
        <>
          <PageHeader title="Security" />
          <SignInToSee title="Sign in to see security alerts" next={repoHref(fullName, "security")} icon="security">
            Dependency advisories, blocked secrets and code scanning results are shown to people who can write here.
          </SignInToSee>
        </>
      );
    }
    const [list, all] = await Promise.all([api.security.listAlerts(req), api.security.listAlerts({ repo: ref })]);
    const open = all.alerts.filter((a) => a.state === "open");
    return (
      <>
        <PageHeader title="Security" lede="Known-vulnerable dependencies, secrets caught before they were pushed, and code scanning results." actions={<PageMenu op={{ service: "SecurityService", method: "ListAlerts", request: req, label: "List security alerts" }} />} />
        <div className="mb-10 grid grid-cols-2 gap-x-6 gap-y-5 sm:grid-cols-4">
          <Stat label="Open alerts" value={open.length} />
          <Stat label="Critical" value={open.filter((a) => a.severity === "critical").length} />
          <Stat label="With a fix ready" value={open.filter((a) => a.fixChangeRequestId).length} />
          <Stat label="Secrets blocked" value={all.alerts.filter((a) => a.kind === "secret" && a.state === "fixed").length} />
        </div>
        <TabLinks
          label="Alert state"
          items={(["open", "fixed", "dismissed"] as const).map((s) => ({ href: repoHref(fullName, s === "open" ? "security" : "security?state=" + s), label: s[0]?.toUpperCase() + s.slice(1), active: state === s, count: all.alerts.filter((a) => a.state === s).length }))}
        />
        <div>
          {list.alerts.length === 0 ? (
            <EmptyState title={"No " + state + " alerts"} icon="security">{state === "open" ? "Nothing needs attention here right now." : "Alerts show up here once they are " + state + "."}</EmptyState>
          ) : (
            <ul className="divide-y divide-line">
              {list.alerts.map((a) => (
                <li key={a.id} className="flex flex-col gap-2 py-4 md:flex-row md:items-start md:gap-6">
                  <div className="min-w-0 flex-1">
                    <p className="flex flex-wrap items-center gap-2">
                      <Badge tone={SEV_TONE[a.severity]}>{a.severity}</Badge>
                      <Badge tone="outline">{KIND[a.kind]}</Badge>
                      <span className="text-md font-medium">{a.title}</span>
                    </p>
                    <p className="mt-1 text-base text-ink-2">{a.detail}</p>
                    <p className="mt-1 flex flex-wrap gap-x-3 text-sm text-ink-3">
                      <span className="mono">{a.location}</span>
                      <span className="mono">{a.advisory}</span>
                      <Time iso={a.createdAt} />
                      {a.dismissedReason ? <span>dismissed: {a.dismissedReason}</span> : null}
                    </p>
                  </div>
                  <div className="flex flex-col items-start gap-2 md:items-end">
                    {a.fixChangeRequestId ? (
                      <Link href={repoHref(fullName, "changes/" + a.fixChangeRequestId)} className="link text-base">
                        Open the fix
                      </Link>
                    ) : null}
                    {a.state === "open" ? <DismissAlert repo={fullName} id={a.id} /> : null}
                  </div>
                </li>
              ))}
            </ul>
          )}
        </div>
      </>
    );
  });
}
