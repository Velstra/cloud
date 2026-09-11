// The whole cell, as last swept. One store, so the rail's counts, the inbox,
// the graph and the map all read the same rows rather than each asking again.
//
// **And what was not swept.** The sweep used to be rows alone: a collection
// whose read failed was simply absent, indistinguishable from one that was
// read and found empty. That difference is the whole of the sentence beside
// the Delete button — "Nothing — safe to remove on its own" was said about a
// volume attached to a running guest, because the attachments read 403'd and
// nothing recorded that it had not been looked at. A truncated list is the
// same failure more quietly: a prefix of a collection read as the collection.

import { useSyncExternalStore } from "react";
import type { Resource } from "@/lib/model";

export type CensusRows = Record<string, Resource[]>;

/** What the last sweep actually managed, and what it did not. */
export type Census = {
  rows: CensusRows;
  /** Collection id → why it could not be read. */
  missing: Record<string, string>;
  /** Collections that hit the page cap, so what is held is a prefix. */
  truncated: string[];
  sweptAt: number;
};

let census: Census = { rows: {}, missing: {}, truncated: [], sweptAt: 0 };
const listeners = new Set<() => void>();

export function setCensus(next: Omit<Census, "sweptAt">) {
  census = { ...next, sweptAt: Date.now() };
  for (const l of listeners) l();
}

export function useCensus(): Census {
  return useSyncExternalStore(
    (l) => { listeners.add(l); return () => listeners.delete(l); },
    () => census, () => census,
  );
}

export function useCensusRows(): CensusRows {
  return useCensus().rows;
}

export const censusSweptAt = () => census.sweptAt;

/**
 * Whether this sweep is in a position to answer "does anything point here".
 *
 * Anything unread or cut short and the answer is no — and saying so is the
 * point: an incomplete sweep that reports "nothing" beside an irreversible
 * button is worse than one that says it could not look.
 */
export function whole(c: Census): boolean {
  return c.sweptAt > 0 && !Object.keys(c.missing).length && !c.truncated.length;
}

/** Why it is not whole, in a sentence, or "" when it is. */
export function whyNotWhole(c: Census): string {
  if (!c.sweptAt) return "Not looked yet.";
  const unread = Object.keys(c.missing);
  const cut = c.truncated;
  if (!unread.length && !cut.length) return "";
  const parts: string[] = [];
  if (unread.length) parts.push(`${unread.length === 1 ? "1 collection was" : unread.length + " collections were"} not readable (${unread.join(", ")})`);
  if (cut.length) parts.push(`${cut.length === 1 ? "1 collection was" : cut.length + " collections were"} too long to read whole (${cut.join(", ")})`);
  return parts.join(", and ") + ", so this is not an answer.";
}
