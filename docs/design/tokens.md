# Beehive design tokens

Status: binding for `apps/web`. The implementation is `apps/web/app/globals.css`; change both in
the same commit. Spec for screens and behaviour: `docs/spec/web-ui.md`.

## Character

Paper and ink. Beehive is read by people who decide, not by people who type, so a page is a few
plain sentences on a quiet ground: warm paper in the light theme, near-black in the dark one, ink
for everything else, hairlines in place of boxes, and pill-shaped controls.

There is **one colour: the yellow mark.** It means only "a person answers for this", and it is a
dot or a ring, never text (on paper it could not be read). Status is a word and a shape; the
`ok`, `warn` and `bad` tints only reinforce it, faintly. Diffs keep a soft green and red, the one
place they carry the meaning people already expect. Primary buttons are solid ink, never a hue.

Large type is light. Mono is only for code, paths, refs and ids. No gradients, no glass, no
shadows on resting surfaces, no emoji. The approved prototype is the visual reference; this
document and `apps/web/app/globals.css` are what the product is built from.

## Type

| Role | Stack | Why |
|---|---|---|
| UI, headings | `-apple-system, BlinkMacSystemFont, "SF Pro Text", system-ui, "Helvetica Neue", Arial, sans-serif` | The device's own UI face: nothing to download, text paints with the first frame |
| Ids, paths, refs, code, diffs | `ui-monospace, "SF Mono", Menlo, monospace` | The device's own mono |

No font file is bundled and no font request is made. The stacks are `--sans` and `--mono` in
`globals.css` and are exported from `apps/web/app/fonts.ts` for the places CSS cannot reach. The
root font size stays 16px so rem spacing keeps its 4px grid.

Mono is for code, paths, refs and ids only. Names (people, agents, repositories), numbers, labels,
runtimes and key hints are in the UI face.

| Token | Size / line | Use |
|---|---|---|
| `text-2xs` | 12 / 16 | Key hints, badges, the label over a number |
| `text-xs` | 13 / 18 | Small print: section labels, field labels, small buttons, the rail's foot |
| `text-sm` | 14 / 20 | Meta lines, secondary text, tables, code, recent repositories |
| `text-base` | 15 / 22 | Controls, tabs, quiet lines, the repository header |
| `text-md` | 16 / 24 | Body: the default, and the rail's items |
| `text-lg` | 18 / 26 | Rows and ledes, light |
| `text-xl` | 22 / 28 | A lane or card title, light |
| `text-2xl` | 34 / 38 | Page titles (`PageHeader`) and large numbers (`Stat`), weight 300, tracking -0.035em |
| `text-3xl` | 48 / 52 | The display step on the marketing landing, weight 300 |
| `.display` | clamp(32px, 4vw, 52px) / 1.06 | A page's one large sentence, weight 200, tracking -0.04em |
| `.eyebrow` | 11 / 16 | The spaced capital label over a title and over a group in the rail, weight 500, tracking 0.14em |

Weights: 200 and 300 for large type, 400 for text, 500 for labels and controls. Nothing is heavier
than 500 in the interface: `font-semibold` is mapped to 500 and `font-bold` to 600 so markup
written before this scale keeps working. `text-2xl` and `text-3xl` are always light, whatever
weight utility sits beside them.

Numbers that line up use `.num` (tabular figures). Code and diff tables use 24px rows so every
line-number target meets WCAG 2.5.8.

## Colour

Every colour is one `light-dark()` pair on `:root`, so the page follows the system setting with no
script. Text tokens pass 4.5:1 on every ground text is set on, in both themes
(`apps/web/lib/tokens.test.ts` computes it from the stylesheet; axe checks the rendered pages).

