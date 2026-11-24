import { HeaderSkeleton, ListSkeleton, Skel, TabsSkeleton } from "@/components/skeletons";

export default function Loading() {
  return (
    <Skel>
      <HeaderSkeleton title="Work" meta="Issues, sub-issues and the board. Assign one to an agent and it starts." />
      <TabsSkeleton labels={["Open", "Closed"]} />
      <ListSkeleton rows={10} rowHeight={59} placeholder="Filter by title, label, assignee or type…" />
    </Skel>
  );
}
