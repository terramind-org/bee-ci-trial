import { HeaderSkeleton, ListSkeleton, Skel } from "@/components/skeletons";

export default function Loading() {
  return (
    <Skel>
      <HeaderSkeleton />
      <ListSkeleton rows={8} rowHeight={59} />
    </Skel>
  );
}
