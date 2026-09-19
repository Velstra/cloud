import { useEffect, useMemo, useState } from "react";
import { Activity, ArrowUpRight, CheckCircle2, Database, HardDrive, Layers3, Network, Plus, RefreshCw, Server, TriangleAlert } from "lucide-react";
import { call } from "@/api/transport";
import { useCensus } from "@/app/census";
import { useStore } from "@/app/store";
import { ago, attentionName, bytes, idOf, nameOf, verdict, VERDICT_ORDER, type Resource } from "@/lib/model";
import { ALL, SCHEMA, collection, routeId } from "@/lib/schema";
import { href } from "@/app/router";
import { useCan } from "@/lib/iam";
import { State } from "./State";
import { Pressed } from "./Pressed";
import { QuotaBars, useQuota } from "./Quota";

export function Overview({ onRefresh }: { onRefresh: () => Promise<void> }) {
  const { rows, missing, truncated, sweptAt } = useCensus();
  const who = useStore((s) => s.who);
  const project = useStore((s) => s.project);
  const can = useCan();
  const quota = useQuota(project);
  const [audit, setAudit] = useState<Resource[]>([]);
  const [auditError, setAuditError] = useState("");
  useEffect(() => {
    let current = true;
    setAudit([]); setAuditError("");
    call("list:audit", "GET", "/api/v1/audit", { pageSize: 6, orderBy: "createdAt desc" })
      .then((a) => { if (current) setAudit(a.items ?? []); })
      .catch(() => { if (current) setAuditError("Activity is unavailable. Refresh to retry."); });
    return () => { current = false; };
  }, [project, who?.subject, sweptAt]);
  const attention = useMemo(() => SCHEMA.flatMap((coll) => coll.condition === "" ? [] :
    (rows[coll.id] ?? []).filter((r) => verdict(r, coll).kind !== "settled").map((r) => ({ coll, r })))
    .sort((a, b) => VERDICT_ORDER[verdict(a.r, a.coll).kind] - VERDICT_ORDER[verdict(b.r, b.coll).kind]), [rows]);
  const incomplete = Object.keys(missing).length > 0 || truncated.length > 0;
  const link = (coll: string, r: Resource) => href({ view: "board", coll, id: routeId(collection(coll)!, r, project) });
  const nodes = rows.nodes ?? [];
  const machines = [...(rows.instances ?? [])].sort((a, b) => Number(b.meta.createdAt ?? 0) - Number(a.meta.createdAt ?? 0)).slice(0, 6);
  const tiles = who?.cellAdmin
    ? [{ id: "nodes", title: "Hosts", Icon: Server }, { id: "instances", title: "Instances", Icon: Layers3 }, { id: "networks", title: "Networks", Icon: Network }, { id: "ceph-clusters", title: "Ceph clusters", Icon: Database }]
    : [{ id: "instances", title: "Instances", Icon: Layers3 }, { id: "volumes", title: "Volumes", Icon: HardDrive }, { id: "networks", title: "Networks", Icon: Network }, { id: "snapshots", title: "Snapshots", Icon: Database }];
  return <div className="arrive-up mx-auto grid max-w-[1600px] gap-5 pb-6">
    <header className="flex flex-wrap items-center justify-between gap-3">
      <div><p className="mb-1 text-xs font-medium text-muted-foreground">{who?.cellAdmin ? "Infrastructure" : "Workspace"} / {project === ALL ? "All projects" : project}</p>
        <h1 className="text-[30px] font-semibold tracking-tight text-foreground">{who?.cellAdmin ? "Cloud overview" : "Your workspace"}</h1></div>
      <div className="flex items-center gap-2"><Pressed variant="outline" onPress={onRefresh}><RefreshCw className="size-4" />Refresh</Pressed>
        {can("write", "instances") && <a href="#/c/instances/new" className="inline-flex h-8 items-center gap-2 rounded-lg bg-primary px-3 text-sm font-medium text-primary-foreground hover:opacity-90"><Plus className="size-4" />Create instance</a>}</div>
    </header>
    <div className="grid grid-cols-2 gap-3 xl:grid-cols-4">{tiles.map(({ id, title, Icon }) => {
      const coll = collection(id); if (!coll) return null;
      const resources = rows[id] ?? [];
      const ready = resources.filter((r) => verdict(r, coll).kind === "settled").length;
      const known = sweptAt > 0 && !missing[id];
      return <a key={id} href={href({ view: "board", coll: id })} className="resource-tile overview-panel grid gap-4 p-4">
        <span className="flex items-center justify-between text-sm text-muted-foreground"><span className="inline-flex items-center gap-2"><Icon className="size-4 text-primary" />{title}</span><ArrowUpRight className="size-3.5" /></span>
        <span className="flex flex-wrap items-baseline justify-between gap-2"><strong className="text-3xl font-semibold tabular-nums tracking-tight">{known ? `${truncated.includes(id) ? "≥ " : ""}${resources.length}` : "—"}</strong><span className="text-xs text-muted-foreground">{known ? `${ready} ready` : sweptAt ? "Unavailable" : "Loading…"}</span></span>
      </a>;
    })}</div>
    <section className="overview-panel" aria-label="Health">
      <div className="flex flex-wrap items-center gap-3 px-5 py-4">
        {attention.length || incomplete ? <TriangleAlert className="size-5 text-[var(--drifting)]" /> : <CheckCircle2 className="size-5 text-[var(--settled)]" />}
        <div className="flex-1"><h2>{!sweptAt ? "Checking resources…" : incomplete ? "Some status data is unavailable" : attention.length ? `${attention.length} resources need attention` : "All resources are ready"}</h2><p className="mt-0.5 text-xs text-muted-foreground">{sweptAt ? `Last checked ${ago(sweptAt)}` : "Waiting for the first inventory read"}</p></div>
      </div>
      {incomplete && <p role="status" className="border-t border-border px-5 py-3 text-xs text-[var(--drifting)]">{Object.keys(missing).map((id) => `${collection(id)?.title ?? id}: unavailable`).concat(truncated.map((id) => `${collection(id)?.title ?? id}: partial inventory`)).join(" · ")}</p>}
      {attention.slice(0, 6).map(({ coll, r }) => <a key={nameOf(r)} href={link(coll.id, r)} className="grid grid-cols-[minmax(0,1fr)_auto] items-center gap-3 border-t border-border px-5 py-3 hover:bg-accent"><span className="min-w-0"><span className="block truncate text-sm font-medium">{attentionName(r, coll)}</span><span className="block truncate text-xs text-muted-foreground">{coll.singular} · {verdict(r, coll).detail || "Waiting for an update"}</span></span><State of={r} coll={coll} /></a>)}
      {attention.length > 6 && <p className="px-5 py-3 text-xs text-muted-foreground">{attention.length - 6} more in the attention inbox</p>}
    </section>
    <div className="grid items-start gap-5 xl:grid-cols-[minmax(0,1.6fr)_minmax(280px,1fr)]">
      <div className="grid min-w-0 gap-5">
        <section className="overview-panel"><div className="flex items-center justify-between border-b border-border px-5 py-4"><h2>Instances</h2><a className="text-xs text-primary hover:underline" href="#/c/instances">View all →</a></div>
          {!sweptAt || missing.instances ? <p className="p-5 text-sm text-muted-foreground">{missing.instances ? "Instances are unavailable. Refresh to retry." : "Loading instances…"}</p> : !machines.length ? <div className="p-5"><p className="text-sm text-muted-foreground">Your first instance starts here.</p>{can("write", "instances") && <a className="mt-2 inline-block text-sm text-primary hover:underline" href="#/c/instances/new">Create instance →</a>}</div> : machines.map((r) => <a key={nameOf(r)} href={link("instances", r)} className="grid grid-cols-[minmax(0,1fr)_auto] items-center gap-3 border-b border-border px-5 py-3 last:border-0 hover:bg-accent"><span className="min-w-0"><span className="block truncate text-sm font-medium">{idOf(r)}</span><span className="text-xs text-muted-foreground">{r.spec?.vcpus ?? "—"} vCPU · {bytes(Number(r.spec?.memoryMib ?? 0) * 1024 ** 2)}</span></span><State of={r} coll={collection("instances")} /></a>)}
        </section>
        {who?.cellAdmin && <section className="overview-panel"><div className="flex items-center justify-between border-b border-border px-5 py-4"><h2>Host capacity</h2><a href="#/c/nodes" className="text-xs text-primary hover:underline">Manage hosts →</a></div>
          <div className="grid gap-3 p-4 sm:grid-cols-2">{nodes.slice(0, 6).map((n) => {
            const cap = n.status?.capacity ?? {}; const used = n.status?.allocated ?? {};
            return <a key={nameOf(n)} href={link("nodes", n)} className="resource-tile rounded-lg border border-border p-3"><span className="mb-3 flex flex-wrap items-center justify-between gap-2"><span className="truncate text-xs font-medium">{idOf(n)}</span><State of={n} coll={collection("nodes")} /></span>
              {["vcpus", "memoryMib"].map((k) => {const value = Number(cap[k]) ? Math.min(100, Number(used[k] ?? 0) / Number(cap[k]) * 100) : 0; return <div key={k} className="mt-2"><div className="mb-1 flex justify-between text-[11px] text-muted-foreground"><span>{k === "vcpus" ? "CPU" : "Memory"}</span><span>{cap[k] ? `${Math.round(value)}%` : "Unknown"}</span></div><div role="meter" aria-label={k === "vcpus" ? "CPU allocation" : "Memory allocation"} aria-valuenow={value} aria-valuemin={0} aria-valuemax={100} className="h-1.5 overflow-hidden rounded-full bg-muted"><div className="h-full rounded-full bg-primary" style={{ width: `${value}%` }} /></div></div>;})}
            </a>;
          })}{!nodes.length && <p className="text-sm text-muted-foreground">{missing.nodes ? "Hosts are unavailable." : "No hosts reported."}</p>}</div>
        </section>}
        {project !== ALL && quota.q && <section className="overview-panel p-5"><h2 className="mb-4">Project limits</h2><QuotaBars q={quota.q} compact /></section>}
      </div>
      <section className="overview-panel"><div className="flex items-center gap-2 border-b border-border px-5 py-4"><Activity className="size-4 text-primary" /><h2>Recent activity</h2></div>
        {auditError ? <p role="status" className="p-5 text-sm text-muted-foreground">{auditError}</p> : !audit.length ? <p className="p-5 text-sm text-muted-foreground">No activity to show.</p> : <ol>{audit.map((r) => {const s = r.spec ?? {}; const verbs: Record<string, string> = {create: "Created", update: "Updated", delete: "Deleted"}; const action = s.kind === "changed" ? (verbs[String(s.verb)] ?? s.verb) : s.kind === "signed-in" ? "Signed in" : s.kind === "signed-out" ? "Signed out" : "Access refused"; return <li key={nameOf(r)} className="border-b border-border px-5 py-3 last:border-0"><p className="break-words text-sm">{action}{s.target ? ` ${String(s.target).split("/").pop()}` : ""}</p><p className="mt-1 text-xs text-muted-foreground"><span>{s.subject || "System"}</span><span title={new Date(Number(s.at ?? r.meta.createdAt)).toLocaleString()}> · {ago(s.at ?? r.meta.createdAt)}</span></p></li>;})}</ol>}
      </section>
    </div>
  </div>;
}
