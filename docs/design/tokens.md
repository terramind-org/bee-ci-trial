# Beehive design tokens

Status: binding for `apps/web`. The implementation is `apps/web/app/globals.css`; change both in
the same commit. Spec for screens and behaviour: `docs/spec/web-ui.md`.

## Character

Beehive uses the Nucleus theme so the forge reads as part of the same product: zinc neutrals,
cobalt `#0034FF` for primary actions only, Geist throughout, status in small dots. Pages follow
the Document rhythm: type and whitespace carry the structure, hairlines stand in for boxes, and
mono is only for code, paths and SHAs.
Values mirror Nucleus (`ide/src/renderer/styles/globals.css`); when Nucleus changes a value, change
it here too. Shape and text carry meaning; colour reinforces it and never carries it alone. No
gradients, no glass, no shadows on resting surfaces, no emoji, no all-caps micro labels.

## Type

| Role | Family | Why |
|---|---|---|
| UI, headings | Geist (variable, 100–900) | The Nucleus UI family |
| Ids, paths, code, diffs | Geist Mono (variable, 100–900) | The Nucleus code family; matches Geist's metrics |

Both are self-hosted from `@fontsource-variable/geist` and `@fontsource-variable/geist-mono`
through `next/font/local`. No runtime font request leaves the origin. The root font size stays
16px so rem spacing keeps its 4px grid; body text is set at 15px.

Mono is for code, paths, refs and SHAs only. Names (people, agents, repositories), numbers,
labels, runtimes and key hints are Geist.

| Token | Size / line | Use |
|---|---|---|
| `text-2xs` | 12 / 16 | Key hints only |
| `text-xs` | 13 / 18 | Badges, captions |
| `text-sm` | 14 / 20 | Meta lines, secondary text, tables, code, small buttons |
| `text-base` | 15 / 22 | Body: default UI text, rows, buttons, inputs |
| `text-md` | 16 / 26 | Reading: change request and issue bodies, `prose-bee` |
| `text-lg` | 17 / 24 | Ledes, empty-state titles |
| `text-xl` | 20 / 28 | Section headings (`Panel`), semibold |
| `text-2xl` | 28 / 34 | Page titles (`PageHeader`), semibold, tight tracking |
| `text-3xl` | 44 / 48 | A repository's name on its home and Code (48px from `md`); the marketing hero |

Numbers that line up use `.num` (tabular figures). Code and diff tables use 24px rows so every
line-number target meets WCAG 2.5.8.

## Colour

HSL, as Nucleus writes it. Text tokens pass 4.5:1 on `bg`, in both themes (verified by axe in CI).

| Token | Dark (default) | Light | Use |
|---|---|---|---|
| `bg`, `surface` | 240 10% 3.9% | 0 0% 100% | Page and panels (Nucleus `background`, `card`) |
| `sunken`, `hover` | 240 5.9% 10% | 240 4.8% 95.9% | Wells, hover rows (Nucleus `muted`) |
| `selected` | 240 3.7% 15.9% | 240 5.9% 92.5% | Current nav item, active tab, neutral badge (Nucleus `secondary`) |
| `line` / `line-strong` | 15.9% / 26% | 90% / 83.9% | Dividers / control borders |
| `input` | 60 2% 18% | 240 4.8% 95.9% | Inputs and search (Nucleus `input-background`, `#30302E` in dark) |
| `popover` | 0 0% 9% | 0 0% 100% | Palette, dialogs, toasts |
| `ink` / `ink-2` / `ink-3` | 95.9% / 72% / 58% | 3.9% / 33.9% / 46.1% | Primary / secondary / tertiary text |
| `accent` | 228 100% 50% | same | Cobalt: primary buttons, the brand mark, focus |
| `accent-ink` | 217 91% 68% | 228 100% 50% | Accent text and links (lifted in dark for contrast) |
| `accent-soft` | cobalt at 16% | cobalt at 8% | Accent tint, selection |
| `live` | 38 92% 50% | same | Amber: something is running right now |
| `ok` / `warn` / `bad` (+ `-soft`) | emerald / amber / red, lifted | emerald / amber / red, deepened | Passed / attention / failed, blocked, sensitive |
| `add` / `del` (+ gutters, words) | translucent emerald / red | same, lighter | Diff lines |
| `--sh-*` | — | — | Syntax colours for sugar-high |

Rules: cobalt only for the primary action, the brand mark and focus; never for status. Running
work is amber (`live`), done is emerald, failed or blocked is red, idle is `ink-3`. Links are
underlined. Risk is a four-bar meter plus the class name.

### Risk mapping (SPEC §11.3)

| Classes | Risk | Meter | Badge tone |
|---|---|---|---|
| generated, docs, comment_only, test_only, rename_only | low | 1 bar | neutral |
| dependency | medium | 2 bars | warn |
| logic | elevated | 3 bars | outline, ink bars |
| sensitive | high | 4 bars | bad |

