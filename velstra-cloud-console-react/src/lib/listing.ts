// A collection, every page of it — and, for an operator looking at every
// project at once, every project of it. The API scopes tenant objects under
// their project and has no cell-wide list of instances, which is right for the
// API: a tenant must never be able to ask for one. The operator's "all
// projects" board is therefore made here, by asking each project in turn, and
// nothing below this file knows the difference.

import { call } from "@/api/transport";
import type { Resource } from "./model";
import { ALL, SCHEMA, basePath, type Collection } from "./schema";

export type Listed = { rows: Resource[]; revision: string; truncated: boolean };

/** How many pages one list may walk before the console stops and says so.
 *  Two hundred pages of two hundred is forty thousand objects — past that a
 *  board is not a board, and a cell with a quarter of a million audit records
 *  would otherwise pull all of them into a browser tab. The old console had
 *  this guard; the rewrite has to keep it. */
const MOST_PAGES = 200;

/** One collection in one project (or the cell), all pages — up to the cap. */
export async function pages(c: Collection, project: string, query: Record<string, unknown> = {}): Promise<Listed> {
  const rows: Resource[] = []; let token: string | undefined; let revision = "";
  let walked = 0; let truncated = false; let previous: string | undefined;
  do {
    const a = await call(`list:${c.id}`, "GET", basePath(c, project), { pageSize: 200, ...query, pageToken: token });
    rows.push(...(a.items ?? [])); revision = a.revision ?? revision;
    previous = token; token = a.nextPageToken;
    // A server that hands back the token it was given would spin here for ever.
    if (token && token === previous) { truncated = true; break; }
    if (++walked >= MOST_PAGES && token) { truncated = true; break; }
  } while (token);
  return { rows, revision, truncated };
}

let known: { at: number; names: string[] } | null = null;

/** Every project's id, kept for a little while: the fan-out below asks for
 *  it once per sweep, not once per collection. */
export async function projectNames(): Promise<string[]> {
  if (known && Date.now() - known.at < 15_000) return known.names;
  const projects = SCHEMA.find((c) => c.id === "projects")!;
  const { rows } = await pages(projects, "");
  const names = rows.map((r) => r.meta.name.split("/").pop() ?? r.meta.name);
  known = { at: Date.now(), names };
  return names;
}

/** The collection as the picker means it: one project, or all of them. */
export async function listEvery(c: Collection, project: string, query: Record<string, unknown> = {}): Promise<Listed> {
  if (project !== ALL || c.scope !== "project") return pages(c, project, query);
  const names = await projectNames();
  const each = await Promise.all(names.map((p) => pages(c, p, query).catch(() => ({ rows: [], revision: "", truncated: false }))));
  return {
    rows: each.flatMap((x) => x.rows),
    revision: each.find((x) => x.revision)?.revision ?? "",
    truncated: each.some((x) => x.truncated),
  };
}
