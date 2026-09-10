// Who may do what in a project: its bindings as rows a person can work with —
// one per member and rung — beside the ladder itself, in the platform's own
// words. Saved as the set it is, with the revision the page was read at, so a
// colleague's change in between is refused rather than overwritten.

import { useEffect, useMemo, useState } from "react";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { call } from "@/api/transport";
import { idOf, type Resource } from "@/lib/model";
import { RUNGS, loadRoles, useCan, type CustomRole } from "@/lib/iam";
import { basePath, type Collection } from "@/lib/schema";
import { listEvery } from "@/lib/listing";
import { SCHEMA } from "@/lib/schema";
import { useStore } from "@/app/store";
import { Pressed } from "./Pressed";

type Binding = { role: string; members: string[] };
type Row = { member: string; role: string };

const rows = (b: Binding[]): Row[] => b.flatMap((x) => x.members.map((m) => ({ member: m, role: x.role })));
const bindings = (r: Row[]): Binding[] => {
  const by = new Map<string, string[]>();
  for (const x of r) { if (!x.member.trim()) continue; by.set(x.role, [...(by.get(x.role) ?? []), x.member.trim()]); }
  return [...by].map(([role, members]) => ({ role, members: [...new Set(members)] }));
};

export function Members({ r, coll, reload }: { r: Resource; coll: Collection; reload: () => void }) {
  const who = useStore((s) => s.who);
  const can = useCan();
  const may = can("administer", coll, idOf(r));
  const [list, setList] = useState<Row[]>(() => rows(r.spec?.bindings ?? []));
  const [roles, setRoles] = useState<Record<string, CustomRole>>({});
  const [users, setUsers] = useState<string[]>([]);
  const [member, setMember] = useState(""); const [role, setRole] = useState("viewer");
  useEffect(() => { setList(rows(r.spec?.bindings ?? [])); }, [r.meta.revision]); // eslint-disable-line react-hooks/exhaustive-deps
  useEffect(() => {
    loadRoles().then(setRoles).catch(() => {});
    if (who?.cellAdmin) listEvery(SCHEMA.find((c) => c.id === "users")!, "").then((x) => setUsers(x.rows.map(idOf))).catch(() => {});
  }, [who?.cellAdmin]);
  const dirty = useMemo(() => JSON.stringify(bindings(list)) !== JSON.stringify(bindings(rows(r.spec?.bindings ?? []))), [list, r]);

  const save = async () => {
    try {
      await call("patch:projects", "PATCH", `${basePath(coll, "")}/${encodeURIComponent(idOf(r))}`, undefined,
        { spec: { bindings: bindings(list) } }, r.meta.revision ? { "if-match": String(r.meta.revision) } : undefined);
      toast("Members saved."); reload();
    } catch (e) { toast.error((e as Error).message); }
  };
  const label = (role: string) => roles[role]?.displayName ? `${roles[role].displayName} (${role})` : role;

  return (
    <div className="grid gap-4">
      <table className="w-full text-xs">
        <thead><tr style={{ color: "var(--text-faint)" }}><th className="pb-1 text-left font-medium">Member</th><th className="pb-1 text-left font-medium">May</th><th /></tr></thead>
        <tbody>
          {list.map((x, i) => (
            <tr key={i} className="border-t" style={{ borderColor: "var(--border-subtle)", background: x.member === who?.subject ? "var(--surface-sunken)" : undefined }}>
              <td className="py-1.5 font-mono">{x.member}{x.member === who?.subject ? <span style={{ color: "var(--text-faint)" }}> · you</span> : null}</td>
              <td className="py-1.5">
                <select disabled={!may} value={x.role} onChange={(e) => setList(list.map((y, j) => (j === i ? { ...y, role: e.target.value } : y)))}
                  className="h-7 rounded-[4px] border px-1.5 text-xs" style={{ borderColor: "var(--border)", background: "var(--surface)", color: "var(--text-body)" }}>
                  {RUNGS.map((g) => <option key={g.rung} value={g.rung}>{g.rung}</option>)}
                  {Object.keys(roles).map((k) => <option key={k} value={k}>{label(k)}</option>)}
                  {!RUNGS.some((g) => g.rung === x.role) && !roles[x.role] && <option value={x.role}>{x.role}</option>}
                </select>
              </td>
              <td className="py-1.5 text-right">{may && <Button size="sm" variant="ghost" onClick={() => setList(list.filter((_, j) => j !== i))}>Remove</Button>}</td>
            </tr>
          ))}
          {!list.length && <tr><td colSpan={3} className="py-2" style={{ color: "var(--text-faint)" }}>Nobody is bound here yet; only cell operators can reach it.</td></tr>}
        </tbody>
      </table>

      {may && (
        <form className="flex flex-wrap items-center gap-2" onSubmit={(e) => { e.preventDefault(); if (!member.trim()) return; setList([...list, { member: member.trim(), role }]); setMember(""); }}>
          <Input list="velstra-users" value={member} onChange={(e) => setMember(e.target.value)} placeholder="user id or subject, as the sign-in reports it" className="h-8 w-72 font-mono text-xs" />
          <datalist id="velstra-users">{users.map((u) => <option key={u} value={u} />)}</datalist>
          <select value={role} onChange={(e) => setRole(e.target.value)} className="h-8 rounded-[4px] border px-1.5 text-xs" style={{ borderColor: "var(--border)", background: "var(--surface)", color: "var(--text-body)" }}>
            {RUNGS.map((g) => <option key={g.rung} value={g.rung}>{g.rung}</option>)}
            {Object.keys(roles).map((k) => <option key={k} value={k}>{label(k)}</option>)}
          </select>
          <Button type="submit" size="sm" variant="secondary" disabled={!member.trim()}>Add</Button>
          <span className="ml-auto flex gap-2">
            {dirty && <Button type="button" size="sm" variant="ghost" onClick={() => setList(rows(r.spec?.bindings ?? []))}>Undo</Button>}
            <Pressed type="button" size="sm" disabled={!dirty} onPress={save}>Save members</Pressed>
          </span>
        </form>
      )}
      {!may && <p className="text-xs" style={{ color: "var(--text-faint)" }}>Changing who may is a project admin's or a cell operator's.</p>}

      <details className="text-xs">
        <summary className="cursor-pointer" style={{ color: "var(--text-muted)" }}>The four rungs, and what each may</summary>
        <table className="mt-2 w-full">
          <tbody>
            {RUNGS.map((g) => (
              <tr key={g.rung} className="border-t align-top" style={{ borderColor: "var(--border-subtle)" }}>
                <td className="py-1 pr-3 font-mono">{g.rung}</td><td className="py-1 pr-3" style={{ color: "var(--text-body)" }}>{g.may}</td><td className="py-1" style={{ color: "var(--text-faint)" }}>not: {g.cannot}</td>
              </tr>
            ))}
            {Object.entries(roles).map(([k, v]) => (
              <tr key={k} className="border-t align-top" style={{ borderColor: "var(--border-subtle)" }}>
                <td className="py-1 pr-3 font-mono">{k}</td>
                <td className="py-1 pr-3" style={{ color: "var(--text-body)" }}>{v.displayName}{v.description ? ` — ${v.description}` : ""}</td>
                <td className="py-1" style={{ color: "var(--text-faint)" }}>{v.grants.map((g) => `${g.verb} ${g.collections.join(", ")}`).join("; ")}</td>
              </tr>
            ))}
          </tbody>
        </table>
        <p className="mt-2" style={{ color: "var(--text-faint)" }}>A project admin is not a cell operator: the cell's operators are named in its configuration and may do anything anywhere, and only they raise a project's limits.</p>
      </details>
    </div>
  );
}

