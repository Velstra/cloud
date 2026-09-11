// The network, drawn: networks with their subnets, the ports on them, the
// routers and floating IPs that reach in, and the guests wired to it all.
// Laid out by layer so the picture reads top-down — the way traffic does —
// and every box is the object, one click away, wearing its verdict.

import { useMemo, useState } from "react";
import { ReactFlow, Background, Controls, MiniMap, type Edge as FlowEdge, type Node as FlowNode } from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import { idOf } from "@/lib/model";
import { buildGraph, networkSlice } from "@/lib/graph";
import { useCensus, whole, whyNotWhole } from "@/app/census";
import { flowNode } from "./Relations";

const LAYER: Record<string, number> = {
  routers: 0, "floatingips": 0, "load-balancers": 0,
  networks: 1, subnets: 2, ports: 3, "security-groups": 3, instances: 4,
};

export function Topology() {
  const census = useCensus();
  const [focus, setFocus] = useState<string | null>(null);
  const g = useMemo(() => buildGraph(census.rows), [census.rows]);
  const { names, edges } = useMemo(() => networkSlice(g), [g]);

  const nodes = useMemo<FlowNode[]>(() => {
    const byLayer = new Map<number, string[]>();
    for (const n of names) {
      const l = LAYER[g.nodes.get(n)!.coll.id] ?? 5;
      byLayer.set(l, [...(byLayer.get(l) ?? []), n]);
    }
    const out: FlowNode[] = [];
    for (const [l, list] of [...byLayer.entries()].sort((a, b) => a[0] - b[0])) {
      list.sort().forEach((n, i) => {
        const x = (i - (list.length - 1) / 2) * 200;
        const node = flowNode(g, n, x, l * 120, n === focus);
        out.push(node);
      });
    }
    return out;
  }, [names, g, focus]);

  const flow = useMemo<FlowEdge[]>(() => edges.map((e, i) => ({
    id: String(i), source: e.from, target: e.to, label: e.label === "runs on" ? "" : e.label,
    animated: e.label === "runs on",
    style: { stroke: focus && (e.from === focus || e.to === focus) ? "var(--brand)" : "var(--border-strong)" },
    labelStyle: { fill: "var(--text-faint)", fontSize: 10 }, labelBgStyle: { fill: "var(--bg-app)" },
  })), [edges, focus]);

  if (!names.size) {
    return <p className="p-8 text-sm" style={{ color: "var(--text-muted)" }}>Nothing on the map yet: no networks, subnets or ports have been read. Sweep again from the inbox once the cell has answered.</p>;
  }
  return (
    <div className="flex h-full flex-col">
      <header className="flex items-baseline gap-4 px-8 pt-6 pb-3">
        <h1 className="text-[26px] font-bold leading-tight" style={{ color: "var(--text-strong)" }}>Map</h1>
        <p className="text-sm" style={{ color: "var(--text-muted)" }}>
          {names.size} things, {edges.length} wires — drawn from the schema's references, top-down the way traffic goes. Click a box to focus it; double-click to open it.
        </p>
        {/* A map with a hole in it looks exactly like a map of a smaller cell.
            The relations panel refuses to claim completeness it does not have,
            and this is drawn from the same sweep, so it says the same thing. */}
        {!whole(census) && (
          <p className="mt-1 text-xs" style={{ color: "var(--drifting)" }}>
            Not the whole cell. {whyNotWhole(census)}
          </p>
        )}
      </header>
      <div className="min-h-0 flex-1 px-8 pb-6">
        <div className="h-full overflow-hidden rounded-[6px] border" style={{ borderColor: "var(--border)", background: "var(--surface-sunken)" }}>
          <ReactFlow nodes={nodes} edges={flow} fitView fitViewOptions={{ padding: 0.2 }} nodesConnectable={false}
            proOptions={{ hideAttribution: true }}
            onNodeClick={(_, n) => setFocus(n.id === focus ? null : n.id)}
            onNodeDoubleClick={(_, n) => { const t = g.nodes.get(n.id); if (t) location.hash = `#/c/${t.coll.id}/${encodeURIComponent(idOf(t.r))}`; }}>
            <Background color="var(--border-subtle)" gap={18} />
            <Controls showInteractive={false} />
            <MiniMap pannable zoomable style={{ background: "var(--surface)" }} maskColor="color-mix(in srgb, var(--bg-app) 70%, transparent)" nodeColor={() => "var(--border-strong)"} />
          </ReactFlow>
        </div>
      </div>
    </div>
  );
}
