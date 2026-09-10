// The schema is the console. Thirty-three collections, each with its columns,
// its fields and what may be done to it — extracted from the Rust crate's
// `schema.rs`, which is the same source the API is written against. A new
// collection is a schema entry; nothing in this app is written per collection
// unless `registry.tsx` says so.

import raw from "../schema.json";

export type FieldKind =
  | "text" | "number" | "switch" | "choice" | "ref" | "refList" | "moment"
  | "lines" | "textList" | "diskList" | "grantList" | "listenerList"
  | "poolList" | "ruleList";

export type Field = {
  key: string;
  label: string;
  kind: FieldKind;
  required: boolean;
  advanced: boolean;
  derived: boolean;
  atCreation: boolean;
  help: string;
  whenEmpty: string;
  options?: { value: string; label: string }[];
  collection?: string;
  filterBy?: string | null;
  spelling?: "id" | "name";
};

export type Column = { label: string; path: string; cell: string; width: number };

export type Agreement = { label: string; asked: string; is: string; note: string };

export type Collection = {
  id: string;
  title: string;
  singular: string;
  group: string;
  scope: "project" | "global";
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

/** Rail order: the operator's vocabulary, not the alphabet. */
export const GROUP_ORDER = ["Compute", "Storage", "Network", "Fleet", "Access", "Cell"];

export const groups = () =>
  GROUP_ORDER.map((name) => ({ name, items: SCHEMA.filter((c) => c.group === name) }))
    .filter((g) => g.items.length);

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