### Change glyphs

A change row starts with a 14px ring that encodes its landing state: dashed ring for closed,
filled `ink-3` for landed, a spinning amber arc for in the merge queue, an emerald ring with a dot
for ready to land, a red ring with a cross for failing checks, and a plain ring for blocked by
policy. Each has an accessible label.

### Principal shapes

Humans are circles (accent tint), agents rounded squares (`selected`), services diamonds
(outline). An agent's runtime is always printed next to its name.

## Space, radius, elevation

- Spacing: Tailwind's 4px scale. Rows 12–14px vertical padding. Page gutter 20 / 32 / 48px at
  mobile / tablet / desktop; content max width 1120px; the rail is 248px.
- Sections are separated by space (40px between a page header and its first section, 40–56px
  between sections), not borders. A hairline (`border-line`, or `.rule`) separates rows in a list
  and sits under tab rows; nothing else gets a border at rest. No boxed panels: `Panel` is a
  heading and its content.
- Radius: `xs` 4px (keys), `sm` 5px, `md` 6px (buttons, inputs), `lg` 8px (rail items), `xl`
  12px (popovers, the repo info band).
- Elevation: none at rest. `shadow-overlay` only on palette, dialogs, menus, popovers and toasts,
  which sit on `popover`.
- Controls: 32px (sm) and 36px (md) high; icon buttons 36–40px square. Buttons press to 97% scale.
- Avatars: the generated marks in `components/avatar.tsx` (`PrincipalMark` renders them). Humans
  are circles, agents rounded squares with a runtime badge, services square tiles with a diamond.
  28px or more in lists, 32px in the rail, 20px inline in a sentence.
- Icons: Phosphor through `components/icons.tsx` (`<Icon name="…" />`), regular weight, `fill`
  for the active rail item and a set star. 16px in text, 18px in buttons and menus, 20px in the rail.

## The app shell

One navigation system (`components/app-shell.tsx`):

- Signed in: a left rail on `sunken` at 50%: the wordmark and the palette button, search, then
  Inbox (with the count of what needs you, in a cobalt pill), Feed, Agents, Repositories and
  Explore; on repo pages the current repository's switcher; New, theme and you with your org at
  the bottom. Below `md` it is a drawer behind a 56px top bar, and the drawer also lists the
  repository's sections.
- Signed out: a 64px top bar: the wordmark, Explore (and Pricing on cloud), search, theme, Sign in.
- Repo pages add the repo header (`components/repo-header.tsx`): owner crumb, the name at 44–48px
  on the repo's home and Code (28px deeper in), the description as a lede, an info band (branch,
  files, latest merge, Clone), then plain tabs over a hairline: Code, Change requests, Issues,
  Runs, Updates, More.
- Each page has at most one "…" menu (`PageMenu` in `components/menu.tsx`), and *Copy as* lives
  only there and on `y`.

## Motion

Durations follow Nucleus (`--dur-hover` 120ms, `--dur-control` 160ms, `--dur-panel` 240ms), easing
`cubic-bezier(0.22, 1, 0.36, 1)`.

| Token | Duration | Use |
|---|---|---|
| hover/state | 120ms | Colour changes, button press |
| tabs, segmented controls | 160ms | Active segment |
| `animate-enter` | 180ms, 4px rise | Dialogs, palette, toasts |
| `animate-arrive` | 260ms, accent tint fading out | A live event joining the stream |
| `animate-pulse-dot` | 1.6s loop | Live and running indicators only |

Nothing else moves. `prefers-reduced-motion: reduce` collapses every animation to a single frame.

### Live data

Server-rendered lists refresh in place when events arrive (`LiveRefresh`, bursts coalesced to
one refresh: 250ms quiet, 1s ceiling). Nothing may jump:

| Change | Treatment |
|---|---|
| A row moves | FLIP (`useFlip`): drawn at its old offset, slides to the new one, 300ms ease-out cubic, transform only |
| A row arrives | Fades and drops 6px into place with a brief accent tint; staggered 30ms in a burst |
| A row leaves | Fades for 200ms in place (`usePresence`), then the rows below slide up |
| A value changes (state, count, stat) | `Swap` re-keys it: 220ms fade and 2px rise; never on first paint |
| Activity feed | History loads in one request (`GetEventLogInfo` + `ListEvents`), then only new events stream; a burst renders once per frame |

Only rows on or near the screen animate; a 300-row feed never runs 300 tweens. A full replacement
(a new filter) is not an arrival and does not animate.

### Loading

Every route has a `loading.tsx` built from `components/skeletons.tsx` that occupies exactly the
boxes the page will: the real title, description and tab labels, the same filter bar, rows of the
measured height (changes, runs, work 59px; agents 77px; activity events 62px), panel headers of 40px
or 45px when the real one carries a button. The skeleton waits 120ms and fades in, so a fast
navigation never flashes one. Swapping skeleton for data moves nothing above the fold.

