import Link from "next/link";
import { Icon } from "@/components/icons";

/** A human-readable counterpart to the Atom alternate in the document metadata. */
export function FeedLink({ pagePath }: { pagePath: string }) {
  return (
    <Link href={pagePath.replace(/\/+$/, "") + "/feed.atom"} className="inline-flex h-8 items-center gap-1.5 text-base text-ink-2 hover:text-ink hover:underline">
      <Icon name="feed" size={16} />
      Activity feed
    </Link>
  );
}
