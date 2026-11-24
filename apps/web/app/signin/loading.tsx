import { BrandMark } from "@/components/domain";
import { Line, Skel } from "@/components/skeletons";

/** The sign-in page waits on the server for its edition: hold its two columns while it answers. */
export default function Loading() {
  return (
    <main id="main" className="mx-auto grid min-h-dvh max-w-5xl items-center gap-10 px-5 py-10 md:grid-cols-[1fr_420px]">
      <Skel>
        <div className="mb-6">
          <BrandMark size={40} />
        </div>
        <Line w={320} lh={32} bar={20} />
        <div className="mt-4 max-w-md">
          <Line w="100%" lh={24} />
          <Line w="92%" lh={24} />
          <Line w="60%" lh={24} />
        </div>
        <div className="mt-6 h-16 max-w-md rounded-md border border-line bg-sunken" />
      </Skel>
      <Skel>
        <div className="h-56 rounded-lg border border-line bg-surface" />
      </Skel>
    </main>
  );
}
