import { PanelSkeleton, Skel, StatsSkeleton } from "@/components/skeletons";

export default function Loading() {
  return (
    <Skel>
      <StatsSkeleton labels={["Status", "Version", "Edition", "Uptime", "Cell"]} />
      <div className="grid grid-cols-1 gap-5 xl:grid-cols-2">
        <PanelSkeleton title="Components" rows={7} rowHeight={52} />
        <PanelSkeleton title="Cells" rows={3} rowHeight={52} />
      </div>
    </Skel>
  );
}
