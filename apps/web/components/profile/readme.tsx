/** The profile README. The server sends it as sanitized HTML, so it is rendered as is. */
import { Panel } from "@/components/ui";

export function ProfileReadme({ html, login }: { html: string; login: string }) {
  return (
    <Panel title={<span>{login}/README</span>} id="readme-heading">
      <div className="prose-bee max-w-3xl" dangerouslySetInnerHTML={{ __html: html }} />
    </Panel>
  );
}
