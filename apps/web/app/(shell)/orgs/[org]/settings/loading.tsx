import { HeaderSkeleton, PanelSkeleton, Skel } from "@/components/skeletons";

export default function Loading() {
  return (
    <Skel>
      <HeaderSkeleton title="Organization settings" eyebrow metaWidth={180} />
      <PanelSkeleton title="Organization" rows={3} rowHeight={48} />
      <PanelSkeleton title="Repositories" rows={4} rowHeight={58} />
    </Skel>
  );
}
