// The cell as a graph, read off the schema rather than drawn by hand.
//
// Every `ref` and `refList` field is an edge: an instance's `flavor` points at
// a flavor, a port's `network` at a network, a volume attachment at both its
// halves. Forty such fields across the schema, and none of them was written
// for this file — so a new collection with a reference is a new edge here
// without anybody drawing it. Understanding an IaaS is understanding this
// graph; this is where the map, the "depends on / used by" strip and the
// blast-radius question all come from.

import { idOf, nameOf, type Resource } from "./model";
import { SCHEMA, at, isRef, type Collection } from "./schema";

export type Node = { coll: Collection; r: Resource };
export type Edge = { from: string; to: string; label: string };   // meta.name → meta.name

export type Graph = {
  nodes: Map<string, Node>;
  out: Map<string, Edge[]>;   // edges leaving a name
  into: Map<string, Edge[]>;  // edges arriving at a name
};

/** A referenced value, as the name it points at. Refs are spelled as ids or
 *  full names depending on the field; both are resolved against the census. */
const resolve = (value: unknown, target: Collection, index: Map<string, string[]>): string[] => {
  const raw = Array.isArray(value) ? value : value == null || value === "" ? [] : [value];
  return raw.flatMap((v) => {
    const s = String(v);
    if (s.includes("/")) return [s];
    return index.get(`${target.id}/${s}`) ?? [];
  });
};

export function buildGraph(census: Record<string, Resource[]>): Graph {
  const nodes = new Map<string, Node>();
  const byId = new Map<string, string[]>();   // "coll/id" → [full names]
  for (const c of SCHEMA) for (const r of census[c.id] ?? []) {
    nodes.set(nameOf(r), { coll: c, r });
    const k = `${c.id}/${idOf(r)}`;
    byId.set(k, [...(byId.get(k) ?? []), nameOf(r)]);
  }
  const out = new Map<string, Edge[]>(), into = new Map<string, Edge[]>();
  // One edge per pair. A guest asks for a node in its spec and reports the
  // node it runs on in its status; when they agree that is one relation with
  // two names, not two relations — listed twice it read as two guests.
  const add = (e: Edge) => {
    if (!nodes.has(e.to)) return;
    const had = (out.get(e.from) ?? []).find((x) => x.to === e.to);
    if (had) { if (!had.label.includes(e.label)) had.label += ", " + e.label; return; }
    out.set(e.from, [...(out.get(e.from) ?? []), e]);
    into.set(e.to, [...(into.get(e.to) ?? []), e]);
  };
  for (const c of SCHEMA) {
    const refs = c.fields.filter(isRef);
    for (const r of census[c.id] ?? []) for (const f of refs) {
      const target = SCHEMA.find((x) => x.id === f.collection);
      if (!target) continue;
      for (const to of resolve(at(r.spec, f.key), target, byId)) add({ from: nameOf(r), to, label: f.label });
    }
    // What the status reports is an edge too: the node an instance actually
    // runs on is more interesting than the one it asked for.
    for (const r of census[c.id] ?? []) {
      const node = r.status?.node;
      if (typeof node === "string" && node) for (const to of resolve(node, SCHEMA.find((x) => x.id === "nodes")!, byId)) add({ from: nameOf(r), to, label: "runs on" });
    }
  }
  return { nodes, out, into };
}

/** Everything within `depth` hops of one object. */
export function neighbourhood(g: Graph, name: string, depth = 1): { names: Set<string>; edges: Edge[] } {
  const names = new Set([name]); let frontier = [name];
  for (let d = 0; d < depth; d++) {
    const next: string[] = [];
    for (const n of frontier) for (const e of [...(g.out.get(n) ?? []), ...(g.into.get(n) ?? [])]) {
      const other = e.from === n ? e.to : e.from;
      if (!names.has(other)) { names.add(other); next.push(other); }
    }
    frontier = next;
  }
  const edges = [...names].flatMap((n) => (g.out.get(n) ?? []).filter((e) => names.has(e.to)));
  return { names, edges };
}

/** The network's half of the graph: what the map draws. */
export const NETWORK_COLLECTIONS = ["networks", "subnets", "ports", "routers", "floatingips", "instances", "load-balancers", "security-groups"];
export function networkSlice(g: Graph): { names: Set<string>; edges: Edge[] } {
  const names = new Set([...g.nodes.keys()].filter((n) => NETWORK_COLLECTIONS.includes(g.nodes.get(n)!.coll.id)));
  const edges = [...names].flatMap((n) => (g.out.get(n) ?? []).filter((e) => names.has(e.to)));
  // An instance belongs on the map only when it is wired to something on it.
  for (const n of [...names]) {
    if (g.nodes.get(n)!.coll.id === "instances" && !edges.some((e) => e.from === n || e.to === n)) names.delete(n);
  }
  return { names, edges: edges.filter((e) => names.has(e.from) && names.has(e.to)) };
}
