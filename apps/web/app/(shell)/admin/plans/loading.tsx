import { PanelSkeleton, Skel } from "@/components/skeletons";

export default function Loading() {
  return (
    <Skel>
      <div className="grid grid-cols-1 gap-5 xl:grid-cols-[minmax(0,2fr)_minmax(0,3fr)]">
        <PanelSkeleton title="Assign a plan" rows={2} rowHeight={72} lines={1} />
        <PanelSkeleton title="Current plans" rows={3} rowHeight={44} lines={1} />
      </div>
    </Skel>
  );
}
