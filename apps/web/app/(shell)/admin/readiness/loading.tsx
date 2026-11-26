import { PanelSkeleton, Skel } from "@/components/skeletons";

export default function Loading() {
  return (
    <Skel>
      <div className="grid grid-cols-1 gap-5 xl:grid-cols-[minmax(0,3fr)_minmax(0,2fr)]">
        <PanelSkeleton title="Checklist" rows={13} rowHeight={68} actions />
        <PanelSkeleton title="Configuration" rows={1} rowHeight={430} lines={1} />
      </div>
    </Skel>
  );
}
