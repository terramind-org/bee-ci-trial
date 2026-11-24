import { HeaderSkeleton, PanelSkeleton, Skel, TabsSkeleton } from "@/components/skeletons";

export default function Loading() {
  return (
    <Skel>
      <HeaderSkeleton title="Settings" metaWidth={460} />
      <TabsSkeleton labels={["General", "Members", "Teams", "Apps", "Audit log"]} counts={false} active={4} />
      <PanelSkeleton rows={10} rowHeight={48} lines={1} />
    </Skel>
  );
}
