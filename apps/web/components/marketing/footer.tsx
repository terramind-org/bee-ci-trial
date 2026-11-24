import Link from "next/link";
import { BrandMark } from "@/components/domain";
import { AGENTS_GUIDE_URL, DOCS_BASE, SELF_HOSTING_URL } from "@/lib/prompts";

const link = "inline-flex min-h-6 items-center text-base text-ink-2 hover:text-ink hover:underline";

function Group({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <nav aria-label={title}>
      <h2 className="mb-2 text-base font-medium">{title}</h2>
      <ul className="flex flex-col gap-1">{children}</ul>
    </nav>
  );
}

const ext = { target: "_blank", rel: "noreferrer noopener" } as const;

export function MarketingFooter() {
  return (
    <footer className="border-t border-line pt-10" data-testid="marketing-footer">
      <div className="grid gap-8 sm:grid-cols-[1.4fr_1fr_1fr_1fr]">
        <div>
          <div className="flex items-center gap-2.5">
            <BrandMark size={24} />
            <span className="text-md font-semibold tracking-tight">Beehive</span>
          </div>
          <p className="mt-3 max-w-[30ch] text-base text-ink-3">Source control built for agents. Open source, one server binary.</p>
        </div>
        <Group title="Product">
          <li>
            <Link className={link} href="/explore">
              Explore
            </Link>
          </li>
          <li>
            <Link className={link} href="/trending">
              Trending
            </Link>
          </li>
          <li>
            <Link className={link} href="/leaderboards">
              Leaderboards
            </Link>
          </li>
          <li>
            <Link className={link} href="/pricing">
              Pricing
            </Link>
          </li>
        </Group>
        <Group title="Developers">
          <li>
            <Link className={link} href="/prompts">
              Prompts
            </Link>
          </li>
          <li>
            <a className={link} href="/llms.txt">
              llms.txt
            </a>
          </li>
          <li>
            <a className={link} href={DOCS_BASE + "/schemas/openapi/bee.v1.json"} {...ext}>
              API
            </a>
          </li>
          <li>
            <a className={link} href={AGENTS_GUIDE_URL} {...ext}>
              Agents guide
            </a>
          </li>
        </Group>
        <Group title="Self-host">
          <li>
            <a className={link} href={SELF_HOSTING_URL} {...ext}>
              Self-hosting guide
            </a>
          </li>
          <li>
            <Link className={link} href="/prompts#self-host-docker">
              Docker prompt
            </Link>
          </li>
          <li>
            <Link className={link} href="/pricing#self-hosted">
              Enterprise edition
            </Link>
          </li>
        </Group>
      </div>
    </footer>
  );
}
