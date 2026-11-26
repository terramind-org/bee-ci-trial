import type { Metadata } from "next";
import Link from "next/link";
import { notFound } from "next/navigation";
import { PageOp } from "@/components/copy-as";
import { Heatmap } from "@/components/heatmap";
import { Panel, TabLinks, buttonClass } from "@/components/ui";
import { Achievements } from "@/components/profile/achievements";
import { newestLands, RecentActivity, type ProfileLand } from "@/components/profile/activity";
import { People, TopTopics, topTopics } from "@/components/profile/org-people";
import { OrgMark, ProfileCard, ProfileFacts } from "@/components/profile/profile-card";
import { ProfileReadme } from "@/components/profile/readme";
import { UserList } from "@/components/profile/user-list";
import { COMMON_LANGUAGES, MEMORY_LIMIT, listHref, ownerQuery, parseListParams, type ListTarget } from "@/components/repo/query";
import { RepoFilters } from "@/components/repo/repo-filters";
import { NoRepoResults as NoRepos, PageNav as Pager, RepoEntries as RepoHits, StarredEntries as StarredRows } from "@/components/discover/repo-entry";
import { searchPage } from "@/components/repo/search-hits";
import { FeedLink } from "@/components/repo/feed-link";
import { currentPrincipal, serverClient } from "@/lib/api/server";
import type { BeeClient } from "@/lib/api/client";
import { BeeError } from "@/lib/api/transport";
import type { Member, Principal, Profile, SearchRepoHit } from "@/lib/api/types";
import { guard, param, type SearchParams } from "@/lib/page";
import { feedAlternates } from "@/lib/atom";
import { isReservedOwner, orgSettingsHref, ownerHref } from "@/lib/routes";

type Params = Promise<{ owner: string }>;

export async function generateMetadata({ params }: { params: Params }): Promise<Metadata> {
  const owner = decodeURIComponent((await params).owner);
  return { title: owner, alternates: feedAlternates(ownerHref(owner), owner) };
}

const TABS = ["overview", "repositories", "stars", "followers", "following"] as const;
type Tab = (typeof TABS)[number];

/** A part of the page the server may not offer (UNIMPLEMENTED) or may hide from this viewer: the rest still renders. */
async function optional<X>(p: Promise<X>): Promise<{ value: X } | { unavailable: string }> {
  try {
    return { value: await p };
  } catch (err) {
    if (err instanceof BeeError && ["unimplemented", "unauthenticated", "permission_denied"].includes(err.code)) return { unavailable: err.reason };
    throw err;
  }
}

/** Newest landed changeset(s) of each repo, so the profile shows what is happening now. */
async function landsOf(api: BeeClient, repos: SearchRepoHit[], perRepo: number): Promise<ProfileLand[]> {
  const per = await Promise.all(
    repos.map(async (h): Promise<ProfileLand[]> => {
      try {
        const log = await api.query.log({ repo: { fullName: h.fullName }, from: { expr: "main" }, firstParent: true, page: { pageSize: perRepo } });
        return log.changesets.map((changeset) => ({ repo: h.fullName, changeset }));
      } catch (err) {
        // A repo with nothing landed (or a different default branch) has no history to show.
        if (err instanceof BeeError && (err.code === "not_found" || err.code === "failed_precondition")) return [];
        throw err;
      }
    }),
  );
  return per.flat();
}

export default async function ProfilePage({ params, searchParams }: { params: Params; searchParams: SearchParams }) {
  const owner = decodeURIComponent((await params).owner);
  if (isReservedOwner(owner)) notFound();
  const sp = await searchParams;
  return guard(async () => {
    const api = await serverClient();
    const [profile, me] = await Promise.all([api.user.getProfile({ owner }), currentPrincipal()]);
    // The client is a Proxy (every property is a function), so it must not travel as a prop: React's
    // dev tools serialisation would take it for a thenable and wait on it forever.
    return profile.kind === "org" ? <OrgProfile profile={profile} me={me} sp={sp} /> : <PersonProfile profile={profile} me={me} sp={sp} />;
  });
}