| Token | Light | Dark | Use |
|---|---|---|---|
| `bg`, `surface` | `#f4f2ec` | `#0f0f0e` | The page: warm paper, near-black |
| `sunken`, `hover` | `#eceae3` | `#171716`, `#191918` | Wells, hovered rows |
| `selected` | `#e6e4dd` | `#20201f` | The current rail item, a neutral badge |
| `line` / `line-strong` | ink at 14% / 30% | ink at 16% / 32% | Hairlines / a control's outline on hover, link underlines |
| `input` | white at 70% | `#1c1b19` at 70% | The field surface, inside a hairline |
| `popover` | `#fbfaf6` | `#191918` | Palette, dialogs, menus, toasts |
| `ink` | `#16171a` | `#f4f3ed` | Text, solid buttons, the focus ring |
| `ink-2` | `#5b5a57` | `#a3a19b` | Secondary text |
| `ink-3` | `#666562` | `#93918c` | Tertiary text: captions, placeholders, counts |
| `faint` | `#9a9892` | `#75736e` | Marks, empty cells and decoration only. Never text: it does not reach 4.5:1 |
| `mark` | `#f0cb45` | `#f4d463` | The yellow mark: a person answers for this |
| `mark-ring` | ink at 55% | transparent | The 1px ring that keeps the mark visible on paper |
| `accent`, `accent-ink`, `focus` | `ink` | `ink` | Kept as names; the accent is ink. `on-accent` is `bg` |
| `accent-soft` | ink at 7% | ink at 9% | A neutral tint: an arriving row |
| `ok` / `warn` / `bad` | `#3f5a49` / `#665a36` / `#7a403a` | `#a9c4b2` / `#cfc39c` / `#dfaaa3` | A faint tint on a status word. Near-neutral on purpose |
| `ok-soft` / `warn-soft` / `bad-soft` | the hue at 9–10% | the hue at 12–13% | The ground of a status badge |
| `live` | `ink-2` | `ink-2` | The dot beside something running now |
| `add` / `del` (+ gutters, words) | green at 12% / red at 10% | green at 14% / red at 14% | Diff lines |
| `--sh-*` | ink with a faint hue | the same, lifted | Syntax colours for sugar-high; comments are `ink-3` |

Rules:

- Yellow is never text and never a status. It marks the person who answers for something: the dot
  before their name (`.dot`), the brand dot, the app icon.
- A status is always a word, usually with a shape (a ring, a bar, an icon). Take the colour away
  and nothing is lost.
- The one solid button on a screen is ink. Everything else is a hairline outline or plain text.
- Links are underlined (`.link`): ink, with the underline in `line-strong` until hovered.
- The focus ring is 2px ink, 2px off the control.

### Risk mapping (SPEC §11.3)

| Classes | Risk | Meter | Badge tone |
|---|---|---|---|
| generated, docs, comment_only, test_only, rename_only | low | 1 bar | neutral |
| dependency | medium | 2 bars | warn |
| logic | elevated | 3 bars | outline, ink bars |
| sensitive | high | 4 bars | bad |

### State shapes

A state is a word with a 12px shape before it, drawn in the text's own ink (`State` in
`components/parts.tsx`). The word carries the meaning; the shape lets the eye find it in a list.

| Shape | Means | Used for |
|---|---|---|
| open ring | open, or waiting on the reader | Needs you, an open issue nobody has |
| ring, half filled | under way | Checks running, in the merge queue, an issue being worked on |
| ring with a dash | waiting on someone else | Needs review, blocked by a rule |
| tick | passed or met | Ready to merge, a rule that is met |
| cross | failed | Checks failing |
| filled dot | merged | Merged |
| dashed ring | closed | Closed |

No shape is coloured: status does not use `ok`, `warn` or `bad` on these screens. The older
14px `ChangeGlyph` ring (dashed for closed, filled for merged, a spinning arc for the merge
queue, a ring with a dot for ready to merge, a ring with a cross for failing checks, a plain ring
for blocked by the rules) remains on screens not yet redesigned; each has an accessible label.

### Principal shapes

Humans are circles, agents rounded squares (`selected`), services diamonds (outline). An agent's
runtime is always printed next to its name. A person who answers for something carries the
yellow dot before their name.

## Space, radius, elevation

- Spacing: Tailwind's 4px scale; the unit is called bee space. Rows 12–20px vertical padding.
  The page column has margins that scale with the window (5vw at the sides, 7vh above, 14vh
  below from `md`; 20px and 32px on a phone) and is at most 1120px wide; the rail is 228px.
- Sections are separated by space (40px between a page header and its first section, 40–56px
  between sections), not borders. A hairline (`border-line`, or `.rule`) separates rows in a list
  and sits under tab rows; nothing else gets a border at rest. No boxed panels: `Panel` is a
  small grey label (13px, weight 500, `ink-2`) and its content.
