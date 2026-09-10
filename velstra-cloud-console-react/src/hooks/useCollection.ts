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
  refresh: () => Promise<void>;
};

const cache = new Map<string, { rows: Resource[]; revision: string }>();

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

  return { ...state, refresh };
}

/** One object, fresh. */
export async function fetchOne(c: Collection, project: string, id: string): Promise<Resource> {
  const x = resolveId(c, project, id);
  return call(`get:${c.id}`, "GET", `${basePath(c, x.project)}/${encodeURIComponent(x.id)}`);
}
