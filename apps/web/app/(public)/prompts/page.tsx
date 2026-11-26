import type { Metadata } from "next";
import { PromptCard } from "@/components/prompt-card";
import { Icon } from "@/components/icons";
import { buttonClass, PageHeader } from "@/components/ui";
import { promptTargets } from "@/lib/prompt-endpoint";
import { AGENTS_GUIDE_URL, getPrompt, PROMPT_GROUPS } from "@/lib/prompts";

export const metadata: Metadata = { title: "Agent prompts" };

/** Every prompt, filled in for this instance, grouped by what the agent is doing. */
export default async function PromptsPage() {
  const { endpoint } = await promptTargets();
  return (
    <div className="min-w-0">
      <div className="mx-auto max-w-[920px]">
        <PageHeader
          title="Agent prompts"
          lede={
            <>
              Copy a prompt into your agent to hand it the whole job. Each one is filled in for <span className="mono">{endpoint}</span>.
            </>
          }
          actions={
            <>
              {/* Plain links: llms.txt is a text file, not a page to prefetch. */}
              <a href="/llms.txt" className={buttonClass("secondary")}>
                <Icon name="file" size={16} />
                llms.txt
              </a>
              <a href={AGENTS_GUIDE_URL} className={buttonClass("secondary")}>
                Agents guide
                <Icon name="external" size={14} />
              </a>
            </>
          }
        />
        <div className="flex flex-col gap-12">
          {PROMPT_GROUPS.map((group) => (
            <section key={group.title} aria-labelledby={"group-" + group.title.toLowerCase().replace(/\s+/g, "-")}>
              <h2 id={"group-" + group.title.toLowerCase().replace(/\s+/g, "-")} className="text-xl font-semibold tracking-tight">
                {group.title}
              </h2>
              <p className="mt-1 mb-4 text-base text-ink-2">{group.blurb}</p>
              <div className="flex flex-col gap-6">
                {group.ids.filter((id) => getPrompt(id)).map((id) => (
                  <PromptCard key={id} id={id} vars={{ endpoint }} />
                ))}
              </div>
            </section>
          ))}
        </div>
      </div>
    </div>
  );
}
