// One object, beside the board rather than over it: the verdict and why,
// where what was asked for and what is disagree, every action the API has for
// it, the spec as a form, the status as it came, and whatever the registry
// adds. A pane you can keep open while you move through the rows.

import { useCallback, useEffect, useRef, useState } from "react";
import { ExternalLink, RefreshCw, Pencil, Trash2, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { toast } from "sonner";
import { call } from "@/api/transport";
import { collectionChanged } from "@/hooks/useCollection";
import { disagreements, humanise, idOf, nameOf, refusals, verdict, type Resource } from "@/lib/model";
import { pathOf, projectOf, type Collection } from "@/lib/schema";
import { entry, objectActions } from "@/registry";
import { remember, useStore } from "@/app/store";
import { useCan } from "@/lib/iam";
import { go } from "@/app/router";
import { fetchOne } from "@/hooks/useCollection";
import { Explain } from "./Explain";
import { Form as ResourceForm } from "./Form";
import { lazy, Suspense } from "react";
const Relations = lazy(() => import("./Relations").then((m) => ({ default: m.Relations })));
import { Timeline } from "./Timeline";
import { History } from "./History";
import { Pressed } from "./Pressed";
import { State } from "./State";
import { useAsk } from "@/features/Ask";

export function Detail({ coll, id, mode, onChanged }: {
  coll: Collection; id: string; mode?: "edit"; onChanged: () => void;
}) {
  const ask = useAsk();
  const project = useStore((s) => s.project);
  const who = useStore((s) => s.who);
  const can = useCan();
  const [r, setR] = useState<Resource | null>(null);
  const [err, setErr] = useState("");
  const [answers, setAnswers] = useState<Record<string, unknown>>({});

  const generation = useRef(0);
  const request = useRef(0);
  const load = useCallback(async () => {
    const current = generation.current;
    const sequence = ++request.current;
    try {
      const value = await fetchOne(coll, project, id);
      if (current === generation.current && sequence === request.current) { setR(value); setErr(""); }
    } catch (e) {
      if (current === generation.current && sequence === request.current) setErr((e as Error).message);
    }
  }, [coll, project, id]);
  useEffect(() => {
    generation.current++;
    setR(null); setErr(""); setAnswers({});
    void load(); remember(`${coll.id}/${id}`);
    const timer = setInterval(() => { if (!document.hidden) void load(); }, Math.max(3, coll.recheck || 5) * 1000);
    return () => { generation.current++; clearInterval(timer); };
  }, [coll.id, id, project, load]);

  const close = () => go({ view: "board", coll: coll.id });

  if (err && !r) return <Pane title={id} onClose={close}><p role="alert" className="text-sm text-destructive">{err}</p><Pressed onPress={load}>Retry</Pressed></Pane>;
  if (!r) return <Pane title={id} onClose={close}><p className="text-sm" style={{ color: "var(--text-muted)" }}>Reading…</p></Pane>;

  const v = verdict(r, coll);
  const diffs = disagreements(r, coll);
  const allActions = objectActions(coll.id, !!who?.cellAdmin);
  const diagnostics = allActions.filter((a) => /explain/i.test(a.id));
  const actions = allActions.filter((a) => !/explain/i.test(a.id));
  const custom = entry(coll.id);
  const here = projectOf(nameOf(r)) ?? project;
  const mayOperate = can("operate", coll, here); const mayWrite = can("write", coll, here);

  if (mode === "edit") {
    return (
      <Pane title={`Edit ${idOf(r)}`} sub={nameOf(r)} onClose={() => go({ view: "board", coll: coll.id, id })}>
        <ResourceForm coll={coll} existing={r}
          onDone={(saved) => {
            toast(`${idOf(saved)} saved.`);
            setR(saved); onChanged(); collectionChanged(coll.id);
            go({ view: "board", coll: coll.id, id });
          }}
          onCancel={() => go({ view: "board", coll: coll.id, id })} />
      </Pane>
    );
  }

  return (
    <Pane title={idOf(r)} sub={nameOf(r)} onClose={close}
      head={
        <div className="flex flex-wrap gap-2">
          <Pressed size="sm" variant="outline" onPress={load}><RefreshCw className="size-3.5" />Refresh</Pressed>
          {coll.editable && mayOperate && <Button size="sm" onClick={() => go({ view: "board", coll: coll.id, id, mode: "edit" })}><Pencil className="size-3.5" /> Edit</Button>}
          {mayOperate && custom.quick?.(r, coll, load)}
          {actions.length > 0 && <details className="relative text-xs"><summary className="rounded-md border border-border px-3 py-2 hover:bg-accent">More actions</summary><div className="mt-2 flex flex-wrap gap-2">{actions.map((a) => (
            <Pressed key={a.id} size="sm" title={a.summary} variant={a.destructive ? "destructive" : "secondary"} onPress={async () => {
              if (a.destructive && !(await ask({ title: `${a.label} ${idOf(r)}?`, confirmLabel: a.label, tone: "danger" }))) return;
              try {
                const answer = await call(a.id, a.method, a.path.replace("{project}", projectOf(nameOf(r)) ?? project).replace(/\{(name|id)\}/, encodeURIComponent(idOf(r))), undefined, a.needsBody ? {} : undefined);
                setAnswers((s) => ({ ...s, [a.id]: answer }));
                toast(a.label + " answered.");
                load();
              } catch (e) { toast.error((e as Error).message); }
            }}>{a.label}</Pressed>
          ))}</div></details>}
          {coll.deletable && mayWrite && (
            <Pressed size="sm" variant="destructive" onPress={async () => {
              if (!(await ask({ title: `Delete ${idOf(r)}?`, body: `It stays visible until its finalizers let go.`, confirmLabel: "Delete", tone: "danger" }))) return;
              try {
                // The revision this screen is showing, so a delete cannot
                // land on a version somebody else changed while the dialog was
                // open. Every PATCH in this console already says it; a DELETE
                // that did not was the one write that raced.
                await call(`delete:${coll.id}`, "DELETE", `${pathOf(coll, r, project)}/${encodeURIComponent(idOf(r))}`,
                  undefined, undefined,
                  r.meta.revision ? { "if-match": String(r.meta.revision) } : undefined);
                toast("Deletion asked for.", { description: "It stays listed until its finalizers let go." });
                onChanged(); collectionChanged(coll.id); close();
              } catch (e) { toast.error((e as Error).message); }
            }}><Trash2 className="size-3.5" /> Delete</Pressed>
          )}
        </div>
      }>
      {err && <p role="alert" className="text-sm text-destructive">Update failed. Displaying the last known state. {err}</p>}
      <Section label="Status">
        <div className="grid gap-2" style={{ borderLeft: `3px solid var(--${v.kind === "unreported" ? "border-strong" : "dot-" + v.kind})`, paddingLeft: 12 }}>
          <State of={r} coll={coll} detail />
          {v.detail && <p className="text-sm" style={{ color: "var(--text-body)" }}>{v.detail}</p>}
          {refusals(r).filter((c) => v.kind === "failing" && c.message !== v.detail).map((c) => (
            <p key={c.kind} className="break-all text-sm" style={{ color: "var(--text-body)" }}>
              <span className="font-mono text-xs" style={{ color: "var(--failing)" }}>{c.kind} · {c.reason}</span>{c.message ? ` — ${c.message}` : ""}
            </p>
          ))}
        </div>
        {coll.condition !== "" && <details className="text-xs text-muted-foreground"><summary>Deployment details</summary><Timeline r={r} coll={coll} /></details>}
      </Section>

      {diffs.length > 0 && (
        <Section label="Asked vs is" sub="the two halves, where they differ">
          <table className="w-full text-xs">
            <thead><tr style={{ color: "var(--text-faint)" }}><th className="text-left font-medium">Field</th><th className="text-left font-medium">Asked for</th><th className="text-left font-medium">Is</th></tr></thead>
            <tbody>
              {diffs.map((d) => (
                <tr key={d.label} className="border-t" style={{ borderColor: "var(--border-subtle)" }}>
                  <td className="py-1.5" style={{ color: "var(--text-body)" }}>{d.label}</td>
                  <td className="py-1.5 font-mono">{String(d.askedValue)}</td>
                  <td className="py-1.5 font-mono" style={{ color: "var(--drifting)" }}>{String(d.isValue)}</td>
                </tr>
              ))}
            </tbody>
          </table>
          <p className="mt-2 text-xs" style={{ color: "var(--text-muted)" }}>{diffs[0].note}</p>
        </Section>
      )}

      {custom.panels?.map((p) => (
        <Section key={p.id} label={p.title}>{p.render(r, coll, load)}</Section>
      ))}

      {diagnostics.length > 0 && <details className="rounded-lg border border-border p-3">
        <summary className="text-sm font-medium">Additional information</summary>
        <div className="mt-3 grid gap-3">
          <div className="flex flex-wrap gap-2">{diagnostics.map((a) => <Pressed key={a.id} size="sm" variant="secondary" title={a.summary} onPress={async () => {
            try {
              const answer = await call(a.id, a.method, a.path.replace("{project}", projectOf(nameOf(r)) ?? project).replace(/\{(name|id)\}/, encodeURIComponent(idOf(r))), undefined, a.needsBody ? {} : undefined);
              setAnswers((s) => ({ ...s, [a.id]: answer }));
            } catch (e) { toast.error((e as Error).message); }
          }}>{a.label.replace(/^Explain /, "Show ")}</Pressed>)}</div>
          {Object.entries(answers).filter(([k]) => diagnostics.some((a) => a.id === k)).map(([k, a]) => <div key={k} className="rounded-[4px] border p-3" style={{ borderColor: "var(--border-subtle)" }}><Explain answer={a} /></div>)}
        </div>
      </details>}
      <details className="rounded-lg border border-border p-3"><summary className="text-sm font-medium">Activity history</summary><div className="mt-3"><History r={r} /></div></details>
      <details className="rounded-lg border border-border p-3"><summary className="text-sm font-medium">Related resources</summary><div className="mt-3"><Suspense fallback={<p className="text-xs text-muted-foreground">Loading related resources…</p>}><Relations r={r} coll={coll} /></Suspense></div></details>

      <details className="rounded-lg border border-border p-3"><summary className="text-sm font-medium">Technical details</summary><Tabs defaultValue="spec" className="mt-3 flex w-full flex-col gap-1">
        <TabsList>
          <TabsTrigger value="spec">Specification</TabsTrigger>
          <TabsTrigger value="status">Status</TabsTrigger>
          <TabsTrigger value="meta">Metadata</TabsTrigger>
        </TabsList>
        <TabsContent value="spec"><KeyValues obj={r.spec ?? {}} refs={coll} /></TabsContent>
        <TabsContent value="status"><KeyValues obj={r.status ?? {}} /></TabsContent>
        <TabsContent value="meta"><KeyValues obj={r.meta as any} /></TabsContent>
      </Tabs></details>
    </Pane>
  );
}

function Pane({ title, sub, head, onClose, children }: {
  title: string; sub?: string; head?: React.ReactNode; onClose: () => void; children: React.ReactNode;
}) {
  return (
    <div className="arrive-right flex h-full min-h-0 flex-col">
      <div className="flex items-start gap-3 border-b px-5 py-4" style={{ borderColor: "var(--border)" }}>
        <div className="min-w-0 flex-1">
          <h2 className="truncate text-lg font-semibold" style={{ color: "var(--text-strong)" }}>{title}</h2>
          {sub && <p className="truncate font-mono text-[11px]" style={{ color: "var(--text-faint)" }}>{sub}</p>}
          {head && <div className="mt-3">{head}</div>}
        </div>
        <Button size="icon" variant="ghost" aria-label="Close" onClick={onClose}><X className="size-4" /></Button>
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto px-5 py-4">
        <div className="grid gap-6">{children}</div>
      </div>
    </div>
  );
}

function Section({ label, sub, children }: { label: string; sub?: string; children: React.ReactNode }) {
  return (
    <section className="grid gap-2">
      <div>
        <h3 className="text-[13px] font-semibold" style={{ color: "var(--text-strong)" }}>{label}</h3>
        {sub && <p className="text-[11px]" style={{ color: "var(--text-faint)" }}>{sub}</p>}
      </div>
      {children}
    </section>
  );
}

/** Keys and values, with a reference turned into a link to the thing. */
function KeyValues({ obj, refs }: { obj: Record<string, unknown>; refs?: Collection }) {
  const entries = Object.entries(obj).filter(([, v]) => v !== null && v !== undefined && v !== "" && !(Array.isArray(v) && !v.length));
  if (!entries.length) return <p className="py-3 text-xs" style={{ color: "var(--text-faint)" }}>Nothing set.</p>;
  return (
    <dl className="grid grid-cols-[minmax(110px,180px)_minmax(0,1fr)] gap-x-5 gap-y-2 py-3 text-xs">
      {entries.map(([k, v]) => {
        const field = refs?.fields.find((f) => f.key === k);
        const link = field?.kind === "ref" && field.collection && typeof v === "string"
          ? { coll: field.collection, id: String(v).split("/").pop()! } : null;
        return (
          <div key={k} className="contents">
            <dt style={{ color: "var(--text-muted)" }}>{field?.label ?? humanise(k)}</dt>
            <dd className="min-w-0 break-words font-mono" style={{ color: "var(--text-body)" }}>
              {link ? (
                <a href={`#/c/${link.coll}/${encodeURIComponent(link.id)}`} className="inline-flex items-center gap-1 underline-offset-2 hover:underline" style={{ color: "var(--brand)" }}>
                  {String(v)} <ExternalLink className="size-3" />
                </a>
              ) : typeof v === "object" ? <pre className="whitespace-pre-wrap">{JSON.stringify(v, null, 1)}</pre> : String(v)}
            </dd>
          </div>
        );
      })}
    </dl>
  );
}
