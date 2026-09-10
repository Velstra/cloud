// The things somebody who ran machines on AWS or GCP reaches for first, on the
// object itself: start it, stop it, reboot it, give it a public address, hang a
// disk on it, back the disk up. None of these is a verb the API has — each is
// a small edit of a spec, or one object made — so they are written here as the
// edits they are, with the wait spelled out where one is needed.

import { useEffect, useState } from "react";
import { toast } from "sonner";
import { ArrowRightLeft, Copy, Play, Power, RotateCw } from "lucide-react";
import { Button } from "@/components/ui/button";
import { DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuTrigger } from "@/components/ui/dropdown-menu";
import { call } from "@/api/transport";
import { humanise, idOf, nameOf, type Resource } from "@/lib/model";
import { SCHEMA, basePath, projectOf, type Collection } from "@/lib/schema";
import { listEvery } from "@/lib/listing";
import { useStore } from "@/app/store";
import { Pressed } from "./Pressed";

const coll = (id: string) => SCHEMA.find((c) => c.id === id)!;

/** Every object of a collection in the project, all pages. */
const all = (project: string, id: string): Promise<Resource[]> => listEvery(coll(id), project).then((x) => x.rows);

/** The project an object's edits go to: its own, whatever the picker says. */
const useProjectOf = (r: Resource) => {
  const picked = useStore((s) => s.project);
  return projectOf(nameOf(r)) ?? picked;
};

const patch = (project: string, r: Resource, c: Collection, spec: Record<string, unknown>) =>
  call(`patch:${c.id}`, "PATCH", `${basePath(c, project)}/${encodeURIComponent(idOf(r))}`, undefined, { spec },
    r.meta.revision ? { "if-match": String(r.meta.revision) } : undefined);

const fresh = (project: string, c: Collection, id: string): Promise<Resource> =>
  call(`get:${c.id}`, "GET", `${basePath(c, project)}/${encodeURIComponent(id)}`);

/** Wait until the guest reports a state, or give up after `seconds`. */
async function untilState(project: string, id: string, state: string, seconds: number): Promise<boolean> {
  const c = coll("instances");
  for (let i = 0; i < seconds; i++) {
    await new Promise((res) => setTimeout(res, 1000));
    const r = await fresh(project, c, id);
    if (r.status?.state === state) return true;
  }
  return false;
}

/** A full name — `projects/<p>/<collection>/<id>`, which is what a create
 *  takes — that nothing in the collection holds yet. */
async function freeName(project: string, id: string, base: string): Promise<string> {
  const taken = new Set((await all(project, id)).map(idOf));
  let pick = base;
  for (let n = 2; taken.has(pick); n++) pick = `${base}-${n}`;
  return `projects/${project}/${id}/${pick}`;
}

const stamp = () => new Date().toISOString().slice(0, 16).replace(/[-:T]/g, "").replace(/^(\d{8})/, "$1-");

// ---- instances ------------------------------------------------------------

