import { HeaderSkeleton, PanelSkeleton, Skel, TabsSkeleton } from "@/components/skeletons";

export default function Loading() {
  return (
    <Skel>
      <HeaderSkeleton title="Settings" metaWidth={300} />
      <TabsSkeleton labels={["General", "Members", "Teams", "Apps", "Audit log", "Billing"]} counts={false} active={-1} />
      <div className="flex flex-col gap-5">
        <PanelSkeleton title="Current plan" rows={2} rowHeight={40} />
        <div className="grid gap-5 lg:grid-cols-2">
          <PanelSkeleton title="Usage this cycle" rows={5} rowHeight={37} />
          <PanelSkeleton title="Seats" rows={5} rowHeight={45} />
        </div>
      </div>
    </Skel>
  );
}
