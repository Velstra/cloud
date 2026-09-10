// What this object depends on, and what depends on it — as chips you can
// follow, and as a small drawn neighbourhood when there is enough of it to be
// worth drawing. The blast-radius question ("if I delete this, what breaks?")
// is the "used by" half, and it is answered by the schema's own references.

import { useMemo } from "react";
import { ReactFlow, Background, type Edge as FlowEdge, type Node as FlowNode } from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import { idOf, nameOf, verdict, type Resource } from "@/lib/model";
import type { Collection } from "@/lib/schema";
import { buildGraph, neighbourhood, type Graph } from "@/lib/graph";
import { useCensusRows } from "@/app/census";
import { State } from "./State";

export function Relations({ r }: { r: Resource; coll: Collection }) {
  const rows = useCensusRows();
  const g = useMemo(() => buildGraph(rows), [rows]);
  const me = nameOf(r);
  const deps = g.out.get(me) ?? [];
  const users = g.into.get(me) ?? [];
  if (!deps.length && !users.length) {
    return <p className="text-xs" style={{ color: "var(--text-faint)" }}>Stands alone: nothing here refers to it, and it refers to nothing.</p>;
  }
  return (
    <div className="grid gap-4">
      <div className="grid grid-cols-2 gap-4 text-xs">
        <Strip title="Depends on" edges={deps} pick={(e) => e.to} g={g} empty="Nothing." />
        <Strip title="Used by" edges={users} pick={(e) => e.from} g={g} empty="Nothing — safe to remove on its own." />
      </div>
      {deps.length + users.length >= 2 && <Neighbourhood g={g} name={me} />}
    </div>
  );
}

function Strip({ title, edges, pick, g, empty }: {
  title: string; edges: { from: string; to: string; label: string }[]; pick: (e: { from: string; to: string }) => string; g: Graph; empty: string;
}) {
  return (
    <div>
      <div className="mb-1.5 font-semibold" style={{ color: "var(--text-strong)" }}>{title}</div>
      {!edges.length ? <span style={{ color: "var(--text-faint)" }}>{empty}</span> : (
        <ul className="flex flex-wrap gap-1.5">
          {edges.map((e, i) => {
            const n = g.nodes.get(pick(e))!;
            return (
              <li key={i}>
                <a href={`#/c/${n.coll.id}/${encodeURIComponent(idOf(n.r))}`} title={e.label}
                  className="inline-flex items-center gap-1.5 rounded-[3px] border px-2 py-0.5 hover:bg-[var(--surface-hover)]"
                  style={{ borderColor: "var(--border)", background: "var(--surface-sunken)" }}>
                  <span style={{ color: "var(--text-faint)" }}>{n.coll.singular}</span>
                  <span className="font-mono" style={{ color: "var(--text-body)" }}>{idOf(n.r)}</span>
                  <State of={n.r} coll={n.coll} />
                </a>
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}

/** The object and its immediate neighbours, laid out in rings. */
function Neighbourhood({ g, name }: { g: Graph; name: string }) {
  const { names, edges } = useMemo(() => neighbourhood(g, name, 1), [g, name]);
  const others = [...names].filter((n) => n !== name);
  const nodes: FlowNode[] = [
    flowNode(g, name, 0, 0, true),
    ...others.map((n, i) => {
      const a = (i / others.length) * Math.PI * 2 - Math.PI / 2;
      return flowNode(g, n, Math.cos(a) * 190, Math.sin(a) * 110, false);
    }),
  ];
  const flow: FlowEdge[] = edges.map((e, i) => ({
    id: String(i), source: e.from, target: e.to, label: e.label,
    style: { stroke: "var(--border-strong)" }, labelStyle: { fill: "var(--text-faint)", fontSize: 10 },
    labelBgStyle: { fill: "var(--surface)" },
  }));
  return (
    <div className="h-[260px] overflow-hidden rounded-[6px] border" style={{ borderColor: "var(--border)", background: "var(--surface-sunken)" }}>
      <ReactFlow nodes={nodes} edges={flow} fitView fitViewOptions={{ padding: 0.25 }} nodesDraggable={false} nodesConnectable={false}
        proOptions={{ hideAttribution: true }} onNodeClick={(_, n) => { const t = g.nodes.get(n.id); if (t) location.hash = `#/c/${t.coll.id}/${encodeURIComponent(idOf(t.r))}`; }}>
        <Background color="var(--border-subtle)" gap={18} />
      </ReactFlow>
    </div>
  );
}

export function flowNode(g: Graph, name: string, x: number, y: number, me: boolean): FlowNode {
  const n = g.nodes.get(name)!;
  const v = verdict(n.r, n.coll);
  const tone = v.kind === "unreported" ? "var(--border-strong)" : `var(--dot-${v.kind})`;
  return {
    id: name, position: { x, y }, data: { label: `${n.coll.singular} · ${idOf(n.r)}` },
    style: {
      fontSize: 11, fontFamily: "var(--font-mono)", padding: "4px 8px", borderRadius: 4,
      background: "var(--surface)", color: "var(--text-body)",
      border: `1px solid ${me ? "var(--brand)" : "var(--border-strong)"}`,
      boxShadow: `inset 3px 0 0 ${tone}`, width: "auto",
    },
  };
}
