import { HeaderSkeleton, PanelSkeleton, Skel } from "@/components/skeletons";

export default function Loading() {
  return (
    <Skel>
      <HeaderSkeleton title="Billing" eyebrow metaWidth={180} />
      <div className="flex flex-col gap-10">
        <PanelSkeleton title="Current plan" rows={3} rowHeight={40} />
        <PanelSkeleton title="Usage this cycle" rows={4} rowHeight={58} />
        <PanelSkeleton title="Seats" rows={3} rowHeight={58} />
      </div>
    </Skel>
  );
}
