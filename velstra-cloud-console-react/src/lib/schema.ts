// The schema is the console. Thirty-three collections, each with its columns,
// its fields and what may be done to it — extracted from the Rust crate's
// `schema.rs`, which is the same source the API is written against. A new
// collection is a schema entry; nothing in this app is written per collection
// unless `registry.tsx` says so.

import raw from "../schema.json";

// The field and column shapes are **generated** — `src/schema.d.ts`, written
// from the same document this file reads, and pinned by
// `velstra-cloud-console/tests/react_schema.rs`. Written by hand they were a
// second copy of the schema that drifted in silence: eleven keys were
// serialised and shipped, and naming one of them here was a compile error, so
// every `check`, every unit and every `min`/`max` sat in the JSON unread.
export type { Field, Column } from "../schema.d";
import type { Field, Column } from "../schema.d";

export type FieldKind = Field["kind"];

/** One kind of field, with the keys that kind actually carries. */
export type FieldOf<K extends FieldKind> = Extract<Field, { kind: K }>;
/** One kind of column, likewise. */
export type ColumnOf<K extends Column["cell"]> = Extract<Column, { cell: K }>;

/** Fields that point at another object — the two kinds with a `collection`. */
export const isRef = (f: Field): f is FieldOf<"ref"> | FieldOf<"refList"> =>
  f.kind === "ref" || f.kind === "refList";

export type Agreement = { label: string; asked: string; is: string; note: string };

/** Whose screen a collection is. See `Audience` in the Rust schema. */
export type Audience = "tenant" | "plumbing" | "operator";

export type Collection = {
  id: string;
  title: string;
  singular: string;
  group: string;
  scope: "project" | "global";
  audience: Audience;
  blurb: string;
  condition: string;
  recheck: number;
  creatable: boolean;
  editable: boolean;
  deletable: boolean;
  explainable: boolean;
  columns: Column[];
  fields: Field[];
  agreements: Agreement[];
};

export const SCHEMA = raw as unknown as Collection[];

export const collection = (id: string) => SCHEMA.find((c) => c.id === id);

/** Rail order. Two vocabularies, because there are two readers. */
export const GROUP_ORDER = ["Compute", "Storage", "Network", "Fleet", "Access", "Cell"];

/**
 * What this person navigates by.
 *
 * A customer gets the fourteen collections they manage. They do not get
 * `ports`, `attachments`, `captures` or `operations` — those are real, they are
 * theirs, and they are made and unmade by the thing that needs them; they are
 * reachable from that thing and from the map, not from the navigation. And they
 * do not get the cell's fifteen at all, which is not a courtesy: the API refuses
 * a tenant every read of `migrations`, `nodes`, `pools`, `device-classes` and
 * `backup-targets`, so a rail entry for one was a link to a red failure.
 */
export const navigable = (cellAdmin: boolean) =>
  SCHEMA.filter((c) => (cellAdmin ? c.audience !== "plumbing" : c.audience === "tenant"));

/**
 * The rail's groups. An operator's words are not a customer's: "Fleet" and
 * "Cell" are what somebody who owns the hardware calls it, and for a customer
 * they held one item each.
 */
const TENANT_GROUP: Record<string, string> = {
  Compute: "Compute", Storage: "Storage", Network: "Networking", Access: "Usage",
};

export const groups = (cellAdmin = true) => {
  const items = navigable(cellAdmin);
  return GROUP_ORDER
    .map((name) => ({
      name: cellAdmin ? name : TENANT_GROUP[name] ?? name,
      items: items.filter((c) => c.group === name),
    }))
    .filter((g) => g.items.length);
};

/** Where a collection lives on the wire. */
export const basePath = (c: Collection, project: string) =>
  c.scope === "project" ? `/api/v1/projects/${project}/${c.id}` : `/api/v1/${c.id}`;

/** A value at a dotted path, the way a column names it. */
export const at = (obj: unknown, path: string): unknown =>
  path.split(".").reduce<any>((o, k) => (o == null ? undefined : o[k]), obj);

/** The project that means every project at once — an operator's view. A
 *  tenant never sees it: the picker offering it is behind `cellAdmin`. */
export const ALL = "*";

/** The project a name lives under, read off `projects/<p>/…`. */
export const projectOf = (name: string): string | undefined => /^projects\/([^/]+)\//.exec(name)?.[1];

/** The path an existing object is reached at — its own project's, never
 *  the picker's, so an operator looking at every project at once edits the
 *  right one. */
export const pathOf = (c: Collection, r: { meta: { name: string } }, fallback: string) =>
  basePath(c, projectOf(r.meta.name) ?? fallback);

/** The id a route carries for an object: `db-1`, or `nord/db-1` when the
 *  board spans projects and `db-1` alone would be ambiguous. */
export const routeId = (c: Collection, r: { meta: { name: string } }, project: string) => {
  const id = r.meta.name.split("/").pop() ?? r.meta.name;
  return project === ALL && c.scope === "project" ? `${projectOf(r.meta.name) ?? ""}/${id}` : id;
};

/** The reverse: which project and which id a route's id means. */
export const resolveId = (c: Collection, project: string, id: string): { project: string; id: string } => {
  if (c.scope === "project" && id.includes("/")) {
    const [p, ...rest] = id.split("/");
    return { project: p, id: rest.join("/") };
  }
  return { project, id };
};