export function InstanceQuick({ r, c, reload }: { r: Resource; c: Collection; reload: () => void }) {
  const project = useProjectOf(r);
  const who = useStore((s) => s.who);
  const state: string = r.status?.state ?? ""; const wanted: string = r.spec?.desiredState ?? "Running";
  const running = state === "Running";
  const [volumes, setVolumes] = useState<Resource[] | null>(null);
  const attached: string[] = r.spec?.volumes ?? [];

  const want = async (desired: "Running" | "Stopped", said: string) => {
    try { await patch(project, r, c, { desiredState: desired }); toast(said); reload(); }
    catch (e) { toast.error((e as Error).message); }
  };

  const loadFree = async () => {
    const [vols, guests] = await Promise.all([all(project, "volumes"), all(project, "instances")]);
    const held = new Set(guests.flatMap((g) => (g.spec?.volumes ?? []) as string[]));
    setVolumes(vols.filter((v) => !held.has(nameOf(v))));
  };

  return (
    <>
      {!running && wanted !== "Running" && (
        <Pressed size="sm" variant="secondary" title="Ask for it to run" onPress={() => want("Running", `${idOf(r)} is being started.`)}><Play className="size-3.5" /> Start</Pressed>
      )}
      {(running || wanted === "Running") && (
        <Pressed size="sm" variant="secondary" title="Ask for it to be shut down; the guest gets an ACPI power button first" onPress={async () => {
          if (!confirm(`Stop ${idOf(r)}? Anything unsaved inside it is lost.`)) return;
          await want("Stopped", `${idOf(r)} is being stopped.`);
        }}><Power className="size-3.5" /> Stop</Pressed>
      )}
      {running && (
        <Pressed size="sm" variant="secondary" busyLabel="Rebooting…" title="Stop, wait for it to be down, start again" onPress={async () => {
          if (!confirm(`Reboot ${idOf(r)}? It is stopped and started again — up to a minute and a half.`)) return;
          try {
            await patch(project, r, c, { desiredState: "Stopped" });
            toast(`${idOf(r)} is being stopped…`);
            const down = await untilState(project, idOf(r), "Stopped", 90);
            if (!down) { toast.error(`${idOf(r)} did not stop within 90 s; it is left stopped-when-it-gets-there. Start it by hand.`); reload(); return; }
            const now = await fresh(project, c, idOf(r));
            await patch(project, now, c, { desiredState: "Running" });
            toast(`${idOf(r)} is starting again.`); reload();
          } catch (e) { toast.error((e as Error).message); }
        }}><RotateCw className="size-3.5" /> Reboot</Pressed>
      )}

      <DropdownMenu onOpenChange={(open) => { if (open && volumes == null) loadFree().catch((e) => toast.error((e as Error).message)); }}>
        <DropdownMenuTrigger render={<Button size="sm" variant="secondary" title="Hang a volume of this project on the guest" />}>Attach a volume ▾</DropdownMenuTrigger>
        <DropdownMenuContent align="start">
          {volumes == null && <DropdownMenuItem disabled>Reading volumes…</DropdownMenuItem>}
          {volumes?.length === 0 && <DropdownMenuItem disabled>Every volume in {project} is attached already — make one under Volumes.</DropdownMenuItem>}
          {volumes?.map((v) => (
            <DropdownMenuItem key={nameOf(v)} onClick={async () => {
              try { await patch(project, r, c, { volumes: [...attached, nameOf(v)] }); toast(`${idOf(v)} is being attached.`); setVolumes(null); reload(); }
              catch (e) { toast.error((e as Error).message); }
            }}>
              <span className="font-mono">{idOf(v)}</span><span style={{ color: "var(--text-faint)" }}>{v.spec?.sizeGib ? ` · ${v.spec.sizeGib} GiB` : ""}</span>
            </DropdownMenuItem>
          ))}
        </DropdownMenuContent>
      </DropdownMenu>

      <PublicIp r={r} />
      {who?.cellAdmin && <Migrate r={r} reload={reload} />}
    </>
  );
}

type Destination = { node: string; allowed: boolean; why: string; detail?: string };

/** Move the guest, live, to a machine that can take it — and watch the move,
 *  or abandon it. The destinations come from `:explainMigration`, so what
 *  cannot receive the guest is listed with the reason rather than left out. */
