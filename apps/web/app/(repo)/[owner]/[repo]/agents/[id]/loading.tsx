import { CrumbSkeleton, DetailHeaderSkeleton, PanelSkeleton, Skel, StatsSkeleton } from "@/components/skeletons";

export default function Loading() {
  return (
    <Skel>
      <CrumbSkeleton w={120} />
      <DetailHeaderSkeleton height={76} titleW={220} />
      <StatsSkeleton labels={["Open changes", "Landed, 7 days", "Spend, 7 days", "Active leases", "Last seen"]} />
      <div className="flex flex-col gap-5">
        <PanelSkeleton title="Delegated tokens" rows={2} rowHeight={83} actions />
        <PanelSkeleton title="Leases held" rows={1} rowHeight={65} actions />
        <PanelSkeleton title="Provenance" rows={4} rowHeight={64} actions />
      </div>
    </Skel>
  );
}
