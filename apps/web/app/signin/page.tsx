import type { Metadata } from "next";
import Link from "next/link";
import { BrandMark } from "@/components/domain";
import { SignIn } from "@/components/sign-in";
import { FIXTURE_REPO } from "@/lib/api/config";
import { adapterKind, apiUrl, edition } from "@/lib/api/server";
import { requestOrigin } from "@/lib/prompt-endpoint";
import { repoHref } from "@/lib/routes";

export const metadata: Metadata = { title: "Sign in" };
// The adapter (fixture or http) is a runtime choice, so this page cannot be prerendered.
export const dynamic = "force-dynamic";

export default async function SignInPage() {
  const fixture = adapterKind() === "fixture";
  const cloud = (await edition()) === "cloud";
  // A self-hosted instance is named by the host people reach it on.
  const instance = new URL(await requestOrigin()).host;
  return (
    <main id="main" className="mx-auto grid min-h-dvh max-w-5xl items-center gap-10 px-5 py-10 md:grid-cols-[1fr_420px]">
      <div>
        <div className="mb-6">
          <BrandMark size={40} />
        </div>
        <h1 className="text-2xl font-semibold tracking-tight">{cloud ? "Create your account, or sign in" : "Sign in to " + instance}</h1>
        <p className="mt-4 max-w-md text-md text-ink-2">
          {cloud ? "Beehive creates your account the first time you sign in, then takes you to set up your first repository. " : ""}
          Thousands of agents land on one branch without colliding. Every change says who wrote it, on whose behalf, with which model, and what policy allowed it.
        </p>
        {fixture ? (
          <p className="rule mt-8 max-w-md pt-4 text-base text-ink-2">
            This server is running on demo data. Any token of eight or more characters signs you in as Mara Okafor, or{" "}
            <Link href={repoHref(FIXTURE_REPO, "activity")} className="link">
              skip straight in
            </Link>
            .
          </p>
        ) : (
          <p className="rule mt-8 max-w-md pt-4 text-base text-ink-2">
            Running <code className="mono">bee-server --dev</code>? Paste the root token it prints on start (also in <code className="mono">&lt;data dir&gt;/credentials</code>). Server:{" "}
            <code className="mono">{apiUrl()}</code>
          </p>
        )}
      </div>
      <SignIn edition={cloud ? "cloud" : "enterprise"} />
    </main>
  );
}
