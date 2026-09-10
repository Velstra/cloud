// Where you are, in the URL. `#/overview`, `#/c/instances`, `#/c/instances/db-1`,
// `#/c/instances/db-1/edit`, `#/c/instances/new`. A link you can send someone.

import { useSyncExternalStore } from "react";

export type Route =
  | { view: "overview" }
  | { view: "map" }
  | { view: "board"; coll: string; id?: string; mode?: "edit" | "new" };

export function parse(hash: string): Route {
  const parts = hash.replace(/^#\/?/, "").split("/").filter(Boolean).map(decodeURIComponent);
  if (parts[0] === "c" && parts[1]) {
    const coll = parts[1];
    if (parts[2] === "new") return { view: "board", coll, mode: "new" };
    if (parts[2]) return { view: "board", coll, id: parts[2], mode: parts[3] === "edit" ? "edit" : undefined };
    return { view: "board", coll };
  }
  if (parts[0] === "map") return { view: "map" };
  return { view: "overview" };
}

export const href = (r: Route) =>
  r.view === "overview" ? "#/overview" : r.view === "map" ? "#/map"
    : `#/c/${r.coll}${r.mode === "new" ? "/new" : r.id ? "/" + encodeURIComponent(r.id) + (r.mode === "edit" ? "/edit" : "") : ""}`;

/// Move, through a cross-fade where the browser can: the old screen is not
/// yanked out from under the pointer. Skipped when motion is reduced.
export const go = (r: Route) => {
  const still = matchMedia("(prefers-reduced-motion: reduce)").matches ||
    document.documentElement.dataset.motion === "reduced";
  const doc = document as Document & { startViewTransition?: (cb: () => void) => void };
  if (!still && doc.startViewTransition) doc.startViewTransition(() => { location.hash = href(r); });
  else location.hash = href(r);
};

export function useRoute(): Route {
  const hash = useSyncExternalStore(
    (l) => { addEventListener("hashchange", l); return () => removeEventListener("hashchange", l); },
    () => location.hash,
    () => location.hash,
  );
  return parse(hash);
}
