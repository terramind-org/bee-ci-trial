import { PanelSkeleton, Skel } from "@/components/skeletons";

export default function Loading() {
  return (
    <Skel>
      <div className="grid grid-cols-1 gap-5 xl:grid-cols-[minmax(0,3fr)_minmax(0,1fr)]">
        <PanelSkeleton title="Enterprise policies" rows={6} rowHeight={72} lines={1} />
        <PanelSkeleton title="Anonymous read" rows={1} rowHeight={96} lines={1} />
      </div>
    </Skel>
  );
}
