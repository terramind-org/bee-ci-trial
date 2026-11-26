/** The top of a user, agent or organization profile: a large mark beside the name, who they are, and where to find them. */
import Link from "next/link";
import type { ReactNode } from "react";
import { Avatar } from "@/components/avatar";
import { PrincipalMark } from "@/components/domain";
import { FollowButton } from "@/components/follow-button";
import { Icon } from "@/components/icons";
import { Badge } from "@/components/ui";
import type { Principal, Profile } from "@/lib/api/types";
import { compact } from "@/lib/format";
import { ownerHref } from "@/lib/routes";

/** The principal the profile stands for, so the mark has the right shape (circle for people, square for agents). */
export function profilePrincipal(p: Profile): Principal {
  return { id: p.principalId, kind: p.kind === "agent" ? "PRINCIPAL_KIND_AGENT" : "PRINCIPAL_KIND_HUMAN", display: p.name || p.login, ownerId: "", runtime: p.runtime ?? "", externalIds: [] };
}

const MARK = 96;

/** Company, place and website as one quiet line. */
export function ProfileFacts({ profile }: { profile: Profile }) {
  const site = profile.website && /^https?:\/\//.test(profile.website) ? profile.website : "";
  const facts: ReactNode[] = [];
  if (profile.company)
    facts.push(
      <span key="company" className="inline-flex items-center gap-1.5">
        <Icon name="team" className="text-ink-3" />
        <span className="sr-only">Company: </span>
        {profile.company}
      </span>,
    );
  if (profile.location)
    facts.push(
      <span key="location" className="inline-flex items-center gap-1.5">
        <Icon name="pages" className="text-ink-3" />
        <span className="sr-only">Location: </span>
        {profile.location}
      </span>,
    );
  if (site)
    facts.push(
      <a key="site" href={site} rel="noreferrer noopener" target="_blank" className="inline-flex min-w-0 max-w-full items-center gap-1.5 text-accent-ink hover:underline">
        <Icon name="external" className="shrink-0 text-ink-3" />
        <span className="sr-only">Website: </span>
        <span className="truncate">{site.replace(/^https?:\/\//, "")}</span>
      </a>,
    );
  if (!facts.length) return null;
  return <div className="flex flex-wrap items-center gap-x-5 gap-y-1 text-base text-ink-2">{facts}</div>;
}

export function ProfileCard({ profile, signedIn, isSelf }: { profile: Profile; signedIn: boolean; isSelf: boolean }) {
  const agent = profile.kind === "agent";
  const base = ownerHref(profile.login);
  return (
    <section aria-label="Profile" className="mb-10 flex min-w-0 flex-col gap-6 sm:flex-row sm:items-start">
      <div className="shrink-0">
        <PrincipalMark principal={profilePrincipal(profile)} size={MARK} />
      </div>
      <div className="flex min-w-0 flex-1 flex-col gap-3">
        <div className="min-w-0">
          <h1 className="text-2xl font-semibold leading-tight tracking-tight [overflow-wrap:anywhere] sm:text-3xl">{profile.name || profile.login}</h1>
          <p className="mt-1 text-lg text-ink-3 [overflow-wrap:anywhere]">{profile.login}</p>
        </div>
        {agent ? (
          <div className="flex flex-wrap items-center gap-2">
            <Badge>agent</Badge>
            {profile.runtime ? <Badge tone="outline">{profile.runtime}</Badge> : null}
            {profile.ownerLogin ? (
              <span className="text-base text-ink-2">
                Run by{" "}
                <Link href={ownerHref(profile.ownerLogin)} className="text-accent-ink underline underline-offset-2">
                  {profile.ownerLogin}
                </Link>
              </span>
            ) : null}
          </div>
        ) : null}
        {profile.bio ? <p className="max-w-2xl text-lg text-ink [overflow-wrap:anywhere]">{profile.bio}</p> : null}
        <ProfileFacts profile={profile} />
        <div className="flex flex-wrap items-center gap-x-6 gap-y-3 pt-1">
          {isSelf ? null : <FollowButton target={profile.login} following={profile.followingByMe} count={profile.followers} signedIn={signedIn} />}
          <p className="flex flex-wrap items-center gap-x-5 gap-y-1 text-base text-ink-2">
            <Link href={base + "?tab=followers"} className="inline-flex h-8 items-center hover:underline">
              <span className="num mr-1 font-medium text-ink">{compact(profile.followers)}</span> {profile.followers === 1 ? "follower" : "followers"}
            </Link>
            <Link href={base + "?tab=following"} className="inline-flex h-8 items-center hover:underline">
              <span className="num mr-1 font-medium text-ink">{compact(profile.following)}</span> following
            </Link>
          </p>
        </div>
        {profile.orgs.length ? (
          <div className="flex flex-wrap items-center gap-x-4 gap-y-1 text-base text-ink-2">
            <h2 className="text-base font-normal text-ink-3">Organizations</h2>
            <ul className="flex flex-wrap gap-x-4 gap-y-1">
              {profile.orgs.map((o) => (
                <li key={o}>
                  <Link href={ownerHref(o)} className="inline-flex h-7 items-center text-ink hover:underline">
                    {o}
                  </Link>
                </li>
              ))}
            </ul>
          </div>
        ) : null}
      </div>
    </section>
  );
}

/** The mark of an organization: its login seeds a service tile, so it never looks like a person. */
export function OrgMark({ profile, size = MARK }: { profile: Profile; size?: number }) {
  return <Avatar principal={{ id: profile.principalId || profile.login, name: profile.name || profile.login, kind: "service", avatarUrl: profile.avatar }} size={size} />;
}
