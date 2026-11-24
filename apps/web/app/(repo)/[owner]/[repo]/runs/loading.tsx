import { HeaderSkeleton, ListSkeleton, Skel, TabsSkeleton } from "@/components/skeletons";

export default function Loading() {
  return (
    <Skel>
      <HeaderSkeleton title="Runs" meta="Workflow runs, their jobs and the checks they report." />
      <TabsSkeleton labels={["All", "Running", "Failed", "Passed"]} />
      <ListSkeleton rows={10} rowHeight={59} placeholder="Filter by workflow, change, ref or actor…" />
    </Skel>
  );
}