- Radius: controls are pills (`rounded-full`). `xs` 4px (focus), `sm` 6px (keys), `md` and `lg`
  10px (rail items, menu items), `xl` 12px (menus, popovers, code blocks), `2xl` 18px (dialogs,
  text areas).
- Elevation: none at rest. `shadow-overlay` only on the palette, dialogs, menus, popovers and
  toasts, which sit on `popover`.
- Avatars: the generated marks in `components/avatar.tsx` (`PrincipalMark` renders them). Humans
  are circles, agents rounded squares with a runtime badge, services square tiles with a diamond.
  28px or more in lists, 20px inline in a sentence.
- Icons: Phosphor through `components/icons.tsx` (`<Icon name="…" />`), regular weight. 16px in
  text, 18px in buttons, menus and the palette. The rail and the repository tabs are words only.
- The brand is the yellow dot beside the word Beehive (`Wordmark`, `BrandMark`), and
  `app/icon.svg` is the same dot on paper (on near-black where the system is dark). No logo
  beyond the dot; hexagons appear only in the comb.

## The kit

Every interactive piece is in `apps/web/components/kit/`, built on UI SDK (`@ui-sdk/react` and
`@ui-sdk/core`, pinned to an exact beta in `package.json`) and styled with the tokens above.
`components/ui.tsx` and `components/menu.tsx` re-export it under the names pages already use.
`globals.css` declares `@layer ui-sdk;` as its first line, as UI SDK's styling guide requires, so
the app's styles win over the small amount of CSS the primitives add.

| Piece | File | Notes |
|---|---|---|
| `Button`, `ButtonLink`, `buttonClass` | `kit/button.tsx` | Pills: 46px, 34px for `sm`. `primary` is solid ink (one per screen), `secondary` a hairline outline, `ghost` plain until hovered, `danger` the outline with the word in `bad`. An `onClick` that returns a promise holds the button pending |
| `ConfirmButton`, `askPerson`, `ConfirmHost` | `kit/confirm.tsx` | The one wrapper for a control that approves or destroys something; see "Controls that need a person" |
| `Modal` and the dialog parts | `kit/dialog.tsx` | The top layer: no portal, no z-index. One backdrop and one enter and exit motion, in `globals.css` |
| `Menu`, `MenuLink`, `MenuButton`, `MenuLabel` | `kit/menu.tsx` | A button that opens a short list. `PageMenu` (in `components/menu.tsx`) is a page's one "…" menu |
| `Popover` | `kit/popover.tsx` | A button that opens a small panel (Clone) |
| `Tabs`, `TabsList`, `TabsTab`, `TabsPanel` | `kit/tabs.tsx` | Tabs that switch panels in place: plain words over a hairline, the selected one underlined in ink |
| `TabLinks` | `kit/tab-links.tsx` | Tabs that are pages: links in a named `nav`, the current one marked with `aria-current` |
| `Field`, `Input`, `Textarea`, `Switch`, `Select` | `kit/field.tsx` | 46px pills on the field surface inside a hairline; 2px ink on focus. A field ties its control to the label, the hint and the error. `Select` is the browser's own `<select>`, styled |
| `EmptyState` | `kit/empty.tsx` | A sentence saying what the place is for, and the one next step. No picture, no invented number |
| `cx`, `tw` | `kit/cx.ts` | `tw(base, extra)` drops the kit's class for a property the caller sets (height, padding, radius, text size), so an override never depends on stylesheet order |

The command palette is UI SDK's Command inside `Modal` (`components/command-palette.tsx`).

### Controls that need a person

A control that approves or destroys something is a `ConfirmButton`:

```tsx
<ConfirmButton agentConfirm="Approves and merges pull request #41" consequence="approving">
  Approve and merge
</ConfirmButton>
```

`agentConfirm` is one plain sentence saying what pressing it does; it becomes UI SDK's
`data-agent-confirm`. `consequence` is `"approving"` (solid ink by default) or `"destructive"`
(the `danger` look by default). An agent driving the page sees the sentence in its snapshot and
cannot press the control on its own: the step waits for a person, who is asked in the kit's
alert dialog (`askPerson`), and without one it fails with "Needs a person".
`components/kit/confirm.test.ts` reads every component's source and fails when an element with
`consequence` or the `danger` look has no `agentConfirm`, when the `danger` look is put on a raw
element, or when a file calls a listed approving or destroying operation without marking a control.

