import { Line, RowsSkeleton, Skel, TabsSkeleton } from "@/components/skeletons";

/** The pull-request list while it loads: the state tabs, then the first group's rows. */
export default function Loading() {
  return (
    <Skel>
      <TabsSkeleton labels={["Open", "Merged", "Closed", "All"]} />
      <div className="mb-8">
        <Line w={220} lh={32} bar={10} />
      </div>
      <p className="mb-3 text-xs font-medium text-ink-2">Needs you</p>
      <RowsSkeleton rows={4} />
    </Skel>
  );
}
