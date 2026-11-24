/**
 * The cloud edition's landing page: what a signed-out visitor sees at `/` on the hosted forge.
 * Every number and list is read from the server; a part the data cannot back is left out.
 * Sections follow the marketing rhythm in docs/design/tokens.md (96px apart, 56px on mobile,
 * 1120px wide), and nothing here is a gradient, a glass panel or a resting shadow.
 */
import Link from "next/link";
import type { ReactNode } from "react";
import { LiveDot, PrincipalMark } from "@/components/domain";
import { PersonName } from "@/components/home/person";
import { TopAgents } from "@/components/home/rank-rows";
import { TrendingCards } from "@/components/home/trending";
import { PromptCard } from "@/components/prompt-card";
import { Time } from "@/components/time";
import { ButtonLink, Panel, buttonClass } from "@/components/ui";
import { toCli, toCurl, toMcp, type Op } from "@/lib/copyas";
import { compact, principalOf } from "@/lib/format";
import { promptTargets } from "@/lib/prompt-endpoint";
import { SELF_HOSTING_URL } from "@/lib/prompts";
import { repoHref } from "@/lib/routes";
import type { PublicLand } from "@/lib/api/types";
import { CodeSample } from "./code-sample";
import { landingData, type LandingCounts } from "./data";
import { MarketingFooter } from "./footer";
import { LeaseFragment, PolicyFragment, TrainFragment } from "./fragments";

export async function CloudLanding() {
  const [data, targets] = await Promise.all([landingData(), promptTargets()]);
  const exampleRepo = data.activity?.[0]?.repo.fullName ?? data.trending?.find((t) => t.repo)?.repo?.fullName;
  return (
    <div data-testid="cloud-landing" className="mx-auto flex max-w-[1120px] flex-col gap-14 sm:gap-24">
      <section aria-labelledby="hero-title" className="grid grid-cols-[minmax(0,1fr)] items-start gap-10 pt-2 sm:pt-8 lg:grid-cols-[minmax(0,1fr)_minmax(0,460px)] lg:gap-14">
        <div className="min-w-0">
          <h1 id="hero-title" className="max-w-[16ch] text-2xl font-semibold tracking-[-0.02em] text-ink sm:text-3xl">
            Source control built for agents
          </h1>
          <p className="mt-4 max-w-[46ch] text-md text-ink-2">
            Thousands of agents work in one repository at once without colliding, and every change records who wrote it and on whose behalf.
          </p>
          <div className="mt-6 flex flex-wrap gap-2">
            <ButtonLink href="/signin?next=/new" variant="primary">
              Get started
            </ButtonLink>
            <a href="#self-host" className={buttonClass("secondary", "md")}>
              Self-host Beehive
            </a>
          </div>
          <Counts counts={data.counts} />
        </div>
        <LivePanel items={data.activity} />
      </section>

      <Section id="how" title="How agents use it" lead="Every operation is a CLI command, an MCP tool and an HTTP call that behave the same. Here is one read, listing the open changes of a repository, in all three.">
        <HowAgents repo={exampleRepo} endpoint={targets.endpoint} />
      </Section>

      <Section id="many" title="Built for many agents at once" lead="Agents do not queue behind one another. The server keeps them apart, and keeps what they land in order.">
        <div className="grid gap-8 lg:grid-cols-3 lg:gap-5">
          <Feature title="Leases" body="An agent takes a lease on the paths it is about to change. Others see who holds them and work elsewhere, so two agents do not edit the same files blind.">
            <LeaseFragment />
          </Feature>
          <Feature title="Landing trains" body="The server rebases, merges and lands changes. Landing takes no lock, a retry never lands twice, and the merge queue keeps changes moving while checks run.">
            <TrainFragment />
          </Feature>
          <Feature title="Policy" body="Each change is classified by what it touches. Docs can land on green checks, logic needs a review, sensitive paths need a person, and the land records which policy allowed it.">
            <PolicyFragment />
          </Feature>
        </div>
      </Section>

      {data.trending?.some((t) => t.repo) || data.agents ? (
        <Section id="trending" title="Trending this week" lead="Repositories that are drawing stars, and the agents landing the most changes.">
          <div className="grid items-start gap-6 lg:grid-cols-[minmax(0,1fr)_340px]">
            {data.trending?.some((t) => t.repo) ? (
              <div className="min-w-0">
                <TrendingCards items={data.trending} className="grid gap-3 sm:grid-cols-2" />
                <Link href="/trending" className="mt-3 inline-block text-base text-ink-2 hover:text-ink hover:underline">
                  See all trending repositories
                </Link>
              </div>
            ) : null}
            {data.agents ? <TopAgents entries={data.agents} limit={5} /> : null}
          </div>
        </Section>
      ) : null}

      <section id="self-host" aria-labelledby="self-host-title" className="scroll-mt-20">
        <div className="grid gap-8 lg:grid-cols-[minmax(0,1fr)_minmax(0,480px)] lg:gap-14">
          <div className="min-w-0">
            <h2 id="self-host-title" className="text-xl font-semibold tracking-tight">
              Self-host for your company
            </h2>
            <p className="mt-2 max-w-[52ch] text-md text-ink-2">The enterprise edition is the same server and web app, run on your own machines. Your code never leaves them.</p>
            <ul className="mt-4 flex max-w-[52ch] flex-col gap-2 text-base text-ink-2">
              <li>Starts from one Docker Compose file.</li>
              <li>Single sign-on with SAML or OIDC, and SCIM provisioning.</li>
              <li>An admin console for policy, domains and access.</li>
            </ul>
            <div className="mt-6 flex flex-wrap gap-2">
              <ButtonLink href="/pricing#self-hosted">Enterprise edition</ButtonLink>
              <a href={SELF_HOSTING_URL} target="_blank" rel="noreferrer noopener" className={buttonClass("ghost", "md")}>
                Self-hosting guide
              </a>
            </div>
          </div>
          <div className="min-w-0">
            <PromptCard id="self-host-docker" vars={{ endpoint: targets.endpoint }} compact />
          </div>
        </div>
      </section>

      <MarketingFooter />
    </div>
  );
}

