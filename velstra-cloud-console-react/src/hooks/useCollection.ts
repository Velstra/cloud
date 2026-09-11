// A collection, loaded and kept: the rows, the revision they were read at, and
// which rows changed verdict since the last read — the question a refresh
// exists to answer.

import { useCallback, useEffect, useRef, useState } from "react";
import { call, watch, type WatchEvent, type WatchState } from "@/api/transport";
import { listEvery } from "@/lib/listing";
import { nameOf, verdict, type Resource } from "@/lib/model";
import { ALL, basePath, resolveId, type Collection } from "@/lib/schema";
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
  /** What the live stream is doing — the third state is what says the screen
   *  will not update itself. */
  live: WatchState;
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
  const [live, setLive] = useState<WatchState>("connecting");
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

  // **One stream per open board.**
  //
  // The API has served `?watch=true` as SSE all along and the other console
  // consumes it; this one polled, and only while a row's verdict said it was
  // busy. What that misses is everything with no verdict to be busy about: a
  // change somebody else made, a controller minting an attachment, a row
  // deleted from another tab.
  //
  // The stream is deliberately *not* narrowed by the label filter on the
  // server: an object that loses the label would simply stop producing events,
  // and its row would sit there for ever saying something that stopped being
  // true. So a PUT that no longer matches removes the row here instead.
  //
  // Only for one project at a time. The ALL board's fan-out would be one
  // stream per project and cannot show anything the list did not; it stays on
  // the read, with the indicator saying so.
  const streamable = !!c && project !== ALL && !!state.revision;
  useEffect(() => {
    if (!c || !streamable) { setLive("unsupported"); return; }
    setLive("connecting");
    const wants = labelFilter(labels);
    const stream = watch(basePath(c, project), state.revision, (e: WatchEvent) => {
      set((s) => {
        const rows = fold(s.rows, e, wants);
        if (rows === s.rows) return s;
        // `previous` is what the changed-row highlight diffs against. Folding
        // one event has to keep it in step, or the next whole-list read lights
        // up every row that moved while the stream was carrying it.
        for (const r of rows) if (c) previous.current.set(nameOf(r), verdict(r, c).kind);
        cache.set(key, { rows, revision: s.revision });
        return { ...s, rows };
      });
    }, setLive);
    return () => stream.stop();
    // The revision is the stream's starting point and must not restart it on
    // every list read, so it is read once when the stream opens.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [c, project, labels, streamable]);

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
  //
  // **And `recheck` on its own, not only `busy`.** A collection whose
  // `condition` is empty is settled unconditionally, so `busy` is never true
  // for it — and `subnets` and `security-groups` carry a `recheck` precisely
  // because their interesting numbers are computed at read time out of *other*
  // objects. A subnet's occupancy is derived from the ports, floating IPs and
  // load balancers that use it; nothing writes to the subnet, so there is no
  // watch event either. Between the two, that board was frozen from the moment
  // it loaded.
  const busy = state.rows.some((r) => c && verdict(r, c).busy);
  const ticking = !!c && (busy || c.recheck > 0);
  useEffect(() => {
    if (!c || !ticking) return;
    const every = Math.max(3, c.recheck || 5) * 1000;
    const t = setInterval(() => { refresh(); }, every);
    return () => clearInterval(t);
  }, [c, ticking, refresh]);

  return { ...state, refresh, busy, live };
}

/** `env=prod, tier=web` as a map, the same reading the list sends. */
function labelFilter(raw: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const part of raw.split(",")) {
    const [k, ...rest] = part.split("=");
    if (k.trim() && rest.length) out[k.trim()] = rest.join("=").trim();
  }
  return out;
}

const matches = (r: Resource, wants: Record<string, string>) =>
  Object.entries(wants).every(([k, v]) => (r.meta.labels ?? {})[k] === v);

/**
 * One event, folded into the rows — or the same array when nothing changed, so
 * React does not re-render a board for an event about a row it does not hold.
 */
function fold(rows: Resource[], e: WatchEvent, wants: Record<string, string>): Resource[] {
  if (e.type === "DELETE") {
    const at = rows.findIndex((r) => nameOf(r) === e.name);
    return at < 0 ? rows : [...rows.slice(0, at), ...rows.slice(at + 1)];
  }
  if (e.type !== "PUT" || !e.resource) return rows;
  const name = nameOf(e.resource);
  const at = rows.findIndex((r) => nameOf(r) === name);
  // An object that has stopped matching the filter leaves the board. The
  // server does not narrow the stream, on purpose: narrowed, this object would
  // simply stop sending events and its row would stay.
  if (!matches(e.resource, wants)) {
    return at < 0 ? rows : [...rows.slice(0, at), ...rows.slice(at + 1)];
  }
  if (at < 0) return [...rows, e.resource];
  return [...rows.slice(0, at), e.resource, ...rows.slice(at + 1)];
}

/** One object, fresh. */
export async function fetchOne(c: Collection, project: string, id: string): Promise<Resource> {
  const x = resolveId(c, project, id);
  return call(`get:${c.id}`, "GET", `${basePath(c, x.project)}/${encodeURIComponent(x.id)}`);
}
