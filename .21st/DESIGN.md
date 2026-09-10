<!-- Scaffolded by 21st, then written by hand. `21st init --design-context`
     detects nothing in this repo — it looks for a JS framework and a token file
     it recognises, and this console is a Rust crate that inlines its own CSS and
     JS. Everything below was read out of those files or measured in a browser.
     `21st init --design-context --refresh` would overwrite both this and
     .21st/design.json; edit design.json and mirror it here. -->
# Project Design Context

## Project

- Name: velstra-cloud-console
- Product type: operator console for an IaaS control plane
- Stack: Rust crate, vanilla JS and CSS, no framework, no build step
- Color mode: dark-first; both appearances re-pointed from one token set
- Density: compact — dense tables, 13px body, 44px row pitch

## Sources

- Tokens: `src/assets/tokens.css`, `src/assets/light.css`
- Components: `src/assets/console.css`, `board.js`, `detail.js`, `form.js`, `model.js`, `shell.html`
- Instructions: the module doc in `src/lib.rs`; the reasoning in the comments of `tokens.css`

## Components

Preferred primitives — reuse these rather than building a lookalike:

- `.btn`, `.btn.primary`, `.btn.quiet` — the quiet one takes its colour when the pointer arrives
- `.btn.busy` — a press that waits on the API says so; do not invent a spinner
- `.state.<verdict>` + `mark()` — the verdict word and its dot, the one way status is shown
- `.overpanel`, `.overrow`, `.linky` — the overview's panel, row and way-through
- `.tally`, `.tallybtn` — a count broken into parts that narrow the list under them
- `.tblwrap > table` — every wide thing scrolls in its own container
- `.railitem`, `.railhome` — navigation; `.on` is the active one
- `el()` in `dom.js` — the only element factory; there is no template language

Patterns: spec vs status with a verdict derived in one function, so no two
screens disagree; board → sheet → form; the margin column carries the label and
the content column carries the answer.

## Tokens

- **Colour** — ground `--bg-app`; surfaces `--surface`, `-raised`, `-sunken`,
  `-hover`; four text levels `--text-strong` → `--text-faint`, every one at or
  above 4.5:1 on every ground it is painted on; `--brand` (signal blue) for
  interaction only; `--green/amber/red-500` for dots and `-300` for the same
  three at text weight; `--product` for the platform's own mark.
- **Type** — `--font-sans` is the system stack and nothing else; `--font-mono`
  for object names, paths, identifiers, log lines. Scale `--text-2xs` .6875rem
  through `--text-hero` 2.5rem; tracking is a function of size.
- **Space** — `--space-1` .25rem through `--space-9` 3rem; the reading layout is
  `--gutter` 180px plus `--gutter-gap` 28px; the rail is 232px.
- **Radius** — 4px default, 6px the largest ordinary surface, pill for dots only.
- **Elevation** — a 1px border and a shadow; never a tint or a blur.
- **Motion** — `--dur-press` 100ms through `--dur-slow` 320ms; `--ease-settle`
  is critically damped, arriving without overshoot.

## Constraints

### Must

- Fetch nothing. No CDN, webfont, icon set or framework: an operator opens this
  during an incident, sometimes from a network that reaches only the API.
- Define every custom property you reference. A `var()` with no definition and
  no fallback is an invalid declaration that silently does nothing — 25 of them
  were found this way on 2026-09-06.
- Keep both appearances resolving from one token set; never write a colour
  literal in a component rule.
- Every text colour clears 4.5:1 on every ground it is actually painted on,
  measured by compositing rather than read off the token table.
- Colour is never the only carrier: a verdict is a word and a dot, not a hue.
- Every animation has a reduced-motion equivalent that carries the same fact
  without moving.

### Avoid

- Glass, blur, translucent materials — "on a page of dense tables it costs twice".
- Radii above 6px — "a rounded card invites you to swipe it".
- Spring and bounce — "a panel that bounces on open is a panel an operator sees
  bounce forty times a day".
- Numbers that count up: a figure that animates is a figure you cannot read
  while it moves.
- The interaction colour as a status or a progress fill: signal blue "never
  means healthy, and it never fills a progress bar".
- Decorative gradients, hero headlines, icon-only controls.

## Decisions

- **2026-09-06** — The light ramp re-points the `-300` status colours as well as
  the `-500` ones. `.state` writes every verdict in the `-300` ramp, which
  `light.css` never re-pointed: "not settled" read 1.47:1 on a white page.
- **2026-09-06** — `--slate-300` added; `--text-faint` points at it instead of
  `--ink-300`, which read 2.99:1 on a raised card.
- **2026-09-06** — Six token names that resolved to nothing were mapped onto the
  real vocabulary (`--text`, `--text-dim`, `--mono`, `--accent`, `--radius`,
  `--border-faint`), restoring three focus and active-state indicators.
- **2026-09-06** — The attention list is ordered by verdict, not by collection.
- **2026-09-06** — One animation was added to the boards and no others: a row
  whose verdict changed is washed once, with a 2px edge instead under reduced
  motion. Motion that only decorates was declined.
- **2026-09-06** — Attention rows were left as they are: `.linky` is already a
  focusable button, and "it is a way through, not an action". What was missing
  was its hover colour, which was one of the dead tokens.
