// The landing: what needs attention across every collection, ranked by what it
// means; the cell's capacity as meters; and what happened last. Nothing here
// is a second list — every row is the object, one click away.

import { useEffect, useMemo, useState } from "react";
import { call } from "@/api/transport";
import { listEvery, projectNames } from "@/lib/listing";
import { ago, idOf, nameOf, verdict, VERDICT_ORDER, type Resource, type Verdict } from "@/lib/model";
import { ALL, SCHEMA, basePath, type Collection } from "@/lib/schema";
import { useStore } from "@/app/store";
import { QuotaBars, useQuota } from "./Quota";
import { State } from "./State";
import { Pressed } from "./Pressed";
import { lazy, Suspense } from "react";
const VerdictChart = lazy(() => import("./VerdictChart").then((m) => ({ default: m.VerdictChart })));

type Row = { coll: Collection; r: Resource; kind: Verdict };

export function Overview() {
  const project = useStore((s) => s.project);
  const who = useStore((s) => s.who);
  const [census, setCensus] = useState<Record<string, { rows: Resource[]; error?: string }>>({});
  const [kind, setKind] = useState<Verdict | null>(null);
  const quota = useQuota(project);

  const sweep = async () => {
    const targets = SCHEMA.filter((c) => c.condition !== "" && c.id !== "audit" && c.id !== "usage" && (who?.cellAdmin || c.scope === "project"));
    const out: typeof census = {};
    await Promise.all(targets.map(async (c) => {
      try {
        out[c.id] = { rows: (await listEvery(c, project)).rows };
      } catch (e) { out[c.id] = { rows: [], error: (e as Error).message }; }
    }));
    // The last few audit entries, asked for as a few — never the whole log.
    try {
      const audit = SCHEMA.find((c) => c.id === "audit")!;
      // Across every project the log is read for the first one only: eight
      // lines of one project beat a page per project nobody asked for.
      const a = await call("list:audit", "GET", basePath(audit, project === ALL && audit.scope === "project" ? (await projectNames())[0] ?? project : project), { pageSize: 8 });
      out.audit = { rows: (a.items ?? []).slice(0, 8) };
    } catch { /* the board says why */ }
    setCensus(out);
  };
  useEffect(() => { sweep(); }, [project, who?.cellAdmin]);

  const attention = useMemo<Row[]>(() => {
    const rows: Row[] = [];
    for (const c of SCHEMA) for (const r of census[c.id]?.rows ?? []) {
      const k = verdict(r, c).kind;
      if (k !== "settled") rows.push({ coll: c, r, kind: k });
    }
    return rows.sort((a, b) => VERDICT_ORDER[a.kind] - VERDICT_ORDER[b.kind] || nameOf(a.r).localeCompare(nameOf(b.r)));
  }, [census]);
  const counts = attention.reduce<Partial<Record<Verdict, number>>>((m, x) => { m[x.kind] = (m[x.kind] ?? 0) + 1; return m; }, {});
  const shown = kind ? attention.filter((x) => x.kind === kind) : attention;
  const unreadable = Object.entries(census).filter(([, v]) => v.error);
  const nodes = census.nodes?.rows ?? [];
  const audit = (census.audit?.rows ?? []).slice(0, 8);

  return (
    <div className="arrive-up grid gap-6">
      <header className="flex items-end gap-4">
        <div>
          <h1 className="text-[32px] font-bold leading-tight" style={{ color: "var(--text-strong)" }}>Overview</h1>
          <p className="text-sm" style={{ color: "var(--text-muted)" }}>
            {who?.cellAdmin ? "What needs attention, what the machines look like, and what happened last." : "What needs attention, your machines, and what happened last."}
          </p>
        </div>
        <div className="ml-auto"><Pressed size="sm" variant="secondary" onPress={sweep}>Refresh</Pressed></div>
      </header>

      {project !== ALL && quota.q && (
        <section className="rounded-[6px] border px-5 py-4" style={{ background: "var(--surface)", borderColor: "var(--border)" }}>
          <div className="mb-3 flex items-baseline gap-2">
            <h2 className="text-[11px] font-semibold uppercase tracking-[0.07em]" style={{ color: "var(--text-muted)" }}>Limits and use · {project}</h2>
            <a href={`#/c/projects/${encodeURIComponent(project)}`} className="ml-auto text-xs hover:underline" style={{ color: "var(--brand)" }}>All limits →</a>
          </div>
          <QuotaBars q={quota.q} compact />
        </section>
      )}

      <section className="rounded-[6px] border" style={{ background: "var(--surface)", borderColor: "var(--border)" }}>
        <div className="flex flex-wrap items-center gap-2 border-b px-5 py-3" style={{ borderColor: "var(--border-subtle)" }}>
          <h2 className="text-[11px] font-semibold uppercase tracking-[0.07em]" style={{ color: "var(--text-muted)" }}>Attention</h2>
          {attention.length > 0 && <>
            <span className="text-xs" style={{ color: "var(--text-faint)" }}>{attention.length} not settled —</span>
            {(Object.keys(counts) as Verdict[]).sort((a, b) => VERDICT_ORDER[a] - VERDICT_ORDER[b]).map((k) => (
              <button key={k} aria-pressed={kind === k} onClick={() => setKind(kind === k ? null : k)}
                className="inline-flex items-center gap-1.5 rounded-[3px] border px-2 py-0.5 text-xs font-medium"
                style={{ background: "var(--surface-sunken)", borderColor: kind === k ? "var(--focus-ring)" : "var(--border)" }}>
                <span className="size-[7px] rounded-full" style={{ background: k === "unreported" ? "var(--text-faint)" : `var(--dot-${k})` }} />
                {counts[k]} {k}
              </button>
            ))}
          </>}
        </div>
        {!attention.length && !unreadable.length ? (
          <p className="px-5 py-4 text-sm"><span style={{ color: "var(--settled)" }}>● Everything has settled.</span> <span style={{ color: "var(--text-muted)" }}>Nothing is drifting or failing.</span></p>
        ) : (
          <ul>
            {shown.map(({ coll, r, kind: k }) => {
              const v = verdict(r, coll);
              return (
                <li key={nameOf(r)}>
                  <a href={`#/c/${coll.id}/${encodeURIComponent(idOf(r))}`}
                    className="grid grid-cols-[minmax(0,1fr)_150px_minmax(0,2fr)] items-center gap-4 border-b px-5 py-2.5 text-[13px] hover:bg-[var(--surface-hover)] focus-visible:bg-[var(--surface-hover)] focus-visible:outline-none"
                    style={{ borderColor: "var(--border-subtle)", boxShadow: `inset 3px 0 0 var(--${k === "unreported" ? "border-strong" : "dot-" + k})` }}>
                    <span className="truncate font-mono" style={{ color: "var(--text-strong)" }}>{coll.id}/{idOf(r)}</span>
                    <State of={r} coll={coll} />
                    <span className="truncate text-xs" style={{ color: "var(--text-muted)" }}>{coll.singular}{v.detail ? ` — ${v.detail}` : ""}</span>
                  </a>
                </li>
              );
            })}
            {unreadable.map(([id, v]) => (
              <li key={id} className="grid grid-cols-[minmax(0,1fr)_150px_minmax(0,2fr)] gap-4 border-b px-5 py-2.5 text-[13px]" style={{ borderColor: "var(--border-subtle)", boxShadow: "inset 3px 0 0 var(--dot-failing)" }}>
                <span className="font-mono">{id}</span><span style={{ color: "var(--failing)" }}>● unreadable</span>
                <span className="truncate text-xs" style={{ color: "var(--text-muted)" }}>{v.error}</span>
              </li>
            ))}
          </ul>
        )}
      </section>

      <section className="rounded-[6px] border" style={{ background: "var(--surface)", borderColor: "var(--border)" }}>
        <h2 className="border-b px-5 py-3 text-[11px] font-semibold uppercase tracking-[0.07em]" style={{ color: "var(--text-muted)", borderColor: "var(--border-subtle)" }}>Where things stand, by collection</h2>
        <Suspense fallback={<div className="h-[220px]" />}><VerdictChart census={census} /></Suspense>
      </section>

      {who?.cellAdmin && nodes.length > 0 && (
        <section className="rounded-[6px] border" style={{ background: "var(--surface)", borderColor: "var(--border)" }}>
          <h2 className="border-b px-5 py-3 text-[11px] font-semibold uppercase tracking-[0.07em]" style={{ color: "var(--text-muted)", borderColor: "var(--border-subtle)" }}>The cell</h2>
          <div className="grid gap-3 p-5 sm:grid-cols-2 lg:grid-cols-4">
            {nodes.map((n) => {
              const cap = (n.status as any)?.capacity ?? {}; const used = (n.status as any)?.allocated ?? {};
              const pct = (k: string) => cap[k] ? Math.min(100, Math.round(100 * (Number(used[k] ?? 0) / Number(cap[k])))) : 0;
              return (
                <a key={nameOf(n)} href={`#/c/nodes/${encodeURIComponent(idOf(n))}`} className="grid gap-2 rounded-[4px] border p-3 hover:bg-[var(--surface-hover)]" style={{ borderColor: "var(--border-subtle)" }}>
                  <div className="flex items-center justify-between"><span className="font-mono text-sm" style={{ color: "var(--text-strong)" }}>{idOf(n)}</span><State of={n} coll={SCHEMA.find((c) => c.id === "nodes")} /></div>
                  {["vcpus", "memoryMib"].map((k) => (
                    <div key={k} className="grid gap-1 text-[11px]" style={{ color: "var(--text-muted)" }}>
                      <div className="flex justify-between"><span>{k === "vcpus" ? "vCPU" : "Memory"}</span><span className="font-mono">{pct(k)}%</span></div>
                      <div className="h-1.5 rounded-full" style={{ background: "var(--surface-sunken)" }}>
                        <div className="h-1.5 rounded-full" style={{ width: pct(k) + "%", background: pct(k) > 90 ? "var(--dot-failing)" : pct(k) > 75 ? "var(--dot-drifting)" : "var(--text-faint)" }} />
                      </div>
                    </div>
                  ))}
                </a>
              );
            })}
          </div>
        </section>
      )}

      {audit.length > 0 && (
        <section className="rounded-[6px] border" style={{ background: "var(--surface)", borderColor: "var(--border)" }}>
          <h2 className="border-b px-5 py-3 text-[11px] font-semibold uppercase tracking-[0.07em]" style={{ color: "var(--text-muted)", borderColor: "var(--border-subtle)" }}>What happened last</h2>
          <ul>
            {audit.map((a) => (
              <li key={nameOf(a)} className="grid grid-cols-[110px_minmax(0,1fr)_120px] gap-4 border-b px-5 py-2 text-xs" style={{ borderColor: "var(--border-subtle)" }}>
                <span className="font-mono" style={{ color: "var(--text-faint)" }}>{ago((a.spec as any)?.at ?? a.meta.createdAt)}</span>
                <span className="truncate" style={{ color: "var(--text-body)" }}>{String((a.spec as any)?.summary ?? (a.spec as any)?.action ?? idOf(a))}</span>
                <span className="truncate font-mono" style={{ color: "var(--text-muted)" }}>{String((a.spec as any)?.who ?? "")}</span>
              </li>
            ))}
          </ul>
        </section>
      )}
    </div>
  );
}

