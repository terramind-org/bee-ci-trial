import { HeaderSkeleton, PanelSkeleton, Skel } from "@/components/skeletons";

export default function Loading() {
  return (
    <Skel>
      <HeaderSkeleton title="Pages" metaWidth={360} />
      <PanelSkeleton title="Deployments" rows={4} rowHeight={52} />
    </Skel>
  );
}