async function PersonProfile({ profile, me, sp }: { profile: Profile; me: Principal | null; sp: Awaited<SearchParams> }) {
  const api = await serverClient();
  const login = profile.login;
  const base = ownerHref(login);
  const asked = param(sp, "tab");
  const tab: Tab = (TABS as readonly string[]).includes(asked) ? (asked as Tab) : "overview";
  const isSelf = !!me && me.display.toLowerCase() === login.toLowerCase();
  const p = parseListParams(sp, false);
  const target: ListTarget = { path: base, fixed: { tab: "repositories" } };
  const tabs = TABS.map((t) => ({
    href: t === "overview" ? base : base + "?tab=" + t,
    label: t[0]!.toUpperCase() + t.slice(1),
    active: t === tab,
    count: t === "repositories" ? profile.publicRepos : t === "followers" ? profile.followers : t === "following" ? profile.following : undefined,
  }));

  let main: React.ReactNode;
  let op: React.ComponentProps<typeof PageOp>["op"] = { service: "UserService", method: "GetProfile", request: { owner: login }, label: "Read this profile" };

  if (tab === "overview") {
    const popular = await api.search.searchRepos({ query: ownerQuery({ q: "", visibility: "", language: "", topic: "" }, "user", login), sort: "stars", page: { pageSize: 6 } });
    const [graph, achievements, lands] = await Promise.all([
      optional(api.user.getContributionGraph({ login })),
      optional(api.user.listAchievements({ login })),
      landsOf(api, popular.hits.slice(0, 4), 2),
    ]);
    main = (
      <div className="flex flex-col gap-12">
        {profile.readme ? <ProfileReadme html={profile.readme} login={login} /> : null}
        <section aria-labelledby="popular-heading">
          <div className="mb-3 flex min-h-9 items-center justify-between gap-4">
            <h2 id="popular-heading" className="text-xl font-semibold tracking-tight">
              Popular repositories
            </h2>
            {popular.hits.length ? (
              <Link href={base + "?tab=repositories"} className="text-base text-accent-ink hover:underline">
                All repositories
              </Link>
            ) : null}
          </div>
          <RepoHits hits={popular.hits} empty={<NoRepos filtered={false} clearHref={base}>No repositories to show yet.</NoRepos>} />
        </section>
        <Panel title="Contributions" id="contributions-heading">
          {"value" in graph ? (
            <Heatmap days={graph.value.days} end={graph.value.toDate || undefined} />
          ) : (
            <p className="text-base text-ink-3">The contribution graph is not available on this server ({graph.unavailable}).</p>
          )}
        </Panel>
        {"value" in achievements ? <Achievements achievements={achievements.value.achievements} /> : null}
        <RecentActivity lands={newestLands(lands, 6)} />
      </div>
    );
  } else if (tab === "repositories") {
    const r = await searchPage(api, ownerQuery(p, "user", login), p);
    const filtered = !!(p.q || p.visibility || p.language || p.topic);
    const clear = listHref({ q: "", visibility: "", language: "", topic: "", cursor: "", page: 1 }, p, false, target);
    const langs = [...new Set([...COMMON_LANGUAGES, ...r.languages.filter(Boolean)])].sort((a, b) => a.localeCompare(b));
    const more = r.hasMoreLocal || !!r.next;
    op = { service: "SearchService", method: "SearchRepos", request: { query: ownerQuery(p, "user", login), sort: p.sort === "stars" ? "stars" : "updated" }, label: "Search " + login + "'s repositories" };
    main = (
      <div className="flex flex-col gap-2">
        <RepoFilters params={p} signedIn={false} languages={langs} target={target} />
        <RepoHits hits={r.hits} empty={<NoRepos filtered={filtered} clearHref={clear}>{login} has no repositories to show.</NoRepos>} />
        <Pager
          label="Pages"
          first={p.cursor || p.page > 1 ? listHref({ cursor: "", page: 1 }, p, false, target) : undefined}
          next={!more ? undefined : r.next ? listHref({ cursor: r.next }, p, false, target) : listHref({ page: p.page + 1 }, p, false, target)}
        />
        {r.truncated ? <p className="text-sm text-ink-3">Showing the first {MEMORY_LIMIT} repositories, ordered by name.</p> : null}
      </div>
    );
  } else if (tab === "stars") {
    const res = await api.user.listStarred({ login, sort: "updated", page: { pageSize: 50 } });
    op = { service: "UserService", method: "ListStarred", request: { login, sort: "updated" }, label: "List repositories " + login + " starred" };
    main = <StarredRows repos={res.repos} empty={<NoRepos filtered={false} clearHref={base}>{login} has not starred anything yet.</NoRepos>} />;
  } else {
    const res = tab === "followers" ? await api.user.listFollowers({ login, page: { pageSize: 50 } }) : await api.user.listFollowing({ login, page: { pageSize: 50 } });
    op = { service: "UserService", method: tab === "followers" ? "ListFollowers" : "ListFollowing", request: { login }, label: "List " + tab + " of " + login };
    main = <UserList users={res.users} empty={tab === "followers" ? "No followers yet" : "Not following anyone yet"} />;
  }

  return (
    <>
      <PageOp op={op} />
      <ProfileCard profile={profile} signedIn={!!me} isSelf={isSelf} />
      <FeedLink pagePath={base} />
      <TabLinks label="Profile sections" items={tabs} />
      <div className="min-w-0">{main}</div>
    </>
  );
}

