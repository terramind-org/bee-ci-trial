import { HeaderSkeleton, RowSkeleton, Skel } from "@/components/skeletons";

export default function Loading() {
  return (
    <Skel>
      <HeaderSkeleton title="Agents" />
      <h2 className="mb-3 text-sm font-medium text-ink-2">Working</h2>
      <ul className="divide-y divide-line border-t border-line">
        {[0, 1, 2].map((i) => <RowSkeleton key={i} i={i} height={124} glyph="square" />)}
      </ul>
    </Skel>
  );
}
