import { HeaderSkeleton, PanelSkeleton, Skel } from "@/components/skeletons";

export default function Loading() {
  return (
    <Skel>
      <HeaderSkeleton title="People and teams" metaWidth={360} />
      <PanelSkeleton rows={8} rowHeight={56} />
    </Skel>
  );
}
