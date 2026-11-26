import { Suspense, type ReactNode } from "react";
import { Frame } from "../frame";
import { GlobalLoading } from "../layout-skeletons";

/** Public pages: readable signed out, so this layout never sends anyone to sign-in. */
export default function PublicLayout({ children }: { children: ReactNode }) {
  return (
    <Suspense fallback={<GlobalLoading />}>
      <Frame>{children}</Frame>
    </Suspense>
  );
}
