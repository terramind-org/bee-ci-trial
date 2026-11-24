/** Followers or following: one row per account, linking to its profile. */
import Link from "next/link";
import { PrincipalMark } from "@/components/domain";
import { EmptyState } from "@/components/ui";
import type { UserSummary } from "@/lib/api/types";
import { ownerHref } from "@/lib/routes";

export function UserList({ users, empty }: { users: UserSummary[]; empty: string }) {
  if (!users.length) return <EmptyState title={empty} icon="user" />;
  return (
    <ul className="rule divide-y divide-line" aria-label="Accounts">
      {users.map((u) => (
        <li key={u.login}>
          <Link href={ownerHref(u.login)} data-nav-item className="flex items-center gap-4 py-4 hover:bg-hover focus-visible:bg-hover">
            <PrincipalMark principal={u.principal ?? { id: u.login, kind: "PRINCIPAL_KIND_HUMAN", display: u.name || u.login, ownerId: "", runtime: "", externalIds: [] }} size={44} />
            <span className="min-w-0 flex-1">
              <span className="block truncate text-md font-medium">{u.name || u.login}</span>
              {u.name && u.name !== u.login ? <span className="block truncate text-base text-ink-3">{u.login}</span> : null}
              {u.bio ? <span className="mt-0.5 block truncate text-base text-ink-2">{u.bio}</span> : null}
            </span>
          </Link>
        </li>
      ))}
    </ul>
  );
}
