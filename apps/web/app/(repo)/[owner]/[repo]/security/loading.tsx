import { HeaderSkeleton, ListSkeleton, Skel, StatsSkeleton, TabsSkeleton } from "@/components/skeletons";

export default function Loading() {
  return (
    <Skel>
      <HeaderSkeleton title="Security" meta="Dependency advisories, secrets caught by push protection, and code scanning." />
      <StatsSkeleton labels={["Open", "Critical open", "With a fix change", "Secrets blocked at push"]} />
      <TabsSkeleton labels={["Open", "Fixed", "Dismissed"]} />
      <ListSkeleton rows={4} rowHeight={91} />
    </Skel>
  );
}
