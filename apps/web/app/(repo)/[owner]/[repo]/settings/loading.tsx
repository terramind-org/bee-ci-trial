import { HeaderSkeleton, PanelSkeleton, Skel } from "@/components/skeletons";

export default function Loading() {
  return (
    <Skel>
      <HeaderSkeleton title="Settings" metaWidth={520} />
      <PanelSkeleton rows={8} rowHeight={52} />
    </Skel>
  );
}
