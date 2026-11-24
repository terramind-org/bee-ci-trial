import { Line, Skel } from "@/components/skeletons";
import { PageHeader } from "@/components/ui";

/** Header, filter bar and a two-column grid of cards at the heights the loaded page has. */
export default function Loading() {
  return (
    <Skel>
      <PageHeader title="Repositories" meta={<Line w={96} lh={18} bar={9} />} />
      <div className="mb-4 flex flex-wrap gap-2">
        <Line w={320} lh={32} bar={32} />
        <Line w={128} lh={32} bar={32} />
        <Line w={128} lh={32} bar={32} />
        <Line w={128} lh={32} bar={32} />
      </div>
      <div className="grid gap-3 md:grid-cols-2">
        {Array.from({ length: 8 }, (_, i) => (
          <div key={i} className="flex flex-col gap-2 rounded-lg border border-line bg-surface p-3.5">
            <Line w={`${48 + (i % 4) * 8}%`} lh={24} bar={12} />
            <Line w="86%" lh={20} bar={9} />
            <Line w="40%" lh={20} bar={9} />
          </div>
        ))}
      </div>
    </Skel>
  );
}
