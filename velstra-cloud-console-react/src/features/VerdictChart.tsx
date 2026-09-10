// The cell at a glance: one bar per collection, split by verdict.

import { Bar, BarChart, CartesianGrid, Legend, ResponsiveContainer, Tooltip as ChartTip, XAxis, YAxis } from "recharts";
import { verdict, type Resource } from "@/lib/model";
import { SCHEMA } from "@/lib/schema";

/** One bar per collection that has anything in it, split by verdict — so a
 *  cell with forty networks drifting and one guest failing reads as what it
 *  is: one problem and one incident, not forty-one rows. */
export function VerdictChart({ census }: { census: Record<string, { rows: Resource[]; error?: string }> }) {
  const data = SCHEMA.filter((c) => c.condition !== "" && (census[c.id]?.rows.length ?? 0) > 0).map((c) => {
    const n = { name: c.title, settled: 0, drifting: 0, failing: 0, unreported: 0, deleting: 0 } as Record<string, number | string>;
    for (const r of census[c.id].rows) (n[verdict(r, c).kind] as number)++;
    return n;
  });
  if (!data.length) return <p className="px-5 py-4 text-xs" style={{ color: "var(--text-faint)" }}>Nothing read yet.</p>;
  const tone = (k: string) => k === "failing" ? "var(--dot-failing)" : k === "drifting" ? "var(--dot-drifting)" : k === "settled" ? "var(--dot-settled)" : "var(--border-strong)";
  return (
    <div className="h-[220px] px-3 py-3">
      <ResponsiveContainer width="100%" height="100%">
        <BarChart data={data} layout="vertical" barCategoryGap={4} margin={{ left: 8, right: 16, top: 4, bottom: 4 }}>
          <CartesianGrid horizontal={false} stroke="var(--border-subtle)" />
          <XAxis type="number" tick={{ fill: "var(--text-faint)", fontSize: 11 }} stroke="var(--border)" allowDecimals={false} />
          <YAxis type="category" dataKey="name" width={120} tick={{ fill: "var(--text-muted)", fontSize: 11 }} stroke="var(--border)" />
          <ChartTip cursor={{ fill: "var(--surface-hover)" }} contentStyle={{ background: "var(--surface-raised)", border: "1px solid var(--border-strong)", borderRadius: 4, fontSize: 12, color: "var(--text-body)" }} />
          <Legend wrapperStyle={{ fontSize: 11, color: "var(--text-muted)" }} />
          {["failing", "drifting", "unreported", "settled"].map((k) => <Bar key={k} dataKey={k} stackId="v" fill={tone(k)} radius={0} />)}
        </BarChart>
      </ResponsiveContainer>
    </div>
  );
}
