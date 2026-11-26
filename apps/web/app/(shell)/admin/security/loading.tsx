import { PanelSkeleton, Skel } from "@/components/skeletons";

export default function Loading() {
  return (
    <Skel>
      <PanelSkeleton title="IP allow list" rows={5} rowHeight={64} lines={1} />
    </Skel>
  );
}
