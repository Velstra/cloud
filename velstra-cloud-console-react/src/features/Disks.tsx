// The disks handed to Ceph.
//
// **The refusals are the feature.** Handing a disk to an OSD erases it, so a
// device is offered only when the node it is plugged into says it is provably
// empty — and every other one is shown *with the reason, in words*. Greying a
// row out answers "why can I not select this disk" with silence, and silence is
// what sends somebody to a terminal to find out something the platform already
// knew.
//
// The sentences are the schema's, not this file's. They are carried in Rust so
// there is one copy of each, and `velstra-cloud-api/tests/console_covers_the_model.rs`
// pins every one against `ceph::may_consume` — this console does not link the
// model, it speaks REST, so nothing else is in a position to stop the two
// wordings drifting apart. What stood here before was a paraphrase no test
// held ("Choosing a disk for an OSD erases it") over two free-text boxes: the
// drift the doc comment was written to prevent, already happened.

import { useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import { call } from "@/api/transport";
import { at, basePath, collection, type FieldOf } from "@/lib/schema";
import { idOf, type Resource } from "@/lib/model";

type Osd = { node: string; device: string; evenIfUnsuitable?: boolean };
type Device = {
  path?: string; sizeGib?: number; rotational?: boolean; model?: string; kernelName?: string;
  state?: { kind?: string } & Record<string, unknown>;
};

/**
 * Why this disk is not offered, in the schema's own sentence, or "" when it is.
 *
 * Substitution only — `{fstype}`, `{at}`, `{sizeGib}` — never a sentence
 * written here.
 */
export function refusalFor(f: FieldOf<"diskList">, device: Device): string {
  const state = device.state ?? { kind: "Free" };
  const kind = state.kind ?? "Free";
  const say = (text: string) =>
    text.replace(/\{(\w+)\}/g, (_, key: string) => {
      const v = key === "minGib" ? f.minGib : key === "sizeGib" ? device.sizeGib : (state as Record<string, unknown>)[key];
      return v === undefined || v === null ? "?" : String(v);
    });
  const template = (f.refusals ?? []).find((r) => r.kind === kind);
  if (template) return say(template.text);
  // Not free, and nothing here has a sentence for it: a node running a newer
  // agent than this page. Refused rather than offered, because the safe
  // direction when the answer is unknown is the conservative one, and this is
  // the one control where being wrong erases somebody's data.
  if (kind !== "Free") return say(f.unknown);
  if (Number(device.sizeGib ?? 0) < Number(f.minGib)) return say(f.tooSmall);
  return "";
}

/**
 * What tells two disks apart at a glance.
 *
 * Spinning versus solid state, because mixing them in one pool is a decision
 * rather than an accident; the kernel name because that is what an operator is
 * holding an `lsblk` against.
 */
const note = (d: Device) =>
  [`${d.sizeGib ?? 0} GiB`, d.rotational ? "spinning" : "solid state", d.model, d.kernelName]
    .filter(Boolean).join(" · ");

const same = (a: Osd, node: string, device: string) => a.node === node && a.device === device;

export function DiskList({ f, value, onChange, disabled }: {
  f: FieldOf<"diskList">; value: Osd[]; onChange: (v: unknown) => void; disabled: boolean;
}) {
  const chosen: Osd[] = Array.isArray(value) ? value : [];
  const [nodes, setNodes] = useState<Resource[] | null>(null);
  const [unreadable, setUnreadable] = useState("");

  useEffect(() => {
    const target = collection(f.collection);
    if (!target) { setNodes([]); return; }
    // Global scope, no project: `nodes` is the cell's, like this collection.
    call(`list:${target.id}`, "GET", basePath(target, ""), { pageSize: 200 })
      .then((a) => setNodes(a.items ?? []))
      // Fails loud, not soft-empty: an empty picker reads as "there are no
      // disks", which is a different and much worse answer than "I could not
      // ask".
      .catch((e) => { setNodes([]); setUnreadable((e as Error).message); });
  }, [f.collection]);

  const add = (node: string, device: string) => onChange([...chosen, { node, device }]);
  const drop = (node: string, device: string) =>
    onChange(chosen.filter((o) => !same(o, node, device)));
  const waive = (node: string, device: string, yes: boolean) =>
    onChange(chosen.map((o) => (same(o, node, device) ? { ...o, evenIfUnsuitable: yes } : o)));

  // Asked for, and no node is reporting the disk. A node that is down looks
  // exactly like this, and dropping these from the screen would let an edit
  // that never touched them silently look like it had removed them.
  const reported = (node: string, device: string) =>
    (nodes ?? []).some((n) => idOf(n) === node &&
      ((at(n, "status.devices") as Device[] | undefined) ?? []).some((d) => d.path === device));
  const stray = chosen.filter((o) => !reported(o.node, o.device));

  // One disk makes one OSD. A spec asking twice for the same device fails on
  // the second step with an error about a device already in use.
  const twice = chosen.filter((o, i) => chosen.findIndex((x) => same(x, o.node, o.device)) !== i);

  return (
    <div className="grid gap-3">
      {/* Above the list, not under it. A warning below a row of buttons is a
          warning read after the click. */}
      <p className="rounded-[4px] border px-2.5 py-1.5 text-[11px]"
        style={{ borderColor: "var(--drifting)", color: "var(--drifting)", background: "var(--surface-sunken)" }}>
        {f.warning}
      </p>

      {twice.length > 0 && (
        <p className="text-[11px]" style={{ color: "var(--failing)" }}>
          {twice[0].device} on {twice[0].node} is listed twice, and one disk makes one OSD.
        </p>
      )}

      {nodes === null && <p className="text-xs" style={{ color: "var(--text-faint)" }}>Reading what the nodes can see…</p>}
      {unreadable && (
        <p className="text-xs" style={{ color: "var(--failing)" }}>
          The nodes could not be read, so no disk can be offered: {unreadable}
        </p>
      )}

      {(nodes ?? []).map((n) => {
        const node = idOf(n);
        const devices = (at(n, "status.devices") as Device[] | undefined) ?? [];
        return (
          <div key={node} className="rounded-[4px] border" style={{ borderColor: "var(--border)" }}>
            <div className="flex items-baseline gap-2 border-b px-2.5 py-1.5" style={{ borderColor: "var(--border-subtle)" }}>
              <span className="font-mono text-xs" style={{ color: "var(--text-strong)" }}>{node}</span>
              {!devices.length && <span className="text-[11px]" style={{ color: "var(--text-faint)" }}>reports no disks</span>}
            </div>
            <ul>
              {devices.map((d) => {
                const device = d.path ?? "";
                const taken = chosen.some((o) => same(o, node, device));
                // A disk already in the spec reads as chosen whatever it
                // reports now: Ceph reports its own disks as OSDs, which
                // `may_consume` refuses — and a control that believed that
                // would render every disk of a working cluster unavailable,
                // with no way left to take one out.
                const why = taken ? "" : refusalFor(f, d);
                const held = chosen.find((o) => same(o, node, device));
                return (
                  <li key={device} className="flex flex-wrap items-center gap-2 border-t px-2.5 py-1.5 text-xs"
                    style={{ borderColor: "var(--border-subtle)", background: taken ? "var(--surface-hover)" : undefined }}
                    data-node={node} data-device={device}>
                    <span className="font-mono" style={{ color: "var(--text-body)" }}>{device}</span>
                    <span style={{ color: "var(--text-faint)" }}>{note(d)}</span>
                    {why ? (
                      <span className="ml-auto flex items-center gap-2 text-right" style={{ color: "var(--text-muted)" }}>
                        <span>Not offered: {why}</span>
                        {/* The waiver is an explicit choice on a refused disk,
                            not a column on every row. */}
                        <Button type="button" size="sm" variant="ghost" disabled={disabled}
                          title="Take it anyway — the platform will not stop you, and the disk is erased"
                          onClick={() => onChange([...chosen, { node, device, evenIfUnsuitable: true }])}>
                          Take it anyway
                        </Button>
                      </span>
                    ) : (
                      <span className="ml-auto flex items-center gap-2">
                        {taken && held?.evenIfUnsuitable && (
                          <button type="button" disabled={disabled} className="text-[11px] underline"
                            style={{ color: "var(--drifting)" }} onClick={() => waive(node, device, false)}>
                            taken against its refusal
                          </button>
                        )}
                        <Button type="button" size="sm" variant={taken ? "ghost" : "secondary"} disabled={disabled}
                          data-disk={taken ? "remove" : "add"}
                          onClick={() => (taken ? drop(node, device) : add(node, device))}>
                          {taken ? "Remove" : "Add"}
                        </Button>
                      </span>
                    )}
                  </li>
                );
              })}
            </ul>
          </div>
        );
      })}

      {stray.length > 0 && (
        <div className="rounded-[4px] border" style={{ borderColor: "var(--drifting)" }}>
          <div className="flex items-baseline gap-2 border-b px-2.5 py-1.5" style={{ borderColor: "var(--border-subtle)" }}>
            <span className="text-xs" style={{ color: "var(--text-strong)" }}>Not reported</span>
            <span className="text-[11px]" style={{ color: "var(--text-faint)" }}>
              asked for, and no node is reporting the disk — a node that is down looks like this
            </span>
          </div>
          <ul>
            {stray.map((o) => (
              <li key={`${o.node}/${o.device}`} className="flex items-center gap-2 border-t px-2.5 py-1.5 text-xs"
                style={{ borderColor: "var(--border-subtle)" }} data-node={o.node} data-device={o.device}>
                <span className="font-mono" style={{ color: "var(--text-body)" }}>{o.device}</span>
                <span style={{ color: "var(--text-faint)" }}>on {o.node}</span>
                <Button type="button" size="sm" variant="ghost" className="ml-auto" disabled={disabled}
                  data-disk="remove" onClick={() => drop(o.node, o.device)}>Remove</Button>
              </li>
            ))}
          </ul>
        </div>
      )}

      {nodes !== null && !nodes.length && !unreadable && (
        <p className="text-xs" style={{ color: "var(--text-faint)" }}>No node has reported its disks yet.</p>
      )}
    </div>
  );
}
