// What the console remembers: who is signed in, which project, how it looks,
// and where you have been. One small external store, read with a hook, so no
// component owns state that two panes need.

import { useSyncExternalStore } from "react";

export type Theme = "system" | "dark" | "light";
export type Density = "comfortable" | "compact";
export type Motion = "auto" | "reduced";

type State = {
  who: { subject: string; displayName: string; cellAdmin: boolean; projects: Record<string, string> } | null;
  project: string;
  theme: Theme;
  density: Density;
  motion: Motion;
  recents: string[];        // "collection/id"
  views: Record<string, { sorting: unknown; visibility: Record<string, boolean>; filter: string; labels: string }>;
  railCollapsed: Record<string, boolean>;
};

const KEY = "velstra-react-prefs";

const load = (): Partial<State> => {
  try { return JSON.parse(localStorage.getItem(KEY) ?? "{}"); } catch { return {}; }
};

let state: State = {
  who: null,
  project: "p1",
  theme: "system",
  density: "comfortable",
  motion: "auto",
  recents: [],
  views: {},
  railCollapsed: {},
  ...load(),
};
state.who = null;

const listeners = new Set<() => void>();
const emit = () => { for (const l of listeners) l(); };

export const getState = () => state;

export function setState(patch: Partial<State>) {
  state = { ...state, ...patch };
  const { project, theme, density, motion, recents, views, railCollapsed } = state;
  try { localStorage.setItem(KEY, JSON.stringify({ project, theme, density, motion, recents, views, railCollapsed })); } catch { /* private mode */ }
  applyTheme(state.theme);
  document.documentElement.dataset.density = state.density;
  document.documentElement.dataset.motion = state.motion;
  emit();
}

export function remember(ref: string) {
  const recents = [ref, ...state.recents.filter((r) => r !== ref)].slice(0, 12);
  setState({ recents });
}

export function applyTheme(t: Theme) {
  const root = document.documentElement;
  if (t === "system") {
    const dark = matchMedia("(prefers-color-scheme: dark)").matches;
    root.setAttribute("data-theme", dark ? "dark" : "light");
  } else root.setAttribute("data-theme", t);
}

export function useStore<T>(select: (s: State) => T): T {
  return useSyncExternalStore(
    (l) => { listeners.add(l); return () => listeners.delete(l); },
    () => select(state),
    () => select(state),
  );
}

applyTheme(state.theme);
document.documentElement.dataset.density = state.density;
document.documentElement.dataset.motion = state.motion;
matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () => {
  if (state.theme === "system") applyTheme("system");
});
