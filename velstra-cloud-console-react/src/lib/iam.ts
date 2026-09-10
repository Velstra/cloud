// Who may do what, as the console reads it — so a button that the API would
// refuse is not drawn for a person it would refuse. The API stays the judge;
// this only keeps the console honest about it.
//
// The platform's answer is a ladder of four rungs per project, plus roles a
// cell wrote down per collection. The session says which rung the caller
// holds in each project; a cell operator holds everything everywhere.

import { useEffect, useState } from "react";
import { call } from "@/api/transport";
import { useStore } from "@/app/store";
import { SCHEMA, type Collection } from "./schema";

export type Rung = "viewer" | "operator" | "editor" | "admin";
export type Verb = "read" | "operate" | "write" | "administer";

/** The ladder, in the platform's own words. */
export const RUNGS: { rung: Rung; may: string; cannot: string }[] = [
  { rung: "viewer", may: "look at everything in the project", cannot: "change anything" },
  { rung: "operator", may: "run what is already there — start, stop, resize, attach, open a console", cannot: "bring anything into existence or take it away" },
  { rung: "editor", may: "that, and create and delete", cannot: "change who may" },
  { rung: "admin", may: "everything, including who may", cannot: "raise the project's own limits — those are the cell's" },
];

const ORDER: Record<Verb, number> = { read: 0, operate: 1, write: 2, administer: 3 };
const TOP: Record<Rung, Verb> = { viewer: "read", operator: "operate", editor: "write", admin: "administer" };

export const rungAllows = (rung: Rung, verb: Verb) => ORDER[TOP[rung]] >= ORDER[verb];
export const isRung = (s: string): s is Rung => s in TOP;

export type CustomRole = { displayName?: string; description?: string; grants: { verb: Verb; collections: string[] }[] };

let cached: Record<string, CustomRole> | null = null;
/** The roles the cell wrote down, by their binding spelling (`roles/x`). */
export async function loadRoles(): Promise<Record<string, CustomRole>> {
  if (cached) return cached;
  const a = await call("list:roles", "GET", "/api/v1/roles", { pageSize: 200 });
  const out: Record<string, CustomRole> = {};
  for (const r of a.items ?? []) out[r.meta.name] = r.spec ?? { grants: [] };
  cached = out;
  return out;
}
export const forgetRoles = () => { cached = null; };

/** `can(verb, collection?, project?)` for the signed-in person. */
export function useCan() {
  const who = useStore((s) => s.who);
  const picked = useStore((s) => s.project);
  const [roles, setRoles] = useState(cached);
  useEffect(() => { if (!roles && who?.cellAdmin) loadRoles().then(setRoles).catch(() => setRoles({})); }, [roles, who]);
  return (verb: Verb, coll?: Collection | string, project?: string): boolean => {
    if (!who) return false;
    if (who.cellAdmin) return true;
    const c = typeof coll === "string" ? SCHEMA.find((x) => x.id === coll) : coll;
    // Objects outside every project are the cell operator's.
    if (c && c.scope !== "project") return false;
    const p = project ?? picked;
    const held = who.projects?.[p];
    if (!held) return true;         // not told: let the API say no, with its reason
    if (isRung(held)) return rungAllows(held, verb);
    // A role the cell wrote down grants per collection; read is implied.
    const role = roles?.[held];
    if (!role) return true;
    if (verb === "read") return true;
    return role.grants.some((g) => ORDER[g.verb] >= ORDER[verb] && (!c || g.collections.includes(c.id)));
  };
}
