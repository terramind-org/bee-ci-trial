import type { Metadata } from "next";
import Link from "next/link";
import { PageFacts } from "@/components/bee/page-facts";
import { BrandMark } from "@/components/domain";
import { SignIn } from "@/components/sign-in";
import { FIXTURE_REPO } from "@/lib/api/config";
import { adapterKind, apiUrl, edition } from "@/lib/api/server";
import { requestOrigin } from "@/lib/prompt-endpoint";
import { repoHref } from "@/lib/routes";
import { toolFor } from "@/lib/tools";
import { requestMode } from "../request-mode";

export const metadata: Metadata = { title: "Sign in" };
export const dynamic = "force-dynamic";

export default async function SignInPage() {
  const fixture = adapterKind() === "fixture";
  const [ed, origin, { mode }] = await Promise.all([edition(), requestOrigin(), requestMode()]);
  const instance = new URL(origin).host;
  const title = ed === "cloud" ? "Create your account, or sign in" : "Sign in to " + instance;
  return (
    <main id="main" className="mx-auto min-h-dvh w-full max-w-[540px] px-5 pt-12 pb-16 sm:px-8 sm:pt-[10vh]">
      <Link href="/" className="mb-8 inline-flex min-h-6 items-center gap-2 text-base font-medium text-ink">
        <BrandMark size={16} />
        Beehive
      </Link>
      {mode === "bee" ? (
        <PageFacts page="sign-in" title={title}
          tools={[toolFor("AuthService/Login", "Sign in with GitHub"), toolFor("AuthService/WhoAmI", "Check a token and read its identity")]}
          groups={[{ title: "instance", facts: [{ label: "host", value: instance }, { label: "edition", value: ed }] }]} />
      ) : (
        <h1 className="display mb-8 break-words text-ink">{title}</h1>
      )}
      <div className={mode === "bee" ? "mt-8" : undefined}><SignIn edition={ed} /></div>
      {fixture ? (
        <p className="rule mt-8 pt-4 text-sm text-ink-2">
          This is a demo. Enter a demo token of at least eight characters to sign in as Mara Okafor, or{" "}
          <Link href={repoHref(FIXTURE_REPO, "activity")} className="link inline-flex min-h-6 items-center">open the demo repository</Link>.
        </p>
      ) : (
        <p className="rule mt-8 pt-4 text-sm text-ink-2">
          Running <code>bee-server --dev</code>? Use the root token it prints on start. Server:{" "}
          <code className="break-all">{apiUrl()}</code>
        </p>
      )}
    </main>
  );
}