function Migrate({ r, reload }: { r: Resource; reload: () => void }) {
  const project = useProjectOf(r);
  // Two answers, one per way of moving: live keeps the guest running and is
  // refused by a machine that cannot present its CPU; a reboot move stops it
  // here and starts it there, which almost any machine can take. Each node
  // is offered the best way it can take, with the reason when it can take
  // neither.
  const [plan, setPlan] = useState<{ from: string; live: Destination[]; reboot: Destination[] } | null>(null);
  const [moves, setMoves] = useState<Resource[]>([]);
  const inst = coll("instances"); const migs = coll("migrations");
  const done = (m: Resource) => (m.status?.conditions ?? []).some((c: { kind: string; status: string }) => c.kind === "Moved" && c.status === "True");
  const look = () => all(project, "migrations").then((m) => setMoves(m.filter((x) => x.spec?.instance === nameOf(r) && !done(x)))).catch(() => {});
  useEffect(() => { look(); const t = setInterval(look, 5000); return () => clearInterval(t); }, [r.meta.name]); // eslint-disable-line react-hooks/exhaustive-deps
  const explain = async () => {
    const ask = (mode: string) => call("explainMigration", "GET", `${basePath(inst, project)}/${encodeURIComponent(idOf(r))}:explainMigration`, { mode });
    try {
      const [live, reboot] = await Promise.all([ask("Live"), ask("Reboot")]);
      setPlan({ from: live.from ?? reboot.from, live: live.destinations ?? [], reboot: reboot.destinations ?? [] });
    } catch (e) { toast.error((e as Error).message); }
  };

  const start = async (to: string, mode: "Live" | "Reboot") => {
    const from = plan?.from ?? r.status?.node ?? "?";
    const said = mode === "Live"
      ? `Move ${idOf(r)} from ${from} to ${to}, live? Memory is copied while it runs and it pauses only for the last pages. If the move fails it stays on ${from}.`
      : `Move ${idOf(r)} from ${from} to ${to} with a reboot? It is stopped on ${from} and started on ${to} — a minute or so of downtime, and anything unsaved inside it is lost.`;
    if (!confirm(said)) return;
    try {
      const name = await freeName(project, "migrations", `${idOf(r)}-to-${to}`);
      await call("create:migrations", "POST", basePath(migs, project), undefined, { meta: { name }, spec: { instance: nameOf(r), toNode: to, mode } });
      toast(`${idOf(r)} is moving to ${to}${mode === "Reboot" ? " with a reboot" : ""}.`); setPlan(null); look(); reload();
    } catch (e) { toast.error((e as Error).message); }
  };
  const offers = plan ? plan.live.map((d) => {
    const cold = plan.reboot.find((x) => x.node === d.node);
    return d.allowed ? { node: d.node, mode: "Live" as const, why: "" }
      : cold?.allowed ? { node: d.node, mode: "Reboot" as const, why: d.detail || humanise(d.why) }
      : { node: d.node, mode: null, why: d.detail || humanise(d.why) };
  }) : [];

  return (
    <>
      {moves.map((m) => (
        <Pressed key={nameOf(m)} size="sm" variant="secondary" title={`${idOf(m)} — abandon it; the guest keeps running where it is`} onPress={async () => {
          if (!confirm(`Abandon the move of ${idOf(r)} to ${m.spec?.toNode}? The guest keeps running on ${m.spec?.fromNode ?? r.status?.node ?? "its node"}; what was copied is thrown away.`)) return;
          try { await call("delete:migrations", "DELETE", `${basePath(migs, project)}/${encodeURIComponent(idOf(m))}`); toast("Move abandoned."); look(); }
          catch (e) { toast.error((e as Error).message); }
        }}>
          <ArrowRightLeft className="size-3.5" /> Moving to {m.spec?.toNode}{m.spec?.mode === "Reboot" ? " with a reboot" : ""}{m.status?.transferredMib ? ` · ${m.status.transferredMib} MiB copied` : m.status?.receiverReady ? " · receiver ready" : " · preparing"} — abandon
        </Pressed>
      ))}
      {moves.length === 0 && (
        <DropdownMenu onOpenChange={(open) => { if (open && !plan) explain(); }}>
          <DropdownMenuTrigger render={<Button size="sm" variant="secondary" title="Move the running guest to another machine, live" />}><ArrowRightLeft className="size-3.5" /> Migrate ▾</DropdownMenuTrigger>
          <DropdownMenuContent align="start" className="min-w-[24rem]">
            {!plan && <DropdownMenuItem disabled>Asking which machines could take it…</DropdownMenuItem>}
            {offers.map((o) => (
              <DropdownMenuItem key={o.node} disabled={!o.mode} onClick={() => o.mode && start(o.node, o.mode)}>
                <span className="grid gap-0.5">
                  <span><span className="font-mono">{o.node}</span>
                    <span style={{ color: o.mode === "Live" ? "var(--settled)" : o.mode ? "var(--drifting)" : "var(--text-faint)" }}> · {o.mode === "Live" ? "live, no downtime" : o.mode ? "with a reboot — stopped here, started there" : "cannot take it"}</span></span>
                  {o.why && <span className="text-[11px]" style={{ color: "var(--text-faint)" }}>{o.mode ? "not live: " : ""}{o.why}</span>}
                </span>
              </DropdownMenuItem>
            ))}
            {plan && offers.length === 0 && <DropdownMenuItem disabled>No other machine in the cell.</DropdownMenuItem>}
          </DropdownMenuContent>
        </DropdownMenu>
      )}
    </>
  );
}

// ---- nodes ----------------------------------------------------------------

/** The two switches an operator throws on a machine: stop placing new guests
 *  here, and move the ones that are here away. Both are spec edits; both are
 *  reversible from the same button. */
