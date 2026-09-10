// The whole cell, as last swept. One store, so the rail's counts, the inbox,
// the graph and the map all read the same rows rather than each asking again.

import { useSyncExternalStore } from "react";
import type { Resource } from "@/lib/model";

export type CensusRows = Record<string, Resource[]>;

let rows: CensusRows = {};
let sweptAt = 0;
const listeners = new Set<() => void>();

export function setCensusRows(next: CensusRows) {
  rows = next; sweptAt = Date.now();
  for (const l of listeners) l();
}

export function useCensusRows(): CensusRows {
  return useSyncExternalStore(
    (l) => { listeners.add(l); return () => listeners.delete(l); },
    () => rows, () => rows,
  );
}
export const censusSweptAt = () => sweptAt;
