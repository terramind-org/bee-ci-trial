import { INTRO } from "@/components/safety/model";
import { HeaderSkeleton, PanelSkeleton, Skel } from "@/components/skeletons";

export default function OrgSafetyLoading() {
  return <Skel><HeaderSkeleton title="Safety" meta={INTRO} eyebrow /><PanelSkeleton title="By signal" rows={3} rowHeight={48} /></Skel>;
}
