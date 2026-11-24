import { Skel } from "@/components/skeletons";
import { SectionsSkeleton } from "@/components/inbox/inbox";
import { Skeleton } from "@/components/ui";

/**
 * The Inbox's frame at its rendered widths: the title, the lede and the sections. Signed-out
 * visitors get the same frame for a moment; the landing then streams in.
 */
export default function Loading() {
  return (
    <Skel>
      <div className="mb-10">
        <h1 className="text-2xl font-semibold tracking-tight text-ink">Inbox</h1>
        <Skeleton className="mt-3 h-5 w-80 max-w-full" />
      </div>
      <SectionsSkeleton />
    </Skel>
  );
}
