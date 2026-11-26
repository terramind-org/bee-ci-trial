import { SectionsSkeleton } from "@/components/inbox/inbox";
import { Line, Skel } from "@/components/skeletons";

/**
 * Needs you while it loads: the label, a line where the large sentence will be, and the list.
 * Signed-out visitors get the same frame for a moment; the landing page then streams in.
 */
export default function Loading() {
  return (
    <Skel>
      <div className="mb-9">
        <p className="eyebrow mb-4">Needs you · all repositories</p>
        <Line w="min(22ch, 80%)" lh={48} bar={26} />
      </div>
      <SectionsSkeleton />
    </Skel>
  );
}
