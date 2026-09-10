// The Ceph cluster as an operator reads it — the way `ceph status` reads it,
// which is what anybody who ran Ceph before opens a terminal for. Health and
// its warnings first, then how full and how busy, then every OSD with its
// fill and both of its states, then every pool with what it holds. Beside each
// of those, what the platform *asked* for, so a disk that is up but no longer
// wanted, or a pool replicated once where three was asked, reads as the
// disagreement it is. Two things are done here: taking a disk out and putting
// it back.

import { useState } from "react";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import { call } from "@/api/transport";
import { ago, bytes, idOf, type Resource } from "@/lib/model";
import { basePath, type Collection } from "@/lib/schema";
import { useStore } from "@/app/store";
import { Pressed } from "./Pressed";

type Osd = { node: string; device: string; evenIfUnsuitable?: boolean };
type Seen = { id: number; host?: string; device?: string; up?: boolean; in?: boolean; usedBytes?: number; totalBytes?: number; pgs?: number; class?: string };
type Pool = { pool: string; size?: number; minSize?: number };
type PoolSeen = { pool: string; storedBytes?: number; objects?: number; maxAvailBytes?: number; size?: number; minSize?: number; pgNum?: number };
type Warning = { code: string; severity?: string; message?: string };
type Pg = { state: string; count: number };

const same = (a: { node?: string; host?: string; device?: string }, b: { node?: string; host?: string; device?: string }) =>
  (a.node ?? a.host) === (b.node ?? b.host) && a.device === b.device;

const HEALTH: Record<string, { word: string; colour: string }> = {
  HEALTH_OK: { word: "Healthy", colour: "var(--settled)" },
  HEALTH_WARN: { word: "Warning", colour: "var(--drifting)" },
  HEALTH_ERR: { word: "Error", colour: "var(--failing)" },
};

const pct = (used?: number, total?: number) => (total ? Math.round(((used ?? 0) / total) * 1000) / 10 : null);

function Fill({ used, total }: { used?: number; total?: number }) {
  const p = pct(used, total);
  if (p == null) return <span style={{ color: "var(--text-faint)" }}>—</span>;
  const colour = p >= 85 ? "var(--failing)" : p >= 70 ? "var(--drifting)" : "var(--brand)";
  return (
    <span className="inline-flex items-center gap-2">
      <span className="inline-block h-1.5 w-12 shrink-0 overflow-hidden rounded-full" style={{ background: "var(--border-strong)" }}>
        <span className="block h-full rounded-full" style={{ width: `${Math.max(1, Math.min(100, p))}%`, background: colour }} />
      </span>
      <span className="whitespace-nowrap font-mono tabular-nums">{bytes(used)} <span style={{ color: "var(--text-faint)" }}>/ {bytes(total)} · {p}%</span></span>
    </span>
  );
}

const Heading = ({ children }: { children: React.ReactNode }) => (
  <div className="mb-1 text-[11px] font-semibold uppercase tracking-[0.07em]" style={{ color: "var(--text-muted)" }}>{children}</div>
);
const Th = ({ children, right }: { children?: React.ReactNode; right?: boolean }) => (
  <th className={`pb-1 font-medium ${right ? "text-right" : "text-left"}`}>{children}</th>
);

