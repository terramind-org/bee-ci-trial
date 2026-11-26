/**
 * The type stacks. The interface uses the system's own UI face, so no font file is bundled or
 * requested and text paints with the first frame; mono is the system's too, for code, paths,
 * refs and ids. `app/globals.css` declares the same two stacks as `--sans` and `--mono`
 * (lib/tokens.test.ts keeps them equal); they are exported here for the places CSS cannot reach,
 * such as an SVG drawn on a canvas or an image response.
 */
export const SANS_STACK = '-apple-system, BlinkMacSystemFont, "SF Pro Text", system-ui, "Helvetica Neue", Arial, sans-serif';

export const MONO_STACK = 'ui-monospace, "SF Mono", Menlo, monospace';
