import { HeaderSkeleton, PanelSkeleton, Skel, TabsSkeleton } from "@/components/skeletons";

export default function Loading() {
  return (
    <Skel>
      <HeaderSkeleton title="Settings" metaWidth={520} />
      <TabsSkeleton labels={["General", "Members", "Teams", "Apps", "Audit log"]} counts={false} active={3} />
      <div className="flex flex-col gap-5">
        <PanelSkeleton title="Owned by this org" rows={3} rowHeight={52} />
        <PanelSkeleton title="Installed on this org" rows={3} rowHeight={52} />
      </div>
    </Skel>
  );
}