## Patterns

Reusable compositions for the discovery surfaces. They use only the tokens above; the Character
rules apply: no gradients, glass, resting shadows, emoji or all-caps micro labels.

**Repo card.** No border: a row with 16px padding that takes `hover` on pointer. Top line: the
full name `owner/repo` in Geist `text-lg` semibold, as a link, then the visibility badge (Public
outline, Private neutral, with its word). Below, the description in `ink-2`, two lines at most,
truncated with an ellipsis. Bottom line in `text-xs`, `ink-3`: a language dot (8px circle plus the
language name), stars with a star icon and `.num` count, and updated-ago (`Time`, absolute in
`title`). The dot is never the only carrier of the language: the name is always printed.

**Stat strip.** Inline numbers on one row, wrapping on mobile: each is a `.num` value in `ink`
`text-lg` followed by its label in `ink-3` `text-xs`, separated by 24px. No boxes, no dividers.
A changing value uses `Swap`.

**Rank row.** A 48px row: the rank number in Geist `.num` at a fixed 32px width, `ink-3`, right
aligned; then the principal mark (circle, rounded square or diamond), the name as a link and, for
agents, the runtime in `ink-3`; the score on the far right, `.num`, `ink`. Rows are `j`/`k` targets
with a visible focus ring. The top three differ only by the rank number weight, not by colour or
medals.

**Contribution heatmap.** A grid of 11px cells, 2px gap, one column per week. Five steps: step 0 is
`sunken`; steps 1 to 4 are `ok` (emerald) at 25%, 45%, 70% and 100% alpha over `sunken`. Steps come
from the count quartiles of the displayed year, so a quiet profile still shows shape. A legend
("Less" to "More") with the five swatches sits below. Colour is never the only signal: the grid has
an accessible text summary ("412 contributions in the last year, busiest week of 3 March"), each
cell is described by a `title` ("3 contributions on 3 March"), and the grid is exposed as a table
with a visually hidden caption.

**Hero type step.** `text-3xl`: 44 / 48, semibold, tight tracking (-0.02em). Used for the headline
of the marketing landing and a repository's name; every other page title stays `text-2xl`. Below
640px the hero steps down to `text-2xl`.

**Landing section rhythm.** Sections on the marketing landing are separated by 96px of vertical
space on desktop and 56px on mobile (below 640px). Content is at most 1120px wide and centred;
the 1240px app max width is not used there. Section headings are `text-xl`, one idea per section,
and nothing animates except the shared hover and enter motion.

### Brief patterns

The change page's Brief tab and the review queue (`docs/spec/web-ui.md` §3). Evidence is the
content, so the frame around it stays quiet: no borders on cards, no shadows, media at full column
width.

**Evidence card.** A figure in the reading column: the media first (16:10 well in `sunken` while it
loads, 8px radius, never cropped: images `object-fit: contain`), then one meta line in `text-sm`:
the title in `ink`, the source chip, the revision (`r3`, `.num`, `ink-3`) and the stale badge when
it applies; the caption follows in `ink-2`, 15px, at most 1000 characters. The hero card spans the
column; others sit two per row from 1024px, one per row below. Cards are `j`/`k` targets with a
visible focus ring.

**Source chip.** Words with a small icon, never colour alone: "From the agent" (or "From <name>"
for a person) with the principal mark in `ink-2`, and "Verified by CI" with a check-circle in `ok`.
`text-xs`, `selected` background, 4px radius, 2px by 6px padding. It links to the check run when
there is one.

**Stale badge.** "Stale: from r2" in `warn` on `warn-soft`, `text-xs`, with a clock icon; the card's
media drops to 60% opacity and regains it on hover or focus. Its `title` says "Captured before the
latest revision (r3); the author may need to capture it again".

**Compare slider.** Before and after stacked in one well; the after image clips at the divider. The
divider is 2px `ink` with a 32px round handle (`surface`, `line-strong` ring) carrying a
left-right arrows icon; the handle is the `role="slider"` and gets the focus ring. "Before" and
"After" labels sit in the top corners in `text-xs` on `popover` at 80%, and swap sides with the
divider so neither is ever hidden. Dragging is direct, without easing; arrow keys move by 5%.

**Benchmark bars.** One row per series: the name and unit on the left in `text-sm`, then two
horizontal bars on a shared scale starting at zero: baseline in `ink-3` at 40% and change in `ok`
when better or `bad` when worse (by `lower_is_better`), each with its `.num` value at the end.
Under the rows, the difference in words: "35 ms faster (29% better) than the baseline". Inline
SVG, no chart library; the SVG has a `<title>` and the values are also in a visually hidden table.

## Themes

Dark and light are complete token sets on `[data-theme]`. Dark is the default, as in Nucleus; the
theme script runs before paint and only picks light when `localStorage["bee-theme"]` says so.
The toggle sits at the bottom of the rail (in the top bar when signed out).
