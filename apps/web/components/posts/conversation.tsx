import Link from "next/link";
import type { BeeClient } from "@/lib/api/client";
import type { Comment, Post, PostAdoption } from "@/lib/api/types";
import { repoHref } from "@/lib/routes";
import { Markdown } from "../markdown";
import { Time } from "../time";
import { optionalRead } from "./post-data";
import { postHref } from "./post-text";

type Adoption = { adoption: PostAdoption; repo: string; href?: string; number?: string };

export async function loadAdoptions(api: BeeClient, postId: string): Promise<Adoption[] | undefined> {
  const result = await optionalRead(api.post.listPostAdoptions({ postId, page: { pageSize: 50 } }));
  if (!result) return;
  const adoptions = result.adoptions.flatMap((adoption) => {
    const repo = adoption.repo?.fullName ?? adoption.subject?.repo?.fullName;
    return repo ? [{ adoption, repo }] : [];
  });
  const repos = new Set(adoptions.filter(({ adoption }) => adoption.subject?.kind === "SUBJECT_KIND_ISSUE" && !/^\d+$/.test(adoption.subject.id)).map(({ repo }) => repo));
  const numbers = new Map(await Promise.all([...repos].map(async (repo) => {
    const rows = await optionalRead(api.issue.listIssues({ repo: { fullName: repo } }));
    return [repo, new Map(rows?.issues.map((issue) => [issue.id, String(issue.number)]))] as const;
  })));
  return adoptions.map(({ adoption, repo }) => {
    const subject = adoption.subject;
    if (subject?.kind === "SUBJECT_KIND_ISSUE") {
      const number = /^\d+$/.test(subject.id) ? subject.id : numbers.get(repo)?.get(subject.id);
      return { adoption, repo, number, href: number ? repoHref(repo, "work/" + number) : undefined };
    }
    return { adoption, repo, href: subject?.id ? repoHref(repo, "changes/" + encodeURIComponent(subject.id)) : undefined };
  });
}

export function AdoptionLine({ item }: { item: Adoption }) {
  const { adoption, repo, number, href } = item;
  const title = adoption.title || (adoption.subject?.kind === "SUBJECT_KIND_ISSUE" ? "Issue" : "Pull request");
  return <>
    <Link className="link" href={repoHref(repo)}>{repo}</Link>
    {" · "}{href ? <Link className="link" href={href}>{number ? "#" + number + " " : ""}{title}</Link> : title}
    {adoption.author?.display ? " · " + adoption.author.display : null}
    {adoption.createdAt ? <> · <Time iso={adoption.createdAt} /></> : null}
  </>;
}

export function Adoptions({ post, items }: { post: Post; items?: Adoption[] }) {
  if (!items?.length && !Number(post.counts?.adoptions)) return null;
  return (
    <section id="adoptions" aria-labelledby="adoptions-title" className="mt-10 scroll-mt-20" data-testid="post-adoptions-section">
      <h2 id="adoptions-title" className="mb-2 text-xl font-medium">Adoptions</h2>
      {items?.length ? <ul className="divide-y divide-line">
        {items.map((item, i) => <li key={i} className="py-3 text-ink-2"><AdoptionLine item={item} /></li>)}
      </ul> : !items ? <p className="text-ink-2"><Link className="link" href={"/signin?next=" + encodeURIComponent(postHref(post) + "#adoptions")}>Sign in</Link> to see the adoptions you can read.</p> : null}
    </section>
  );
}

export function PostComments({ comments, repo }: { comments: Comment[]; repo: string }) {
  return (
    <section id="comments" aria-labelledby="comments-title" className="mt-10 scroll-mt-20">
      <h2 id="comments-title" className="mb-2 text-xl font-medium">Comments on the pull request</h2>
      {comments.length ? <ul className="divide-y divide-line">
        {comments.map((comment) => <li key={comment.id} className="py-3">
          <p className="mb-2 flex flex-wrap gap-x-2 text-sm text-ink-2">
            {comment.author?.display}
            {comment.createdAt ? <Time iso={comment.createdAt} /> : null}
          </p>
          <Markdown source={comment.body} repo={repo} headingOffset={2} />
        </li>)}
      </ul> : <p className="text-ink-2">There are no comments yet.</p>}
    </section>
  );
}
