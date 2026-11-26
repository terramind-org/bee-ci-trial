import { HeaderSkeleton, PanelSkeleton, Skel } from "@/components/skeletons";

export default function Loading() {
  return (
    <Skel>
      <HeaderSkeleton title="Teams" metaWidth={460} />
      <PanelSkeleton rows={6} rowHeight={56} />
    </Skel>
  );
}
