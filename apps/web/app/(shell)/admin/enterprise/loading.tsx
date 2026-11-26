import { PanelSkeleton, Skel } from "@/components/skeletons";

export default function Loading() {
  return (
    <Skel>
      <div className="grid grid-cols-1 gap-5 xl:grid-cols-[minmax(0,2fr)_minmax(0,3fr)]">
        <PanelSkeleton title="Details" rows={5} rowHeight={72} lines={1} />
        <div className="flex flex-col gap-5">
          <PanelSkeleton title="Organizations" rows={2} rowHeight={44} lines={1} />
          <PanelSkeleton title="Owners" rows={2} rowHeight={44} lines={1} />
        </div>
      </div>
    </Skel>
  );
}
