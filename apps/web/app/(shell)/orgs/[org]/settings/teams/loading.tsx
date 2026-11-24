import { HeaderSkeleton, PanelSkeleton, Skel, TabsSkeleton } from "@/components/skeletons";

export default function Loading() {
  return (
    <Skel>
      <HeaderSkeleton title="Settings" metaWidth={460} />
      <TabsSkeleton labels={["General", "Members", "Teams", "Apps", "Audit log"]} counts={false} active={2} />
      <PanelSkeleton rows={6} rowHeight={56} />
    </Skel>
  );
}
