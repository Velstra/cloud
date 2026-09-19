---
version: alpha
name: Velstra Cloud
description: Compact operational console for cloud administrators and project members.
colors:
  primary: "#2e96c4"
  background: "#0b0f14"
  surface: "#141a22"
  foreground: "#f4f8fb"
typography:
  sans:
    fontFamily: "Geist Variable, system-ui, sans-serif"
  mono:
    fontFamily: "ui-monospace, monospace"
rounded:
  panel: "12px"
spacing:
  navigation: "232px"
  overview-gap: "20px"
components:
  button: {}
  panel: {}
  form: {}
  table: {}
---

# Velstra Cloud design

## Overview

A working cloud console for infrastructure operators and project members. The
signature is a compact operational overview: resources, health and the next
useful action together. Prefer scanable facts over explanatory paragraphs.
Preserve the Velstra blue identity; avoid marketing heroes and decorative charts.

## Colors

The canonical source is `velstra-cloud-console-react/src/index.css` (runtime
ownership). Dark: app #0b0f14, surface #141a22, brand #2e96c4, text #f4f8fb.
Light: app #f4f6fa, surface #ffffff, brand #2179a1, text #0b0e14.
Success, warning and failure retain their semantic tokens and textual labels.

## Typography

Geist Variable for headings and controls, with system-ui fallback. Monospace is
reserved for addresses, identifiers and aligned numeric data. Body 14px; page
titles 28–32px; supporting text 12–13px. English product copy, sentence case.

## Layout

Persistent 232px navigation, grouped by task. Overview summaries lead to resource
boards. Boards own their scrolling; detail and creation panels scroll separately.
On narrow screens show the active detail/form at full width, with a return action.

## Elevation & Depth

Borders separate normal surfaces. Reserve shadows for overlays. Use blue washes
for selection; never make a data panel look like a promotional banner.

## Shapes

Controls use the existing radius tokens; overview panels use 12px corners.
Status indicators pair shape and text. Capacity bars are meters, never fake progress.

## Components

Tokens flow from index.css through Tailwind's `@theme inline` aliases into shared
`components/ui` primitives. `Pressed` owns asynchronous button geometry and
duplicate prevention. `Form` owns validation and help disclosure. `Shell` owns
navigation; `app/census.ts` owns the shared inventory snapshot.

Motion uses --dur-fast (140ms), --dur-base (220ms) and --ease-settle. Animate
arrival and state changes once. Both OS reduced motion and the app preference
disable movement. Scrollbar colors follow border/text tokens in both themes.

## Do's and Don'ts

- Show stale, incomplete and failed reads explicitly; never infer health from missing data.
- Show requested changes separately from observed completion.
- Keep disk erasure and permission consequences visible before confirmation.
- Put extended explanations behind disclosure; retain clear labels and error recovery.
- Keep project-qualified resource links when viewing all projects.
