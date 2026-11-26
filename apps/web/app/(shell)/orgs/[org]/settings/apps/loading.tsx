import { HeaderSkeleton, PanelSkeleton, Skel } from "@/components/skeletons";

export default function Loading() {
  return (
    <Skel>
      <HeaderSkeleton title="Apps" eyebrow metaWidth={180} />
      <div className="flex flex-col gap-10">
        <PanelSkeleton title="Owned apps" rows={3} rowHeight={58} />
        <PanelSkeleton title="Installed apps" rows={3} rowHeight={58} />
      </div>
    </Skel>
  );
}