## The app shell

One navigation system (`components/app-shell.tsx`):

- Signed in: a 228px left rail on the page ground with a hairline on its right. The brand dot
  and the word Beehive, with the palette button; then words, no icons: **Needs you** (with its
  count as a plain number), **Feed**, **Repositories**; under the organisation's name as an
  eyebrow, **People and teams**, **Agents and budgets**, **Record**; under "Recent", the
  repositories this browser opened last, written `owner / name` at 14px. The current item sits
  on `selected` in ink; the rest are `ink-2`. At the foot: the person (the yellow dot and their
  name, a link to their profile), the Person · Agent switch and the theme switch, both `.seg`
  pills whose pressed half is solid ink. Below `md` the rail is a drawer behind a 56px top bar,
  and the drawer also lists the repository's sections.
- Signed out: a 64px top bar with a hairline under it: the wordmark, Explore (and Pricing on
  cloud), search, theme, Sign in (the solid button).
- Repo pages add the repo header (`components/repo-header.tsx`), the same on every page: one line
  at 15px with `owner / name` (the name in ink, weight 500), the Public, Private or Internal
  word with a shape, the description in grey, and on the right Follow, Star and Fork as small
  pills written `Follow · 3`, then the "…" menu. The fork origin, when readable, is a quiet line
  with its sync control when needed. On the repo's home and Code a quiet line follows
  with the branch, file count, latest merge and Clone. Then plain tabs over a hairline: Code,
  Issues, Pull requests, Agents, CI, Safety, Insights, Settings, and More.
- Each page has at most one "…" menu (`PageMenu` in `components/menu.tsx`), and *Copy as* lives
  only there and on `y`.

## Two modes

`<html data-mode>` is `keeper` for a person and `bee` for an agent, chosen on the server
(`apps/web/lib/mode.ts`). The agent's view is the same page, presented for reading by a program:

- `main` is set in mono at 13.5px on a 1.7 line, and every title in it is 15px, weight 600.
- Nothing moves: every animation and transition is off.
- The page opens with a short header in plain text (`components/bee-header.tsx`): who the reader
  is and whom it answers to, where the operations are listed, and whether this browser has
  registered the page's tools.

A person can preview it with the Person · Agent switch in the rail. Tailwind variants `bee:` and
`keeper:` style one mode only.

## Motion

Easing `cubic-bezier(0.33, 1, 0.68, 1)`. Colour and outline changes take 160ms.

| Token | Duration | Use |
|---|---|---|
| hover, press, focus | 160ms | Colour and outline changes |
| popups | 160ms fade, 180ms 4px rise | Dialogs, the palette, menus, popovers, from UI SDK's starting and ending styles |
| `animate-enter` | 180ms, 4px rise | Toasts |
| `animate-arrive` | 260ms, a neutral tint fading out | A live event joining the stream |
| `animate-pulse-dot` | 1.6s loop | Live and running indicators only |

### Event pictures

The pictures use inline SVG in ink tones. A drawn body fits within 16px and the person's
arrival alone carries the yellow `--mark` dot. Each motion runs once and leaves its resting
picture. Quiet work lists always show a still picture.

| Event | Duration | Motion |
|---|---|---|
| Loading | 1600ms, ease-in-out | One figure eight |
| Task finished | 600ms, standard ease-out | Fill the cell and draw its cap line |
| Task started | 420ms, standard ease-out | Two quick turns |
| Merge confirmed | 700ms, standard ease-out | Arc to the baseline below the title |
| Merge refused | 580ms, ease-in-out | Approach the guard marks and turn back |
| Work paused | 720ms, standard ease-out | Three soft wisps rise |
| Your request to join was accepted | 640ms, standard ease-out | The marked picture flies in |

With `prefers-reduced-motion: reduce` or `html[data-mode=bee]`, these pictures go straight to
rest with no animation. Text and controls are available immediately in every mode.

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
navigation never flashes one. The skeleton bars are still. The loading indicator plays one
figure eight beside its text and then rests. Swapping skeleton for data moves nothing above
the fold.

## Patterns

**The pieces every redesigned screen is made of** (`components/parts.tsx`; text and layout only,
controls come from the kit):

