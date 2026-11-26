import { Line, Skel } from "@/components/skeletons";
import { PageHeader } from "@/components/ui";
import { getPrompt, PROMPT_GROUPS } from "@/lib/prompts";

/** The page at its real structure: the static text is real, only the endpoint and Copy are placeholders. */
export default function Loading() {
  return (
    <Skel>
      <div className="min-w-0 overflow-hidden px-4 py-5 sm:px-6 lg:px-8 lg:py-7">
        <div className="mx-auto max-w-[920px]">
          <PageHeader
            title="Agent prompts"
            meta={
              <>
                <span>Copy a prompt into your agent to hand it the whole job.</span>
                <Line w={220} lh={18} bar={9} />
              </>
            }
          />
          <div className="flex flex-col gap-8">
            {PROMPT_GROUPS.map((group) => (
              <section key={group.title}>
                <h2 className="text-md font-semibold tracking-tight">{group.title}</h2>
                <p className="mt-0.5 mb-3 text-sm text-ink-2">{group.blurb}</p>
                <div className="flex flex-col gap-3">
                  {group.ids.map((id) => {
                    const prompt = getPrompt(id);
                    return prompt ? (
                      <div key={id} className="rounded-lg border border-line bg-surface">
                        <div className="flex items-start gap-3 px-3.5 py-3">
                          <div className="min-w-0 flex-1">
                            <h3 className="text-md font-medium [overflow-wrap:anywhere]">{prompt.title}</h3>
                            <p className="mt-0.5 text-sm text-ink-2">{prompt.when}</p>
                          </div>
                          <span className="h-7 w-[68px] shrink-0 rounded-md border border-line-strong" />
                        </div>
                        <div className="h-9 border-t border-line" />
                      </div>
                    ) : null;
                  })}
                </div>
              </section>
            ))}
          </div>
        </div>
      </div>
    </Skel>
  );
}
