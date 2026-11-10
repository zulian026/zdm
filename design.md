# Design: ZDM

The locked system. Every Hallmark run reads this first and pages share it. Change it on purpose; the file is the rule.

## System
- Genre: modern-minimal
- Shape of the first page: app screen
- Theme: Console (deal 1/3, seed download-manager-desktop-tool-3135·472bf7ff·2026-11-09·r0)
- Axes: dark/grotesk-sans/chromatic-amber · posture: restrained

## Tokens
`src/tokens.css` is the source of truth. The roles:
```css
:root {
  --color-paper: oklch(17% 0.008 250);  --color-paper-2: oklch(21% 0.009 250);  --color-paper-3: oklch(13.5% 0.007 250);
  --color-ink: oklch(95% 0.005 250);  --color-ink-2: oklch(83% 0.008 250);  --color-muted: oklch(71% 0.01 250);
  --color-rule: oklch(31% 0.01 250);  --color-rule-2: oklch(42% 0.012 250);
  --color-accent: #c6f24e;  --color-accent-ink: oklch(18% 0.04 75);  --color-accent-bright: oklch(92% 0.12 80);
  --color-accent-soft: oklch(29% 0.05 80);  --color-focus: #c6f24e;
  --color-ok: oklch(76% 0.15 150);  --color-ok-soft: oklch(24.5% 0.03 150);
  --color-bad: oklch(70% 0.17 25);  --color-bad-soft: oklch(24.5% 0.035 25);
  --color-scrim: oklch(8% 0.005 250 / 0.7);
  --font-display: "Host Grotesk", ui-sans-serif, system-ui, sans-serif;  --font-body: var(--font-display);
  --font-outlier: "Atkinson Hyperlegible Mono", ui-monospace, monospace;
  --radius-card: 10px;  --radius-pill: 6px;  --rule-hair: 1px;  --shadow-card: none;
}
```
The spacing scale, the type scale, the easings and the durations are the scaffold's and live in `src/tokens.css`.
Brand: lime #c6f24e is the one accent (icon, segment strip, chart line, speed figures). Console's cool blue is not used as a second signal.

## Controls
- Primary: ink fill, paper-3 text · 6px radius · 36px height
- Secondary: paper-2 fill, 1px rule border · same radius and height
- Selection (active filter pill / tab / scrub day): accent-soft fill, accent text, transparent border — a selection state, never a second primary. Added in the finish-review fix round (2026-11-09): the old inverted ink pill read as a second filled primary beside Add URL (F4).
- Short numeric setting panels hug their field (`.setting-panel--short`, 160px) instead of spanning the row half empty.

## Motion
- Character: still; 120ms colour-only transitions, no entrance, no scale; under reduced motion everything ends at once.

## Shell and signature
- Shell: 224px sticky sidebar, #app 100dvh, only main#content scrolls on desktop; Menu sheet on phone.
- Signature: per-download segment strip (lime done, bright lime active, red failed, rule-2 pending).
- Measured figures (speeds, sizes, ETAs, counts, hashes, URLs, chart labels) in Atkinson Hyperlegible Mono; labels, prose and controls in Host Grotesk.
- No gradients, no glow, no shadows on resting surfaces. Colour only on data.

## Exports
`src/tokens.css` in this project. Tailwind, DTCG or shadcn blocks are added here on request, per `export-formats.md`.
