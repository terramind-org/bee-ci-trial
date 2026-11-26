import Link from "next/link";
import { buttonClass } from "@/components/ui";

/** "More" for a cursor-paged list: a link to the same view with the server's next-page token. */
export function Pager({ nextHref }: { nextHref: string | null }) {
  if (!nextHref) return null;
  return (
    <nav aria-label="Pages" className="mt-6 flex">
      <Link href={nextHref} className={buttonClass("secondary", "sm")}>
        Show more
      </Link>
    </nav>
  );
}

/** `/path?a=1&b=2` from the non-empty values. */
export function href(path: string, query: Record<string, string | undefined>): string {
  const qs = new URLSearchParams();
  for (const [k, v] of Object.entries(query)) if (v) qs.set(k, v);
  const s = qs.toString();
  return s ? path + "?" + s : path;
}
