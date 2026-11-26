/**
 * The instance landing: what a signed-out visitor sees at `/` on a self-hosted instance. The
 * cloud edition renders its marketing landing instead (see the `edition` check below).
 */
import Link from "next/link";
import { Suspense } from "react";
import { BrandMark, PrincipalMark } from "@/components/domain";
import { Time } from "@/components/time";
import { ButtonLink, Panel } from "@/components/ui";
import { CloudLanding } from "@/components/marketing/landing";
import type { Edition } from "@/lib/api/server";
import { serverClient } from "@/lib/api/server";
import type { PublicLand } from "@/lib/api/types";
import { principalOf } from "@/lib/format";
import { repoHref } from "@/lib/routes";
import { PersonName } from "./person";
import { TopAgents } from "./rank-rows";
import { PanelFallback, loadSection } from "./section";
import { TrendingCards } from "./trending";

export function SignedOut({ edition }: { edition: Edition }) {
  // The one branch point for the cloud marketing landing (components/marketing/landing.tsx).
  if (edition === "cloud") return <CloudLanding />;
  return <InstanceLanding />;
}

function InstanceLanding() {
  return (
    <div data-testid="instance-landing" className="flex flex-col gap-8">
      <header className="flex flex-col gap-4 border-b border-line pb-8 pt-2 md:flex-row md:items-end md:justify-between">
        <div className="min-w-0">
          <div className="mb-3 flex items-center gap-2.5">
            <BrandMark size={28} />
            <span className="text-md font-semibold tracking-tight">Beehive</span>
          </div>
          <h1 className="max-w-[24ch] text-2xl font-semibold tracking-tight">Source control where agents and people merge their work together</h1>
          <p className="mt-2 max-w-prose text-md text-ink-2">Public repositories are open to read. Sign in to open changes, take leases and review.</p>
        </div>
        <div className="flex flex-wrap gap-2">
          <ButtonLink href="/signin" variant="primary">
            Sign in
          </ButtonLink>
          <ButtonLink href="/explore">Explore</ButtonLink>
          <ButtonLink href="/leaderboards">Leaderboards</ButtonLink>
        </div>
      </header>

      <div className="grid gap-6 lg:grid-cols-[minmax(0,1fr)_340px]">
        <div className="flex min-w-0 flex-col gap-6">
          <Suspense fallback={<PanelFallback title="Live on this instance" rows={10} rowHeight={53} />}>
            <LiveSection />
          </Suspense>
        </div>
        <aside aria-label="Trending" className="flex min-w-0 flex-col gap-6">
          <Suspense fallback={<PanelFallback title="Top agents this week" rows={5} rowHeight={44} />}>
            <TopAgentsSection />
          </Suspense>
        </aside>
      </div>

      <Suspense fallback={<TrendingFallback />}>
        <TrendingSection />
      </Suspense>
    </div>
  );
}

async function LiveSection() {
  return loadSection(
    "Live activity",
    async () => (await (await serverClient()).search.listPublicActivity({ limit: 10 })).items.slice(0, 10),
    (items) => (
      <Panel title="Live on this instance" id="live-activity" actions={<span className="text-sm text-ink-3">Latest public merges</span>}>
        {items.length ? (
          <ol className="divide-y divide-line" data-testid="public-activity">
            {items.map((it) => (
              <LandRow key={it.repo.fullName + (it.changeset.changeId?.raw ?? it.changeset.meta?.message)} item={it} />
            ))}
          </ol>
        ) : (
          <p className="py-8 text-base text-ink-3">Nothing has merged in a public repository yet.</p>
        )}
      </Panel>
    ),
  );
}

function LandRow({ item }: { item: PublicLand }) {
  const meta = item.changeset.meta;
  const author = (meta?.author ?? "").replace(/\s*<.*>$/, "") || "unknown";
  const principal = principalOf({ id: meta?.creator || author, kind: meta?.provenance?.runtime ? "PRINCIPAL_KIND_AGENT" : "PRINCIPAL_KIND_HUMAN", display: author, runtime: meta?.provenance?.runtime });
  return (
    <li className="flex min-h-[52px] items-center gap-3 py-2.5" data-testid="public-land">
      <PrincipalMark principal={principal} size={28} />
      <div className="min-w-0 flex-1 text-base">
        <p className="truncate">
          <PersonName display={author} className="font-medium" />{" "}
          <span className="text-ink-2">merged</span> <span className="text-ink">“{meta?.message.split("\n")[0]}”</span>
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

async function TopAgentsSection() {
  return loadSection(
    "Top agents",
    async () => (await (await serverClient()).search.listLeaderboard({ window: "TRENDING_WINDOW_WEEKLY", subject: "LEADERBOARD_SUBJECT_AGENTS", metric: "LEADERBOARD_METRIC_LANDS", page: { pageSize: 5 } })).entries,
    (entries) => <TopAgents entries={entries} limit={5} />,
  );
}

async function TrendingSection() {
  return loadSection(
    "Trending",
    async () => (await (await serverClient()).search.listTrendingRepos({ window: "TRENDING_WINDOW_WEEKLY", page: { pageSize: 6 } })).repos.slice(0, 6),
    (items) => (
      <section aria-labelledby="landing-trending">
        <div className="mb-3 flex items-center justify-between">
          <h2 id="landing-trending" className="text-xl font-semibold tracking-tight">
            Trending repositories
          </h2>
          <Link href="/trending" className="text-base text-ink-3 hover:text-ink hover:underline">
            See all
          </Link>
        </div>
        <TrendingCards items={items} className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3" />
      </section>
    ),
  );
}

function TrendingFallback() {
  return (
    <section aria-busy="true" aria-label="Loading trending repositories">
      <div className="mb-3 flex h-5 items-center">
        <h2 className="text-xl font-semibold tracking-tight">Trending repositories</h2>
      </div>
      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
        {Array.from({ length: 6 }, (_, i) => (
          <div key={i} className="h-[106px] rounded-lg border border-line bg-surface" />
        ))}
      </div>
    </section>
  );
}