export function NodeQuick({ r, c, reload }: { r: Resource; c: Collection; reload: () => void }) {
  const project = useStore((s) => s.project);
  const schedulable = r.spec?.schedulable !== false; const evacuating = !!r.spec?.evacuate;
  const flip = async (spec: Record<string, unknown>, said: string) => {
    try { await patch(project, r, c, spec); toast(said); reload(); } catch (e) { toast.error((e as Error).message); }
  };
  return (
    <>
      <Pressed size="sm" variant="secondary" title={schedulable ? "New guests stop being placed here; the ones here stay" : "Let new guests be placed here again"}
        onPress={() => flip({ schedulable: !schedulable }, schedulable ? `${idOf(r)} takes no new guests.` : `${idOf(r)} takes guests again.`)}>
        {schedulable ? "Cordon" : "Uncordon"}
      </Pressed>
      <Pressed size="sm" variant={evacuating ? "secondary" : "destructive"} title={evacuating ? "Stop moving guests away" : "Move every guest here to another machine, live where it can be"}
        onPress={async () => {
          if (!evacuating && !confirm(`Evacuate ${idOf(r)}? Every guest on it is moved to another machine — live where the destination can take it, otherwise stopped and started there.`)) return;
          await flip({ evacuate: !evacuating, ...(evacuating ? {} : { schedulable: false }) }, evacuating ? `${idOf(r)} keeps its guests.` : `${idOf(r)} is being evacuated.`);
        }}>
        {evacuating ? "Stop evacuating" : "Evacuate"}
      </Pressed>
    </>
  );
}

/** Allocate-and-associate, or release: the two things a public address is for. */
function PublicIp({ r }: { r: Resource }) {
  const project = useProjectOf(r);
  const [mine, setMine] = useState<Resource[] | null>(null);
  const look = () => all(project, "floatingips").then((f) => setMine(f.filter((x) => x.spec?.instance === nameOf(r)))).catch(() => setMine([]));
  useEffect(() => { look(); }, [r.meta.name, r.meta.revision]); // eslint-disable-line react-hooks/exhaustive-deps
  const fips = coll("floatingips");
  if (mine == null) return null;
  if (mine.length === 0) return (
    <Pressed size="sm" variant="secondary" title="Take the lowest free address from the cell's public pool and put it in front of this guest" onPress={async () => {
      try {
        const name = await freeName(project, "floatingips", `ip-${idOf(r)}`);
        await call("create:floatingips", "POST", basePath(fips, project), undefined, { meta: { name }, spec: { instance: nameOf(r) } });
        toast(`A public address is being given to ${idOf(r)}.`); look();
      } catch (e) { toast.error((e as Error).message); }
    }}>Get a public IP</Pressed>
  );
  return (
    <>
      {mine.map((f) => (
        <Pressed key={nameOf(f)} size="sm" variant="secondary" title={`${f.spec?.address ?? f.status?.address ?? idOf(f)} — release it back to the pool`} onPress={async () => {
          if (!confirm(`Release ${f.spec?.address ?? idOf(f)}? Anything that reached the guest by it stops doing so.`)) return;
          try { await call("delete:floatingips", "DELETE", `${basePath(fips, project)}/${encodeURIComponent(idOf(f))}`); toast("Address is being released."); look(); }
          catch (e) { toast.error((e as Error).message); }
        }}>Release {f.spec?.address ?? f.status?.address ?? idOf(f)}</Pressed>
      ))}
    </>
  );
}

// ---- volumes --------------------------------------------------------------