function Section({ id, title, lead, children }: { id: string; title: string; lead?: string; children: ReactNode }) {
  return (
    <section id={id} aria-labelledby={id + "-title"} className="scroll-mt-20">
      <h2 id={id + "-title"} className="text-xl font-semibold tracking-tight">
        {title}
      </h2>
      {lead ? <p className="mt-2 max-w-[62ch] text-md text-ink-2">{lead}</p> : null}
      <div className="mt-6">{children}</div>
    </section>
  );
}

function Feature({ title, body, children }: { title: string; body: string; children: ReactNode }) {
  return (
    <article className="flex min-w-0 flex-col gap-4">
      <div>
        <h3 className="text-lg font-semibold">{title}</h3>
        <p className="mt-1.5 text-base text-ink-2">{body}</p>
      </div>
      <div className="mt-auto">{children}</div>
    </article>
  );
}

/** A stat strip: the numbers the data backs, each `.num` value followed by its label. Absent numbers are omitted. */
function Counts({ counts }: { counts: LandingCounts }) {
  const items: [number, string][] = [];
  if (counts.publicRepos !== undefined) items.push([counts.publicRepos, counts.publicRepos === 1 ? "public repository" : "public repositories"]);
  if (counts.landsThisWeek !== undefined) items.push([counts.landsThisWeek, (counts.landsThisWeek === 1 ? "land" : "lands") + " this week"]);
  if (counts.activeAgents !== undefined) items.push([counts.activeAgents, (counts.activeAgents === 1 ? "agent" : "agents") + " landing this week"]);
  if (!items.length) return null;
  return (
    <dl data-testid="landing-counts" className="mt-10 flex flex-wrap gap-x-6 gap-y-3">
      {items.map(([n, label]) => (
        <div key={label} className="flex items-baseline gap-1.5">
          <dt className="sr-only">{label}</dt>
          <dd className="num text-lg font-semibold">{compact(n)}</dd>
          <dd className="text-sm text-ink-3" aria-hidden="true">
            {label}
          </dd>
        </div>
      ))}
    </dl>
  );
}

function LivePanel({ items }: { items: PublicLand[] | null }) {
  return (
    <Panel title="Live right now" id="live-now" actions={<LiveDot label="Public lands" />}>
      {!items ? (
        <p className="py-8 text-base text-ink-3">Public activity is not available on this server.</p>
      ) : items.length ? (
        <ol className="divide-y divide-line" data-testid="public-activity">
          {items.map((it) => (
            <LiveRow key={it.repo.fullName + (it.changeset.changeId?.raw ?? it.changeset.meta?.message)} item={it} />
          ))}
        </ol>
      ) : (
        <p className="py-8 text-base text-ink-3">Nothing has landed in a public repository yet.</p>
      )}
    </Panel>
  );
}

function LiveRow({ item }: { item: PublicLand }) {
  const meta = item.changeset.meta;
  const author = (meta?.author ?? "").replace(/\s*<.*>$/, "") || "unknown";
  const principal = principalOf({ id: meta?.creator || author, kind: meta?.provenance?.runtime ? "PRINCIPAL_KIND_AGENT" : "PRINCIPAL_KIND_HUMAN", display: author, runtime: meta?.provenance?.runtime });
  return (
    <li className="flex min-h-[52px] items-center gap-3 py-2.5" data-testid="public-land">
      <PrincipalMark principal={principal} size={28} />
      <div className="min-w-0 flex-1 text-base">
        <p className="truncate">
          <PersonName display={author} className="font-medium" /> <span className="text-ink-2">landed</span> “{meta?.message.split("\n")[0]}”
        </p>
        <p className="truncate text-sm text-ink-3">
          <Link href={repoHref(item.repo.fullName)} className="hover:underline">
            {item.repo.fullName}
          </Link>
        </p>
      </div>
      <Time iso={meta?.committerTime?.time} className="num shrink-0 text-sm text-ink-3" />
    </li>
  );
}

function HowAgents({ repo, endpoint }: { repo: string | undefined; endpoint: string }) {
  const name = repo ?? "your-org/your-repo";
  const op: Op = { service: "ChangeService", method: "ListChanges", request: { repo: { fullName: name }, state: "CHANGE_STATE_OPEN" }, label: "List open changes" };
  return (
    <>
      <div className="grid gap-3 lg:grid-cols-3">
        <CodeSample label="CLI" text={toCli(op)} />
        <CodeSample label="MCP" text={toMcp(op)} />
        <CodeSample label="HTTP" text={toCurl(op, endpoint)} />
      </div>
      <p className="mt-3 text-sm text-ink-3">{repo ? "Shown for " + repo + "." : "Replace your-org/your-repo with a repository you can read."} Set BEE_TOKEN or send a bearer token for repositories that are not public.</p>
    </>
  );
}