export function Ceph({ r, coll, reload }: { r: Resource; coll: Collection; reload: () => void }) {
  const project = useStore((s) => s.project);
  const spec = r.spec ?? {}; const st = r.status ?? {};
  const asked: Osd[] = spec.osds ?? []; const up: Osd[] = st.osdsUp ?? []; const seen: Seen[] = st.osds ?? [];
  const mons: string[] = spec.monitors ?? []; const monsUp: string[] = st.monitorsUp ?? [];
  const mgrsUp: string[] = st.managersUp ?? [];
  const pools: Pool[] = spec.pools ?? []; const present: string[] = st.poolsPresent ?? []; const poolSeen: PoolSeen[] = st.poolStats ?? [];
  const warnings: Warning[] = st.warnings ?? []; const pgs: Pg[] = st.pgs ?? [];
  const health = HEALTH[st.health as string];
  const [taken, setTaken] = useState<Osd[]>([]);

  const patch = async (next: Partial<typeof spec>, said: string) => {
    try {
      await call("patch:ceph-clusters", "PATCH", `${basePath(coll, project)}/${encodeURIComponent(idOf(r))}`,
        undefined, { spec: next }, r.meta.revision ? { "if-match": String(r.meta.revision) } : undefined);
      toast(said); reload();
    } catch (e) { toast.error((e as Error).message); }
  };

  const clean = pgs.find((p) => p.state === "active+clean")?.count ?? 0;
  const unclean = pgs.filter((p) => p.state !== "active+clean");
  const stale = st.at && Date.now() - st.at > 5 * 60_000;

  // Every disk that is asked for, seen, or up — one row each, keyed by node
  // and device, in the order the spec lists them and then the rest.
  const rows = [...asked.map((o) => ({ node: o.node, device: o.device })),
    ...[...seen.map((s) => ({ node: s.host ?? "", device: s.device ?? "" })), ...up, ...taken].filter((o, i, all) =>
      !asked.some((a) => same(a, o)) && all.findIndex((x) => same(x, o)) === i)];

  // One column of exactly the pane's width — `minmax(0,1fr)` rather than the
  // implicit `auto`, which would size the track to the OSD table's longest
  // row and push everything, strip included, past the pane's edge. The
  // tables scroll inside their own wrappers instead.
  return (
    <div className="grid grid-cols-[minmax(0,1fr)] gap-5">
      {/* What Ceph says, in one strip. */}
      <div className="grid gap-3 rounded-[4px] border p-3" style={{ borderColor: "var(--border-subtle)", background: "var(--surface-sunken)" }}>
        <div className="flex flex-wrap items-center gap-x-5 gap-y-1 text-xs">
          <span className="inline-flex items-center gap-2 text-sm font-medium" style={{ color: health?.colour ?? "var(--text-faint)" }}>
            <span className="size-2.5 rounded-full" style={{ background: health?.colour ?? "var(--border-strong)" }} />
            {health?.word ?? "Not read yet"}
            {st.health && <span className="font-mono text-[11px] font-normal" style={{ color: "var(--text-faint)" }}>{st.health}</span>}
          </span>
          {st.totalBytes ? <span><Fill used={st.usedBytes} total={st.totalBytes} /></span> : null}
          {st.pgsTotal ? (
            <span style={{ color: unclean.length ? "var(--drifting)" : "var(--text-body)" }}>
              {clean}/{st.pgsTotal} PGs clean{unclean.length ? ` · ${unclean.map((p) => `${p.count} ${p.state}`).join(", ")}` : ""}
            </span>
          ) : null}
          {st.objects ? <span style={{ color: "var(--text-muted)" }}>{Number(st.objects).toLocaleString()} objects</span> : null}
          {st.at ? (
            <span className="font-mono tabular-nums" style={{ color: "var(--text-muted)" }}>
              ↓ {bytes(st.readBps ?? 0)}/s · {st.readOps ?? 0} op/s &nbsp; ↑ {bytes(st.writeBps ?? 0)}/s · {st.writeOps ?? 0} op/s
            </span>
          ) : null}
          <span className="ml-auto text-[11px]" style={{ color: stale ? "var(--drifting)" : "var(--text-faint)" }} title={st.at ? new Date(st.at).toLocaleString() : undefined}>
            {st.at ? `read on ${st.seenBy ?? "?"} ${ago(st.at)}${stale ? " — stale" : ""}` : "no node with the admin keyring has reported yet"}
          </span>
        </div>
        {warnings.length > 0 && (
          <ul className="grid gap-1 text-xs">
            {warnings.map((w) => (
              <li key={w.code} className="flex gap-2">
                <span className="font-mono text-[11px]" style={{ color: w.severity === "HEALTH_ERR" ? "var(--failing)" : "var(--drifting)" }}>{w.code}</span>
                <span style={{ color: "var(--text-body)" }}>{w.message}</span>
              </li>
            ))}
          </ul>
        )}
      </div>

      <div className="grid grid-cols-2 gap-5">
        <div>
          <Heading>Monitors · {monsUp.length}/{mons.length} up</Heading>
          {mons.map((m) => <Dot key={m} ok={monsUp.includes(m)} label={m} sub={monsUp.includes(m) ? "in quorum" : "asked for, not up"} />)}
          {!mons.length && <p className="text-xs" style={{ color: "var(--text-faint)" }}>None asked for.</p>}
        </div>
        <div>
          <Heading>Managers · {mgrsUp.length} up</Heading>
          {mgrsUp.map((m) => <Dot key={m} ok label={m} />)}
          {!mgrsUp.length && <p className="text-xs" style={{ color: "var(--text-faint)" }}>None reported.</p>}
        </div>
      </div>

      <div>
        <Heading>OSDs · {seen.length ? `${seen.filter((s) => s.up).length}/${seen.length} up · ${seen.filter((s) => s.in).length} in` : `${up.length}/${asked.length} up`}</Heading>
        <div className="min-w-0 overflow-x-auto">
          <table className="w-full text-xs">
            <thead><tr style={{ color: "var(--text-faint)" }}><Th>Node</Th><Th>Disk</Th><Th>State</Th><Th>Used</Th><Th right>PGs</Th><Th /></tr></thead>
            <tbody>
              {rows.map((o) => {
                const isAsked = asked.some((a) => same(a, o));
                const isUp = up.some((u) => same(u, o));
                const s = seen.find((x) => same(x, o));
                const spec = asked.find((a) => same(a, o));
                const gone = !isAsked && !isUp && !s;
                const state = s
                  ? [s.up ? "up" : "down", s.in ? "in" : "out"].join(" · ")
                  : isUp ? "up" : gone ? "gone" : "asked for, not up";
                const colour = gone ? "var(--text-faint)" : s ? (s.up && s.in ? "var(--settled)" : "var(--drifting)") : isUp ? "var(--settled)" : "var(--drifting)";
                return (
                  <tr key={o.node + o.device} className="border-t" style={{ borderColor: "var(--border-subtle)", color: gone ? "var(--text-faint)" : undefined }}>
                    <td className="py-1.5 font-mono"><a href={`#/c/nodes/${encodeURIComponent(o.node)}`} style={{ color: "var(--brand)" }}>{o.node}</a></td>
                    <td className="py-1.5 font-mono whitespace-nowrap">
                      {o.device}
                      {s ? <span style={{ color: "var(--text-faint)" }}> · osd.{s.id}</span> : null}
                      {spec?.evenIfUnsuitable ? <span title="Taken although the platform called it unsuitable" style={{ color: "var(--text-faint)" }}> · forced</span> : null}
                    </td>
                    <td className="py-1.5 whitespace-nowrap" style={{ color: colour }}>
                      {state}{s?.class ? <span style={{ color: "var(--text-faint)" }}> · {s.class}</span> : null}
                      {!isAsked && !gone ? <span style={{ color: "var(--drifting)" }}> — no longer asked for, draining</span> : null}
                    </td>
                    <td className="py-1.5">{s ? <Fill used={s.usedBytes} total={s.totalBytes} /> : <span style={{ color: "var(--text-faint)" }}>—</span>}</td>
                    <td className="py-1.5 text-right font-mono tabular-nums">{s?.pgs ?? "—"}</td>
                    <td className="py-1.5 text-right">
                      {isAsked ? (
                        <Pressed size="sm" variant="secondary" title="Remove this disk from the cluster's spec; Ceph drains and forgets it"
                          onPress={async () => {
                            if (!confirm(`Take ${o.node}:${o.device} out of the cluster? Its data is re-placed on the others first; with size 1 pools that is data lost.`)) return;
                            setTaken((t) => [...t, o]);
                            await patch({ osds: asked.filter((x) => !same(x, o)) }, `${o.device} on ${o.node} is being taken out.`);
                          }}>Take out</Pressed>
                      ) : gone ? (
                        <Button size="sm" variant="ghost" onClick={() => patch({ osds: [...asked, o] }, `${o.device} on ${o.node} is being added again.`)}>Add again</Button>
                      ) : (
                        <Pressed size="sm" variant="secondary" onPress={() => patch({ osds: [...asked, o] }, `${o.device} on ${o.node} is being put back.`)}>Put back</Pressed>
                      )}
                    </td>
                  </tr>
                );
              })}
              {!rows.length && <tr><td colSpan={6} className="py-2 text-xs" style={{ color: "var(--text-faint)" }}>No disks yet — edit the cluster and pick some.</td></tr>}
            </tbody>
          </table>
        </div>
      </div>

      <div>
        <Heading>Pools · {present.length}/{pools.length} present</Heading>
        <div className="min-w-0 overflow-x-auto">
          <table className="w-full text-xs">
            <thead><tr style={{ color: "var(--text-faint)" }}><Th>Pool</Th><Th>Replicas</Th><Th right>PGs</Th><Th right>Stored</Th><Th right>Objects</Th><Th right>Room left</Th></tr></thead>
            <tbody>
              {[...pools.map((p) => p.pool), ...poolSeen.map((p) => p.pool).filter((n) => !pools.some((p) => p.pool === n))].map((name) => {
                const p = pools.find((x) => x.pool === name); const s = poolSeen.find((x) => x.pool === name);
                const is = present.includes(name) || !!s;
                const sizeDiffers = p?.size != null && s?.size != null && p.size !== s.size;
                return (
                  <tr key={name} className="border-t" style={{ borderColor: "var(--border-subtle)" }}>
                    <td className="py-1.5 font-mono">
                      <span className="mr-2 inline-block size-2 rounded-full align-middle" style={{ background: is ? "var(--dot-settled)" : "var(--dot-failing)" }} />
                      {name}{!p && <span style={{ color: "var(--text-faint)" }}> · not in the spec</span>}{p && !is && <span style={{ color: "var(--text-faint)" }}> · asked for, not present</span>}
                    </td>
                    <td className="py-1.5" style={{ color: (s?.size ?? p?.size ?? 1) <= 1 ? "var(--drifting)" : undefined }}>
                      {s?.size ?? p?.size ?? "—"}× · min {s?.minSize ?? p?.minSize ?? "—"}
                      {(s?.size ?? p?.size ?? 1) <= 1 ? " · no redundancy" : ""}
                      {sizeDiffers ? <span style={{ color: "var(--drifting)" }}> · asked for {p!.size}×</span> : null}
                    </td>
                    <td className="py-1.5 text-right font-mono tabular-nums">{s?.pgNum ?? "—"}</td>
                    <td className="py-1.5 text-right font-mono tabular-nums">{s ? bytes(s.storedBytes ?? 0) : "—"}</td>
                    <td className="py-1.5 text-right font-mono tabular-nums">{s?.objects?.toLocaleString() ?? "—"}</td>
                    <td className="py-1.5 text-right font-mono tabular-nums">{s?.maxAvailBytes ? bytes(s.maxAvailBytes) : "—"}</td>
                  </tr>
                );
              })}
              {!pools.length && !poolSeen.length && <tr><td colSpan={6} className="py-2" style={{ color: "var(--text-faint)" }}>No pools.</td></tr>}
            </tbody>
          </table>
        </div>
      </div>
    </div>
  );
}

function Dot({ ok, label, sub }: { ok: boolean; label: string; sub?: string }) {
  return (
    <div className="flex items-center gap-2 py-1 text-xs">
      <span className="size-2 rounded-full" style={{ background: ok ? "var(--dot-settled)" : "var(--dot-failing)" }} />
      <span className="font-mono" style={{ color: "var(--text-body)" }}>{label}</span>
      {sub && <span style={{ color: "var(--text-faint)" }}>{sub}</span>}
    </div>
  );
}
