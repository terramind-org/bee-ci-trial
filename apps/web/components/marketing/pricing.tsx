/** Plan cards, the self-hosted card and the FAQ for `/pricing`. Server components; prices come from ListBillingPlans. */
import type { ReactNode } from "react";
import { Badge, ButtonLink, buttonClass, cx } from "@/components/ui";
import type { BillingPlan } from "@/lib/api/types";
import { usd } from "@/lib/format";
import { SELF_HOSTING_URL } from "@/lib/prompts";

export const PRODUCT_LABEL: Record<string, string> = {
  BILLING_PRODUCT_ACTIONS_MINUTES: "runner minutes",
  BILLING_PRODUCT_STORAGE_GB: "GB-months of storage",
  BILLING_PRODUCT_WORKSPACES_HOURS: "workspace hours",
  BILLING_PRODUCT_AGENT_TOKENS: "agent tokens",
  BILLING_PRODUCT_PACKAGES_TRANSFER: "GB of package transfer",
  BILLING_PRODUCT_LFS: "GB of large-file storage",
  BILLING_PRODUCT_SEATS: "seats",
  BILLING_PRODUCT_MARKETPLACE: "marketplace credit",
};

/** "2,000 runner minutes included". */
export function includedLine(u: BillingPlan["included"][number]): string {
  const label = PRODUCT_LABEL[u.product] ?? u.unit + "s";
  return u.quantity.toLocaleString("en-US") + " " + label + " included";
}

export function PlanCard({ plan }: { plan: BillingPlan }) {
  const monthly = Number(plan.seatMonthlyPriceMicroUsd);
  const yearly = Number(plan.seatYearlyPriceMicroUsd);
  const priced = monthly > 0 || yearly > 0;
  return (
    <article aria-labelledby={"plan-" + plan.id} data-testid="plan-card" data-plan={plan.id} className="flex min-w-0 flex-col rounded-lg border border-line bg-surface p-5">
      <h3 id={"plan-" + plan.id} className="text-lg font-semibold">
        {plan.name}
      </h3>
      <p className="mt-1 min-h-[3.25rem] text-base text-ink-2">{plan.description}</p>
      <div className="mt-4 min-h-[3.5rem]">
        {plan.selfServe ? (
          priced ? (
            <>
              <p className="num text-xl font-semibold">
                {usd(monthly)} <span className="text-base font-normal text-ink-3">per seat per month</span>
              </p>
              {yearly > 0 ? (
                <p className="num mt-0.5 text-base text-ink-2">
                  {usd(yearly)} <span className="text-ink-3">per seat per year</span>
                </p>
              ) : null}
            </>
          ) : (
            <p className="text-xl font-semibold">Free</p>
          )
        ) : (
          <p className="text-xl font-semibold">By contract</p>
        )}
      </div>
      <div className="mt-2">
        {plan.selfServe ? (
          <ButtonLink href="/signin?next=/new" variant={priced ? "primary" : "secondary"} className="w-full">
            {priced ? "Start with " + plan.name : "Get started"}
          </ButtonLink>
        ) : (
          <a href={SELF_HOSTING_URL} target="_blank" rel="noreferrer noopener" className={buttonClass("secondary", "md", "w-full")}>
            Contact sales
          </a>
        )}
      </div>
      {plan.included.length ? (
        <ul className="mt-5 flex flex-col gap-1.5 border-t border-line pt-4 text-base" aria-label={plan.name + " included usage"}>
          {plan.included.map((u) => (
            <li key={u.product} className="num text-ink-2">
              {includedLine(u)}
            </li>
          ))}
        </ul>
      ) : null}
      {plan.features.length ? (
        <ul className={cx("flex flex-col gap-1.5 text-base", plan.included.length ? "mt-3" : "mt-5 border-t border-line pt-4")} aria-label={plan.name + " features"}>
          {plan.features.map((f) => (
            <li key={f} className="text-ink">
              {f}
            </li>
          ))}
        </ul>
      ) : null}
    </article>
  );
}

export function SelfHostedCard() {
  return (
    <section id="self-hosted" aria-labelledby="self-hosted-title" data-testid="self-hosted-card" className="rule scroll-mt-20 pt-8">
      <div className="flex flex-col gap-4 md:flex-row md:items-center md:justify-between">
        <div className="min-w-0">
          <div className="flex items-center gap-2">
            <h2 id="self-hosted-title" className="text-lg font-semibold">
              Enterprise (self-hosted)
            </h2>
            <Badge tone="outline">Open source</Badge>
          </div>
          <p className="mt-1.5 max-w-[62ch] text-base text-ink-2">Run the same server and web app on your own machines, from one Docker Compose file. Single sign-on, SCIM provisioning and an admin console are part of it. Billing is optional for self-hosters.</p>
        </div>
        <div className="flex shrink-0 flex-wrap gap-2">
          <a href={SELF_HOSTING_URL} target="_blank" rel="noreferrer noopener" className={buttonClass("secondary", "md")}>
            Self-hosting guide
          </a>
          <ButtonLink href="/prompts#self-host-docker">Docker prompt</ButtonLink>
        </div>
      </div>
    </section>
  );
}

/** Answers come from docs/PRODUCT.md (§1, §3.2, §4.11, §4.20). */
const FAQ: { q: string; a: ReactNode }[] = [
  {
    q: "What am I paying for?",
    a: "Seats on a paid plan, plus metered usage beyond what the plan includes: runner minutes, storage, workspace hours and agent tokens. A usage query is a pure function of recorded events and the prices in force, so the numbers on your bill can be reproduced.",
  },
  {
    q: "Can I run Beehive myself instead?",
    a: "Yes. Beehive is open source and ships as one server binary that you can host yourself. Billing is optional for self-hosters, and usage is still metered for quotas.",
  },
  {
    q: "Can I bring my existing repositories?",
    a: "Yes. A one-command import brings in a repository or a whole organisation from GitHub, with a dry run first. Any git client also works against Beehive through its git gateway, offline work included.",
  },
  {
    q: "What happens if I downgrade or buy fewer seats?",
    a: "A downgrade takes effect at the end of the billing cycle. You cannot buy fewer seats than are in use: remove members first.",
  },
  {
    q: "Do agents need seats?",
    a: "Agents work through the same API, CLI and MCP tools as people. Their usage is attributed to the person who owns them, and the seats in use are listed on your organisation's billing page.",
  },
];

export function Faq() {
  return (
    <section aria-labelledby="faq-title" data-testid="pricing-faq">
      <h2 id="faq-title" className="text-xl font-semibold tracking-tight">
        Questions
      </h2>
      <dl className="mt-4 divide-y divide-line">
        {FAQ.map((f) => (
          <div key={f.q} className="py-4">
            <dt className="text-md font-medium">{f.q}</dt>
            <dd className="mt-1 max-w-[70ch] text-base text-ink-2">{f.a}</dd>
          </div>
        ))}
      </dl>
      <p className="mt-4 text-base text-ink-2">Education and nonprofit plans are assigned by an administrator.</p>
    </section>
  );
}
