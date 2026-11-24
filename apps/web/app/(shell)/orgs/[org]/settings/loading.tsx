import { HeaderSkeleton, PanelSkeleton, Skel, TabsSkeleton } from "@/components/skeletons";

export default function Loading() {
  return (
    <Skel>
      <HeaderSkeleton title="Settings" metaWidth={440} />
      <TabsSkeleton labels={["General", "Members", "Teams", "Apps", "Audit log"]} counts={false} active={-1} />
      <PanelSkeleton rows={8} rowHeight={52} />
    </Skel>
  );
}
