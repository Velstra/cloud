// One object, beside the board rather than over it: the verdict and why,
// where what was asked for and what is disagree, every action the API has for
// it, the spec as a form, the status as it came, and whatever the registry
// adds. A pane you can keep open while you move through the rows.

import { useEffect, useState } from "react";
import { ExternalLink, Pencil, Trash2, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { toast } from "sonner";
import { call } from "@/api/transport";
import { disagreements, humanise, idOf, nameOf, refusals, verdict, type Resource } from "@/lib/model";
import { pathOf, projectOf, type Collection } from "@/lib/schema";
import { entry, objectActions } from "@/registry";
import { remember, useStore } from "@/app/store";
import { useCan } from "@/lib/iam";
import { go } from "@/app/router";
import { fetchOne } from "@/hooks/useCollection";
import { Explain } from "./Explain";
import { Form } from "./Form";
import { lazy, Suspense } from "react";
const Relations = lazy(() => import("./Relations").then((m) => ({ default: m.Relations })));
import { Timeline } from "./Timeline";
import { History } from "./History";
import { Pressed } from "./Pressed";
import { State } from "./State";

export function Detail({ coll, id, mode, onChanged }: {
  coll: Collection; id: string; mode?: "edit"; onChanged: () => void;
}) {
  const project = useStore((s) => s.project);
  const can = useCan();
  const [r, setR] = useState<Resource | null>(null);
  const [err, setErr] = useState("");
  const [answers, setAnswers] = useState<Record<string, unknown>>({});

  const load = () => fetchOne(coll, project, id).then((x) => { setR(x); setErr(""); }).catch((e) => setErr(e.message));
  useEffect(() => { setR(null); load(); remember(`${coll.id}/${id}`); }, [coll.id, id, project]);

  // A drifting object is worth asking about again: the agent is on it.
  useEffect(() => {
    if (!r || !verdict(r, coll).busy) return;
    const t = setInterval(load, Math.max(3, coll.recheck || 5) * 1000);
    return () => clearInterval(t);
  }, [r, coll]);

  const close = () => go({ view: "board", coll: coll.id });

  if (err) return <Pane title={id} onClose={close}><p className="text-sm" style={{ color: "var(--failing)" }}>{err}</p></Pane>;
  if (!r) return <Pane title={id} onClose={close}><p className="text-sm" style={{ color: "var(--text-muted)" }}>Reading…</p></Pane>;

  const v = verdict(r, coll);
  const diffs = disagreements(r, coll);
  const actions = objectActions(coll.id);
  const custom = entry(coll.id);
  const here = projectOf(nameOf(r)) ?? project;
  const mayOperate = can("operate", coll, here); const mayWrite = can("write", coll, here);

  if (mode === "edit") {
    return (
      <Pane title={`Edit ${idOf(r)}`} sub={nameOf(r)} onClose={() => go({ view: "board", coll: coll.id, id })}>
        <Form coll={coll} existing={r}
          onDone={(saved) => { toast(`${idOf(saved)} saved.`); setR(saved); onChanged(); go({ view: "board", coll: coll.id, id }); }}
          onCancel={() => go({ view: "board", coll: coll.id, id })} />
      </Pane>
    );
  }

  return (
    <Pane title={idOf(r)} sub={nameOf(r)} onClose={close}
      head={
        <div className="flex flex-wrap gap-2">
          {coll.editable && mayOperate && <Button size="sm" onClick={() => go({ view: "board", coll: coll.id, id, mode: "edit" })}><Pencil className="size-3.5" /> Edit</Button>}
          {mayOperate && custom.quick?.(r, coll, load)}
          {actions.map((a) => (
            <Pressed key={a.id} size="sm" title={a.summary} variant={a.destructive ? "destructive" : "secondary"} onPress={async () => {
              if (a.destructive && !confirm(`${a.label} ${idOf(r)}?`)) return;
              try {
                const answer = await call(a.id, a.method, a.path.replace("{project}", projectOf(nameOf(r)) ?? project).replace(/\{(name|id)\}/, encodeURIComponent(idOf(r))), undefined, a.needsBody ? {} : undefined);
                setAnswers((s) => ({ ...s, [a.id]: answer }));
                toast(a.label + " answered.");
                load();
              } catch (e) { toast.error((e as Error).message); }
            }}>{a.label}</Pressed>
          ))}
          {coll.deletable && mayWrite && (
            <Pressed size="sm" variant="destructive" onPress={async () => {
              if (!confirm(`Delete ${idOf(r)}? It stays visible until its finalizers let go.`)) return;
              try {
                await call(`delete:${coll.id}`, "DELETE", `${pathOf(coll, r, project)}/${encodeURIComponent(idOf(r))}`);
                toast("Deletion asked for."); onChanged(); close();
              } catch (e) { toast.error((e as Error).message); }
            }}><Trash2 className="size-3.5" /> Delete</Pressed>
          )}
        </div>
      }>
      <Section label="Convergence" sub="what was asked for, and what is">
        <div className="grid gap-2" style={{ borderLeft: `3px solid var(--${v.kind === "unreported" ? "border-strong" : "dot-" + v.kind})`, paddingLeft: 12 }}>
          <State of={r} coll={coll} detail />
          {v.detail && <p className="text-sm" style={{ color: "var(--text-body)" }}>{v.detail}</p>}
          {refusals(r).filter((c) => c.message !== v.detail).map((c) => (
            <p key={c.kind} className="break-all text-sm" style={{ color: "var(--text-body)" }}>
              <span className="font-mono text-xs" style={{ color: "var(--failing)" }}>{c.kind} · {c.reason}</span>{c.message ? ` — ${c.message}` : ""}
            </p>
          ))}
        </div>
        {coll.condition !== "" && <Timeline r={r} coll={coll} />}
      </Section>

      <Section label="History" sub="what was asked, by whom — and what was refused, in the words they were given">
        <History r={r} />
      </Section>

      <Section label="Relations" sub="what it depends on, and what would notice if it went">
        <Suspense fallback={<p className="text-xs" style={{ color: "var(--text-faint)" }}>Reading the neighbourhood…</p>}><Relations r={r} coll={coll} /></Suspense>
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

      {Object.keys(answers).length > 0 && (
        <Section label="Answers" sub="what the last actions said">
          {Object.entries(answers).map(([k, a]) => (
            <div key={k} className="mb-3 rounded-[4px] border p-3" style={{ borderColor: "var(--border-subtle)" }}>
              <div className="mb-2 text-[11px] font-semibold uppercase tracking-[0.07em]" style={{ color: "var(--text-faint)" }}>{actions.find((x) => x.id === k)?.label ?? k}</div>
              <Explain answer={a} />
            </div>
          ))}
        </Section>
      )}

      {custom.panels?.map((p) => (
        <Section key={p.id} label={p.title} sub={p.sub}>{p.render(r, coll, load)}</Section>
      ))}

      <Tabs defaultValue="spec" className="flex w-full flex-col gap-1">
        <TabsList>
          <TabsTrigger value="spec">Specification</TabsTrigger>
          <TabsTrigger value="status">Status</TabsTrigger>
          <TabsTrigger value="meta">Metadata</TabsTrigger>
        </TabsList>
        <TabsContent value="spec"><KeyValues obj={r.spec ?? {}} refs={coll} /></TabsContent>
        <TabsContent value="status"><KeyValues obj={r.status ?? {}} /></TabsContent>
        <TabsContent value="meta"><KeyValues obj={r.meta as any} /></TabsContent>
      </Tabs>
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
