/**
 * Needs you: safety items first, then a plain sentence per pull request, highest risk first. Each pull request row names its
 * repository, number and branch, says why it is in front of a person, names the person who
 * answers for it (the only place the yellow mark sits), and offers one action.
 */
import Link from "next/link";
import { ButtonLink } from "@/components/kit/button";
import { BranchLine, Mark, Row, Rows, Sep } from "@/components/parts";
import { relativeTime } from "@/lib/format";
import { ACTION, needWords, type Need, type SafetyNeed } from "@/lib/inbox/needs";
import { riskOf } from "@/lib/inbox/words";
import { repoHref } from "@/lib/routes";

/** Where a need's one action goes: the pull request, or the post panel on it. */
export function needHref(need: Need): string {
  return repoHref(need.repo, "changes/" + need.change.id) + (need.kind === "post" ? "#post" : "");
}

/** `acme/api #41`. */
export function needRef(need: Need): string {
  return need.repo + " #" + need.change.number;
}

export function NeedsList({ needs, safety, now }: { needs: Need[]; safety: SafetyNeed[]; now: number }) {
  return (
    <Rows testId="needs-you" label="Work that needs you">
      {safety.map((n, i) => (
        <Row
          key={n.id}
          data-kind={n.kind}
          meta={
            <>
              {n.answers ? <Mark>{n.answers.display} answers for it</Mark> : null}
              {n.at ? (
                <>
                  {n.answers ? <Sep /> : null}
                  <time dateTime={n.at}>{relativeTime(n.at, now)}</time>
                </>
              ) : null}
            </>
          }
          action={
            <ButtonLink href={n.href} size="sm" variant={i === 0 ? "primary" : "secondary"}>
              Review safety
              <span className="sr-only">: {n.title}</span>
            </ButtonLink>
          }
        >
          <span className="text-ink-2">{n.repo}</span> <span aria-hidden="true">·</span> <span className="font-normal">{n.title}.</span> {n.reason}
        </Row>
      ))}
      {needs.map((n, i) => {
        const words = needWords(n);
        const risk = n.kind === "post" ? null : riskOf(n.change.revisions.at(-1)?.diffClass ?? "");
        return (
          <Row
            key={n.kind + n.repo + n.change.id}
            data-kind={n.kind}
            meta={
              <>
                {n.answers ? (
                  <>
                    <Mark>{n.answers.display} answers for it</Mark>
                    <Sep />
                  </>
                ) : null}
                <BranchLine branch={n.branch} target={n.change.targetRef} />
                {risk ? (
                  <>
                    <Sep />
                    <span>{risk.level} risk</span>
                  </>
                ) : null}
                {n.at ? (
                  <>
                    <Sep />
                    <time dateTime={n.at}>{relativeTime(n.at, now)}</time>
                  </>
                ) : null}
              </>
            }
            // The first row is the next thing to do and carries the screen's one solid button.
            action={
              <ButtonLink href={needHref(n)} size="sm" variant={i === 0 && safety.length === 0 ? "primary" : "secondary"}>
                {ACTION[n.kind]}
                <span className="sr-only">: {n.change.title}</span>
              </ButtonLink>
            }
          >
            <span className="text-ink-2">{needRef(n)}</span> <span aria-hidden="true">·</span> <span className="font-normal">{words.who}</span> {words.verb}{" "}
            <Link href={needHref(n)} className="link font-normal">
              {n.change.title}
            </Link>
            . {words.why}
          </Row>
        );
      })}
    </Rows>
  );
}