- **Mark.** The yellow dot, then the name of the person who answers for the work. It appears
  beside that person and nowhere else on a screen: not on an author who is merely named, not on
  a count, not on a state.
- **Row.** A sentence in `text-lg`, weight 300, between hairlines (20px above and below), with one
  small line under it in `text-xs` `ink-2` whose facts are separated by a middle dot. A row that
  is a link takes `hover` on pointer.
- **Section.** A hairline above, the label in a 160px left column (`text-xs`, medium, `ink-2`) and
  the content beside it; under the label below `md`. Sections are 40px apart.
- **Label and value pairs.** A 130px label column in `ink-2`, values in `ink`; a pair with no
  value is left out rather than shown empty.
- **One click away.** Detail that is not the first thing to read (every rule, the checks, the
  evidence, all the files) sits behind a native disclosure whose summary is an underlined phrase
  in `ink-2`. Nothing is removed, only folded.
- **One solid button per screen.** The screen's main decision is solid ink; every other button
  is an outline. A pressed choice among several is a tick and the `selected` ground, not solid.
- **Branch and target** are mono with an arrow, `ws/billing/tax → main`; the arrow is hidden from
  screen readers, which hear "into".

Reusable compositions for the discovery surfaces. They use only the tokens above; the Character
rules apply: no gradients, glass, resting shadows, emoji or all-caps micro labels.

**Repo card.** No border: a row with 16px padding that takes `hover` on pointer. Top line: the
full name `owner/repo` in `text-lg`, as a link, then the visibility badge (Public
outline, Private neutral, with its word). Below, the description in `ink-2`, two lines at most,
truncated with an ellipsis. Bottom line in `text-xs`, `ink-3`: a language dot (8px circle plus the
language name), stars with a star icon and `.num` count, and updated-ago (`Time`, absolute in
`title`). The dot is never the only carrier of the language: the name is always printed.

**Stat strip.** Inline numbers on one row, wrapping on mobile: each is a `.num` value in `ink`
`text-lg` followed by its label in `ink-3` `text-xs`, separated by 24px. No boxes, no dividers.
A changing value uses `Swap`.

**Rank row.** A 48px row: the rank number in `.num` at a fixed 32px width, `ink-3`, right
aligned; then the principal mark (circle, rounded square or diamond), the name as a link and, for
agents, the runtime in `ink-3`; the score on the far right, `.num`, `ink`. Rows are `j`/`k` targets
with a visible focus ring. The top three differ only by the rank number weight, not by colour or
medals.

**Contribution heatmap.** A grid of 11px cells, 2px gap, one column per week. Five steps: step 0 is
`sunken`; steps 1 to 4 are `ok` at 25%, 45%, 70% and 100% alpha over `sunken`. Steps come
from the count quartiles of the displayed year, so a quiet profile still shows shape. A legend
("Less" to "More") with the five swatches sits below. Colour is never the only signal: the grid has
an accessible text summary ("412 contributions in the last year, busiest week of 3 March"), each
cell is described by a `title` ("3 contributions on 3 March"), and the grid is exposed as a table
with a visually hidden caption.

**Hero type step.** `text-3xl`: 48 / 52, weight 300, tight tracking. Used for the headline of the
marketing landing; every other page title stays `text-2xl`. Below 640px the hero steps down to
`text-2xl`.

**Landing section rhythm.** Sections on the marketing landing are separated by 96px of vertical
space on desktop and 56px on mobile (below 640px). Content is at most 1120px wide. Section
headings are `text-xl`, one idea per section,
and nothing animates except the shared hover and enter motion.

### Brief patterns

The pull request's overview and the pull-request list (`docs/spec/web-ui.md` §3). Evidence is the
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

Light and dark are the two halves of every `light-dark()` pair. The page **follows the system
setting by default** (`color-scheme: light dark` on `:root`), with no script. When the person
picks a theme it is stored in `localStorage["bee-theme"]` and overrides the system: a script
inlined at the top of `<body>` (`THEME_SCRIPT` in `components/theme.tsx`) writes the resolved
theme to `<html data-theme>` before the first paint, and `[data-theme]` pins `color-scheme`, so
there is no flash. The switch (System · Light · Dark) sits at the foot of the rail, and in the
top bar when signed out; System forgets the stored choice.