/** What a role the cell writes down grants: a verb, per collection. */
export function GrantsEditor({ value, onChange, disabled }: { value: { verb: string; collections: string[] }[]; onChange: (v: unknown) => void; disabled: boolean }) {
  const grants = value ?? [];
  const tenantCollections = SCHEMA.filter((c) => c.scope === "project" && c.id !== "audit" && c.id !== "usage").map((c) => c.id);
  const set = (i: number, g: { verb: string; collections: string[] }) => onChange(grants.map((x, j) => (j === i ? g : x)));
  return (
    <div className="grid gap-3">
      {grants.map((g, i) => (
        <div key={i} className="grid gap-2 rounded-[4px] border p-3" style={{ borderColor: "var(--border-subtle)" }}>
          <div className="flex items-center gap-2 text-xs">
            <span style={{ color: "var(--text-muted)" }}>May</span>
            <select disabled={disabled} value={g.verb} onChange={(e) => set(i, { ...g, verb: e.target.value })} className="h-7 rounded-[4px] border px-1.5 text-xs" style={{ borderColor: "var(--border)", background: "var(--surface)", color: "var(--text-body)" }}>
              <option value="read">read</option><option value="operate">operate — run what is there</option><option value="write">write — create and delete</option><option value="administer">administer — change who may</option>
            </select>
            <span style={{ color: "var(--text-muted)" }}>these:</span>
            {!disabled && <Button type="button" size="sm" variant="ghost" className="ml-auto" onClick={() => onChange(grants.filter((_, j) => j !== i))}>Remove</Button>}
          </div>
          <div className="flex flex-wrap gap-x-3 gap-y-1 text-xs">
            {tenantCollections.map((c) => (
              <label key={c} className="inline-flex items-center gap-1">
                <input type="checkbox" disabled={disabled} checked={g.collections.includes(c)} onChange={(e) => set(i, { ...g, collections: e.target.checked ? [...g.collections, c] : g.collections.filter((x) => x !== c) })} />
                <span className="font-mono">{c}</span>
              </label>
            ))}
          </div>
        </div>
      ))}
      <div><Button type="button" size="sm" variant="secondary" disabled={disabled} onClick={() => onChange([...grants, { verb: "operate", collections: [] }])}>Add a grant</Button></div>
      <p className="text-[11px]" style={{ color: "var(--text-faint)" }}>Read is implied by every other verb on the same collection. No wildcard: a role names what it covers.</p>
    </div>
  );
}
