import { HeaderSkeleton, PanelSkeleton, Skel } from "@/components/skeletons";
import { INTRO } from "@/components/safety/model";

export default function SafetyLoading() {
  return <Skel><HeaderSkeleton title="Safety" meta={INTRO} /><PanelSkeleton title="Needs a look" rows={3} rowHeight={120} /></Skel>;
}
