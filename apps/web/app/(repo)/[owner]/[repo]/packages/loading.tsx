import { HeaderSkeleton, ListSkeleton, Skel } from "@/components/skeletons";

export default function Loading() {
  return (
    <Skel>
      <HeaderSkeleton title="Packages" metaWidth={360} />
      <ListSkeleton rows={4} rowHeight={59} />
    </Skel>
  );
}
