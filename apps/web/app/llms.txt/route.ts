import { llmsTxt } from "@/lib/llms";
import { promptTargets } from "@/lib/prompt-endpoint";

/**
 * The llms.txt index, generated from lib/prompts.ts. The endpoint it names depends on how the
 * request reached the app, so the text is built per host, kept in memory, and handed to CDNs
 * with a long max-age.
 */
export async function GET(): Promise<Response> {
  return new Response(llmsTxt(await promptTargets()), {
    headers: {
      "Content-Type": "text/plain; charset=utf-8",
      "Cache-Control": "public, max-age=300, s-maxage=3600",
    },
  });
}
