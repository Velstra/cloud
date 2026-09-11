// A collection, loaded and kept: the rows, the revision they were read at, and
// which rows changed verdict since the last read — the question a refresh
// exists to answer.

import { useCallback, useEffect, useRef, useState } from "react";
import { call } from "@/api/transport";
import { listEvery } from "@/lib/listing";
import { nameOf, verdict, type Resource } from "@/lib/model";
import { basePath, resolveId, type Collection } from "@/lib/schema";
import { useStore } from "@/app/store";

export type Loaded = {
  rows: Resource[];
  revision: string;
  loading: boolean;
  error: string;
  changed: Set<string>;
  /** The list hit the page cap: what is shown is a prefix, not the collection. */
  truncated: boolean;
  /** Something on this list has not settled, so the list is re-reading itself. */
  busy: boolean;
  refresh: () => Promise<void>;
};

const cache = new Map<string, { rows: Resource[]; revision: string }>();

// Who is looking at what, so a write on one screen reaches the list on
// another. A create used to leave the board it was made from without the row:
// the form navigated to the new object and the list beside it still held the
// answer from before. Polling does not help there — the board has no knowledge
// of the object to call busy.
const watching = new Map<string, Set<() => void>>();

/** Say that a collection changed, so every list of it reads again. */
export function collectionChanged(id: string) {
  for (const again of watching.get(id) ?? []) again();
}

export function useCollection(c: Collection | undefined, labels = ""): Loaded {
  const project = useStore((s) => s.project);
  const key = c ? `${basePath(c, project)}?${labels}` : "";
  const [state, set] = useState(() => ({
    rows: c ? cache.get(key)?.rows ?? [] : [], revision: "", loading: !!c, error: "",
    changed: new Set<string>(), truncated: false,
  }));
  const previous = useRef<Map<string, string>>(new Map());
  const run = useRef(0);

  const refresh = useCallback(async () => {
    if (!c) return;
    const mine = ++run.current;
    set((s) => ({ ...s, loading: true, error: "" }));
    try {
      // The list, every page of it. The contract pages with a token; a board
      // that shows the first page and calls it the collection is a board that
      // hides the object somebody is looking for.
      const { rows, revision, truncated } = await listEvery(c, project, { labels: labels || undefined });
      if (mine !== run.current) return;
      const moved = new Set<string>();
      const now = new Map<string, string>();
      for (const r of rows) {
        const k = verdict(r, c).kind;
        now.set(nameOf(r), k);
        const was = previous.current.get(nameOf(r));
        if (was && was !== k) moved.add(nameOf(r));
      }
      previous.current = now;
      cache.set(key, { rows, revision });
      set({ rows, revision, loading: false, error: "", changed: moved, truncated });
    } catch (e) {
      if (mine !== run.current) return;
      set((s) => ({ ...s, loading: false, error: String((e as Error).message) }));
    }
  }, [c, project, labels, key]);

  useEffect(() => { previous.current = new Map(); refresh(); }, [refresh]);

  useEffect(() => {
    if (!c) return;
    const mine = () => { refresh(); };
    const set = watching.get(c.id) ?? new Set();
    set.add(mine); watching.set(c.id, set);
    return () => { set.delete(mine); };
  }, [c, refresh]);

  // **While anything here is working, read it again.**
  //
  // Every write on this platform is asynchronous — a create answers with an
  // operation and the object converges afterwards — so a list that is only
  // ever read once is a list that is wrong from the moment somebody uses it.
  // The clearest case: make a thing, and the board it was made from does not
  // have it. It was not even a stale row; it was an absent one.
  //
  // The rule is the one the detail pane has always used: poll *only* while a
  // verdict says something is busy, and stop the moment everything has
  // settled. A board of settled objects costs nothing, which is why this is
  // not an interval on every screen.
  //
  // `recheck` is the collection's own answer for how fast, and it is there for
  // the objects whose state changes without a write for a watch to carry — a
  // migration past its deadline is decided by the clock. Five seconds for
  // everything else: fast enough that a guest coming up is watched rather than
  // waited for, slow enough that twenty rows are twelve reads a minute.
  const busy = state.rows.some((r) => c && verdict(r, c).busy);
  useEffect(() => {
    if (!c || !busy) return;
    const every = Math.max(3, c.recheck || 5) * 1000;
    const t = setInterval(() => { refresh(); }, every);
    return () => clearInterval(t);
  }, [c, busy, refresh]);

  return { ...state, refresh, busy };
}

/** One object, fresh. */
export async function fetchOne(c: Collection, project: string, id: string): Promise<Resource> {
  const x = resolveId(c, project, id);
  return call(`get:${c.id}`, "GET", `${basePath(c, x.project)}/${encodeURIComponent(x.id)}`);
}