export function VolumeQuick({ r, reload }: { r: Resource; c: Collection; reload: () => void }) {
  const project = useProjectOf(r);
  const [guests, setGuests] = useState<Resource[] | null>(null);
  const holder = guests?.find((g) => ((g.spec?.volumes ?? []) as string[]).includes(nameOf(r)));
  const inst = coll("instances"); const backups = coll("backups");
  useEffect(() => { all(project, "instances").then(setGuests).catch(() => setGuests([])); }, [project, r.meta.revision]);

  return (
    <>
      {guests && !holder && (
        <DropdownMenu>
          <DropdownMenuTrigger render={<Button size="sm" variant="secondary" title="Hang this volume on a guest of the project" />}>Attach to a guest ▾</DropdownMenuTrigger>
          <DropdownMenuContent align="start">
            {guests.length === 0 && <DropdownMenuItem disabled>No guests in {project} yet.</DropdownMenuItem>}
            {guests.map((g) => (
              <DropdownMenuItem key={nameOf(g)} onClick={async () => {
                try { await patch(project, g, inst, { volumes: [...((g.spec?.volumes ?? []) as string[]), nameOf(r)] }); toast(`${idOf(r)} is being attached to ${idOf(g)}.`); reload(); }
                catch (e) { toast.error((e as Error).message); }
              }}><span className="font-mono">{idOf(g)}</span><span style={{ color: "var(--text-faint)" }}> · {g.status?.state ?? "unreported"}</span></DropdownMenuItem>
            ))}
          </DropdownMenuContent>
        </DropdownMenu>
      )}
      {holder && (
        <Pressed size="sm" variant="secondary" title={`Take it off ${idOf(holder)}; unmount it inside first`} onPress={async () => {
          if (!confirm(`Detach ${idOf(r)} from ${idOf(holder)}? Unmount it inside the guest first, or what is being written is lost.`)) return;
          try { await patch(project, holder, inst, { volumes: ((holder.spec?.volumes ?? []) as string[]).filter((v) => v !== nameOf(r)) }); toast(`${idOf(r)} is being detached.`); reload(); }
          catch (e) { toast.error((e as Error).message); }
        }}>Detach from {idOf(holder)}</Pressed>
      )}
      <Pressed size="sm" variant="secondary" title="Copy it to the cell's most roomy backup target, now" onPress={async () => {
        try {
          const name = await freeName(project, "backups", `${idOf(r)}-${stamp()}`);
          await call("create:backups", "POST", basePath(backups, project), undefined, { meta: { name }, spec: { volume: nameOf(r) } });
          toast(`Backup ${name.split("/").pop()} asked for.`); reload();
        } catch (e) { toast.error((e as Error).message); }
      }}>Back up now</Pressed>
    </>
  );
}

// ---- how to reach it --------------------------------------------------------

/** The user an image's family logs in as, by convention of the distributions. */
const loginUser = (image: string) => {
  const f = image.split("/").pop() ?? "";
  for (const [k, u] of [["debian", "debian"], ["ubuntu", "ubuntu"], ["fedora", "fedora"], ["centos", "centos"], ["rocky", "rocky"], ["alma", "almalinux"], ["alpine", "alpine"], ["arch", "arch"]] as const)
    if (f.startsWith(k)) return u;
  return "root";
};

export function Connect({ r }: { r: Resource }) {
  const project = useProjectOf(r);
  const [publicIps, setPublicIps] = useState<string[]>([]);
  useEffect(() => {
    all(project, "floatingips").then((f) => setPublicIps(f.filter((x) => x.spec?.instance === nameOf(r)).map((x) => x.spec?.address ?? x.status?.address).filter(Boolean))).catch(() => {});
  }, [project, r.meta.name, r.meta.revision]);
  const privateIps: string[] = r.status?.addresses ?? [];
  const user = loginUser(String(r.spec?.image ?? ""));
  const reach = publicIps[0] ?? privateIps[0];
  const line = reach ? `ssh ${user}@${reach}` : "";
  const Line = ({ label, value }: { label: string; value: string }) => (
    <div className="flex items-center gap-2 text-xs">
      <span className="w-24 shrink-0" style={{ color: "var(--text-muted)" }}>{label}</span>
      <code className="font-mono" style={{ color: "var(--text-body)" }}>{value}</code>
      <Button size="icon-xs" variant="ghost" title="Copy" onClick={() => navigator.clipboard?.writeText(value).then(() => toast("Copied."))}><Copy className="size-3" /></Button>
    </div>
  );
  return (
    <div className="grid gap-1.5">
      {publicIps.map((ip) => <Line key={ip} label="Public IP" value={ip} />)}
      {privateIps.map((ip) => <Line key={ip} label="Private IP" value={ip} />)}
      {!privateIps.length && !publicIps.length && <p className="text-xs" style={{ color: "var(--text-faint)" }}>No address yet — it comes with the first lease once the guest is running.</p>}
      {line && <Line label="SSH" value={line} />}
      {line && !(r.spec?.sshKeys ?? []).length && !r.spec?.userData && (
        <p className="text-[11px]" style={{ color: "var(--drifting)" }}>No SSH key or cloud-init was given at creation, so the image's default user has nothing to log in with. The Screen panel still works.</p>
      )}
      <p className="text-[11px]" style={{ color: "var(--text-faint)" }}>The login user is the image family's convention ({user}); a cloud-init file may have chosen another.</p>
    </div>
  );
}
