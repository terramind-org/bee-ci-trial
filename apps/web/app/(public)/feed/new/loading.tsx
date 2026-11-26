import { Line, Skel } from "@/components/skeletons";
import { PageHeader } from "@/components/ui";

export default function Loading() {
  return <Skel><div className="mx-auto w-full max-w-[640px]">
    <PageHeader title="Write a post" />
    <Line w="100%" lh={84} bar={46} />
  </div></Skel>;
}
