/** A person's or agent's name. Links to the profile only when the display name is a valid login. */
import Link from "next/link";
import { ownerHref, isReservedOwner } from "@/lib/routes";

/**
 * A principal carries a display name, not a login. Where the display is itself a valid login (as
 * for agents) it links to the profile; a name with spaces cannot be a login, so it stays plain
 * text instead of linking to a page that is not there.
 */
export function loginFrom(display: string): string | null {
  return /^[\w.-]+$/.test(display) && !isReservedOwner(display) ? display : null;
}

export function PersonName({ display, className }: { display: string; className?: string }) {
  const login = loginFrom(display);
  return login ? (
    <Link href={ownerHref(login)} className={(className ?? "") + " hover:underline"}>
      {display}
    </Link>
  ) : (
    <span className={className}>{display}</span>
  );
}