async function OrgProfile({ profile, me, sp }: { profile: Profile; me: Principal | null; sp: Awaited<SearchParams> }) {
  const api = await serverClient();
  const login = profile.login;
  const p = parseListParams(sp, false);
  const target: ListTarget = { path: ownerHref(login) };
  const query = ownerQuery(p, "org", login);
  const [page, topicHits, members] = await Promise.all([
    searchPage(api, query, p),
    api.search.searchRepos({ query: ownerQuery({ q: "", visibility: "", language: "", topic: "" }, "org", login), sort: "stars", page: { pageSize: 100 } }),
    me ? optional(api.org.listMembers({ org: login })) : Promise.resolve({ unavailable: "UNAUTHENTICATED" }),
  ]);
  const people: Member[] | null = "value" in members ? members.value.members : null;
  const mine = people?.find((m) => me && m.principal?.id === me.id);
  const admin = mine?.role === "ROLE_OWNER" || mine?.role === "ROLE_ADMIN";
  const filtered = !!(p.q || p.visibility || p.language || p.topic);
  const clear = listHref({ q: "", visibility: "", language: "", topic: "", cursor: "", page: 1 }, p, false, target);
  const langs = [...new Set([...COMMON_LANGUAGES, ...topicHits.hits.map((h) => h.primaryLanguage).filter(Boolean)])].sort((a, b) => a.localeCompare(b));
  const more = page.hasMoreLocal || !!page.next;

  return (
    <>
      <PageOp op={{ service: "SearchService", method: "SearchRepos", request: { query, sort: p.sort === "stars" ? "stars" : "updated" }, label: "Search " + login + "'s repositories" }} />
      <section aria-label="Organization" className="mb-10 flex min-w-0 flex-col gap-6 sm:flex-row sm:items-start">
        <div className="shrink-0">
          <OrgMark profile={profile} />
        </div>
        <div className="flex min-w-0 flex-1 flex-col gap-3">
          <div className="min-w-0">
            <h1 className="text-2xl font-semibold leading-tight tracking-tight [overflow-wrap:anywhere] sm:text-3xl">{profile.name || login}</h1>
            <p className="mt-1 text-lg text-ink-3 [overflow-wrap:anywhere]">{login}</p>
          </div>
          {profile.bio ? <p className="max-w-2xl text-lg text-ink [overflow-wrap:anywhere]">{profile.bio}</p> : null}
          <ProfileFacts profile={profile} />
          <FeedLink pagePath={ownerHref(login)} />
          {admin ? (
            <div className="pt-1">
              <Link href={orgSettingsHref(login)} className={buttonClass("secondary", "md")}>
                Organization settings
              </Link>
            </div>
          ) : null}
        </div>
      </section>
      <div className="grid grid-cols-1 gap-x-14 gap-y-12 lg:grid-cols-[minmax(0,1fr)_300px]">
        <section aria-labelledby="org-repos-heading" className="flex min-w-0 flex-col gap-3">
          <h2 id="org-repos-heading" className="text-xl font-semibold tracking-tight">
            Repositories <span className="num font-normal text-ink-3">{page.total}</span>
          </h2>
          <RepoFilters params={p} signedIn={false} languages={langs} target={target} />
          <RepoHits hits={page.hits} empty={<NoRepos filtered={filtered} clearHref={clear}>{login} has no repositories you can see.</NoRepos>} />
          <Pager
            label="Pages"
            first={p.cursor || p.page > 1 ? listHref({ cursor: "", page: 1 }, p, false, target) : undefined}
            next={!more ? undefined : page.next ? listHref({ cursor: page.next }, p, false, target) : listHref({ page: p.page + 1 }, p, false, target)}
          />
          {page.truncated ? <p className="text-sm text-ink-3">Showing the first {MEMORY_LIMIT} repositories, ordered by name.</p> : null}
        </section>
        <aside className="flex min-w-0 flex-col gap-10" aria-label="Organization details">
          <People members={people} />
          <TopTopics topics={topTopics(topicHits.hits)} />
        </aside>
      </div>
    </>
  );
}
