import type { Metadata } from "next";
import { notFound } from "next/navigation";
import { PageMenu } from "@/components/menu";
import { ButtonLink, EmptyState, PageHeader } from "@/components/ui";
import { Faq, PlanCard, SelfHostedCard } from "@/components/marketing/pricing";
import { edition, serverClient, sessionState } from "@/lib/api/server";
import { BeeError } from "@/lib/api/transport";
import { guard } from "@/lib/page";

export const metadata: Metadata = { title: "Pricing" };
// The edition and the plans are a runtime fact of the server, so this page cannot be prerendered.
export const dynamic = "force-dynamic";

const OP = { service: "BillingService", method: "ListBillingPlans", request: {}, label: "List the plans" } as const;

/** `/pricing`: the plans on the hosted edition. A self-hosted instance has no such page. */
export default async function PricingPage() {
  if ((await edition()) !== "cloud") notFound();
  return guard(async () => {
    const api = await serverClient();
    // The server lists the plans offered to an account, so it needs one: the caller's first org.
    // Signed out, or without an org, there is no account to ask about.
    const { me } = await sessionState();
    const org = me ? (await api.org.listOrgs({})).orgs[0]?.name : undefined;
    if (!org) {
      return (
        <div className="mx-auto flex max-w-[1120px] flex-col gap-10">
          <PageHeader title="Pricing" lede="Pay per seat, plus usage beyond what a plan includes." />
          <div data-testid="pricing-sign-in">
            <EmptyState
              title={me ? "Create an organization to see its plans" : "Sign in to see the plans for your organization"}
              icon="releases"
              action={<ButtonLink href={me ? "/new" : "/signin?next=/pricing"} variant="primary">{me ? "New organization" : "Sign in"}</ButtonLink>}
            >
              Plans are offered to an account, so the server lists them for an organization you belong to.
            </EmptyState>
          </div>
          <SelfHostedCard />
          <Faq />
        </div>
      );
    }
    let plans;
    try {
      plans = (await api.billing.listBillingPlans({ scope: { org } })).plans;
    } catch (err) {
      if (!(err instanceof BeeError) || err.code !== "unimplemented") throw err;
      return (
        <div className="mx-auto max-w-[1120px]">
          <PageHeader title="Pricing" lede="Plans and usage prices for Beehive." />
          <div data-testid="pricing-unavailable">
            <EmptyState title="Plans are not available on this server" icon="releases">
              This server does not offer billing (<span className="mono">{err.reason}</span>). {err.message}
            </EmptyState>
          </div>
          <div className="mt-8">
            <SelfHostedCard />
          </div>
        </div>
      );
    }
    return (
      <div className="mx-auto flex max-w-[1120px] flex-col gap-10">
        <PageHeader title="Pricing" lede="Pay per seat, plus usage beyond what a plan includes." actions={<PageMenu op={{ ...OP, request: { scope: { org } } }} />} />
        <section aria-label="Plans">
          <div className="grid gap-4 md:grid-cols-2 lg:grid-cols-3" data-testid="plans">
            {plans.map((p) => (
              <PlanCard key={p.id} plan={p} />
            ))}
          </div>
        </section>
        <SelfHostedCard />
        <Faq />
      </div>
    );
  });
}
