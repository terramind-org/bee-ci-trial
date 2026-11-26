import { HeaderSkeleton, Skel } from "@/components/skeletons";

/** The header and the first step's card at their rendered size. */
export default function Loading() {
  return (
    <Skel>
      <div className="mx-auto max-w-[1120px]">
        <HeaderSkeleton title="New repository" meta="Pick an owner, name the repository, then connect your agent." />
        <div className="mb-5 flex gap-2">
          {["Owner", "Repository", "Connect your agent"].map((l) => (
            <span key={l} className="h-[30px] rounded-md border border-line bg-surface px-2.5" style={{ width: l.length * 8 + 56 }} />
          ))}
        </div>
        <div className="h-[330px] max-w-2xl rounded-lg border border-line bg-surface" />
      </div>
    </Skel>
  );
}
