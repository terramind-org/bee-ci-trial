import { HeaderSkeleton, PanelSkeleton, Skel, TabsSkeleton } from "@/components/skeletons";

export default function Loading() {
  return (
    <Skel>
      <HeaderSkeleton title="Record" metaWidth={460} />
      <TabsSkeleton labels={["General", "Members", "Teams", "Apps", "Record"]} counts={false} active={4} />
      <PanelSkeleton rows={10} rowHeight={48} lines={1} />
    </Skel>
  );
}
