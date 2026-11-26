import { HeaderSkeleton, PanelSkeleton, Skel } from "@/components/skeletons";

export default function Loading() {
  return (
    <Skel>
      <HeaderSkeleton title="Releases" meta="Tags with notes and assets. Published releases are immutable." />
      <div className="flex flex-col gap-5">
        <PanelSkeleton title="Draft a release" rows={1} rowHeight={120} />
        <PanelSkeleton rows={4} rowHeight={64} />
      </div>
    </Skel>
  );
}
