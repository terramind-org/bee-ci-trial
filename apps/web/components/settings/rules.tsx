import { PolicyEditor, type PolicyUnavailable } from "@/components/policy-editor";
import { Row, Rows } from "@/components/parts";
import type { Policy } from "@/lib/api/types";
import { policyRules } from "./policy-rules";

export { policyRules } from "./policy-rules";

export const RULES_CHANGE_NOTE = "Rules change through a pull request, so the current rules decide who may change them.";

export function Rules({ repo, policy, defaultRef, unavailable }: {
  repo: string;
  policy: Policy;
  defaultRef: string;
  unavailable?: PolicyUnavailable;
}) {
  const rules = policyRules(policy.normalizedJson);
  return (
    <PolicyEditor repo={repo} source={policy.source} path={policy.path} epoch={policy.epoch} defaultRef={defaultRef} unavailable={unavailable}>
      {rules.length ? (
        <Rows label="Effective rules" testId="effective-rules">
          {rules.map((rule, index) => (
            <Row key={index}>
              {rule.raw ? <code className="mono whitespace-pre-wrap text-sm [overflow-wrap:anywhere]">{rule.text}</code> : rule.text}
            </Row>
          ))}
        </Rows>
      ) : unavailable ? (
        <div className="space-y-2 text-base text-ink-2">
          <p>The effective rules are unavailable on this server.</p>
          {policy.source ? <pre className="mono max-h-64 overflow-auto rounded-xl bg-sunken p-3 text-sm [overflow-wrap:anywhere] whitespace-pre-wrap">{policy.source}</pre> : null}
        </div>
      ) : (
        <p className="text-base text-ink-2">The server returned no effective rules to show.</p>
      )}
      <p className="text-base text-ink-2">{RULES_CHANGE_NOTE}</p>
    </PolicyEditor>
  );
}
