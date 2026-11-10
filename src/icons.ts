/* One icon set for the whole UI (taste-skill app-screens §13).
   Outline only: 24x24 viewBox, fill none, stroke currentColor, 1.5px,
   round caps and joins. Paths are simple geometry drawn in Lucide style;
   no emoji, no filled icons, no colour on chrome icons. Sizes come from
   CSS (.icon = 16px, row actions = 14px, empty state = 32px). */

const svg = (inner: string): string =>
  `<svg class="icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true" focusable="false">${inner}</svg>`;

export const ICONS = {
  download: svg(
    `<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><path d="m7 10 5 5 5-5"/><path d="M12 15V3"/>`,
  ),
  checkCircle: svg(
    `<circle cx="12" cy="12" r="10"/><path d="m9 12 2 2 4-4"/>`,
  ),
  settings: svg(
    `<path d="M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.08a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z"/><circle cx="12" cy="12" r="3"/>`,
  ),
  plus: svg(`<path d="M12 5v14"/><path d="M5 12h14"/>`),
  play: svg(`<path d="m7 4.5 13 7.5-13 7.5z"/>`),
  pause: svg(`<path d="M9 5v14"/><path d="M15 5v14"/>`),
  x: svg(`<path d="M18 6 6 18"/><path d="m6 6 12 12"/>`),
  trash: svg(
    `<path d="M3 6h18"/><path d="M8 6V4a1 1 0 0 1 1-1h6a1 1 0 0 1 1 1v2"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6"/><path d="M10 11v6"/><path d="M14 11v6"/>`,
  ),
  search: svg(
    `<circle cx="11" cy="11" r="7"/><path d="m21 21-4.3-4.3"/>`,
  ),
  list: svg(
    `<path d="M9 6h11"/><path d="M9 12h11"/><path d="M9 18h11"/><path d="M4 6h.01"/><path d="M4 12h.01"/><path d="M4 18h.01"/>`,
  ),
  globe: svg(
    `<circle cx="12" cy="12" r="10"/><path d="M12 2a14.5 14.5 0 0 0 0 20 14.5 14.5 0 0 0 0-20"/><path d="M2 12h20"/>`,
  ),
} as const;

export type IconName = keyof typeof ICONS;

export function icon(name: IconName): string {
  return ICONS[name];
}
