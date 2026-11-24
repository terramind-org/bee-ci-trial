import { HeaderSkeleton, ListSkeleton, Skel } from "@/components/skeletons";

export default function Loading() {
  return (
    <Skel>
      <HeaderSkeleton title="Agents" meta="Every agent has an owner, a runtime and tokens no broader than its owner’s. Work is attributable end to end." />
      <ListSkeleton rows={6} rowHeight={77} placeholder="Filter by name, runtime, owner or model…" glyph="square" />
    </Skel>
  );
}
