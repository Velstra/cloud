// One account, as a cell operator manages it: where it is bound and as what,
// its password, and — for a service account — the tokens it signs in with.
// A token is shown exactly once, the way the API hands it out.

import { useEffect, useState } from "react";
import { toast } from "sonner";
import { Copy } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { call } from "@/api/transport";
import { ago, idOf, type Resource } from "@/lib/model";
import { listEvery } from "@/lib/listing";
import { SCHEMA, type Collection } from "@/lib/schema";
import { useStore } from "@/app/store";
import { Pressed } from "./Pressed";

type Binding = { role: string; members: string[] };

export function Account({ r }: { r: Resource }) {
  const who = useStore((s) => s.who);
  const me = who?.subject === idOf(r);
  const [where, setWhere] = useState<{ project: string; role: string }[] | null>(null);
  const [pw, setPw] = useState(""); const [current, setCurrent] = useState("");
  const [purpose, setPurpose] = useState(""); const [minted, setMinted] = useState<{ token: string; purpose?: string } | null>(null);
  const [tokens, setTokens] = useState<{ id: string; purpose?: string; issuedAt?: number }[] | null>(null);
  const listTokens = () => call("list-tokens", "GET", `/api/v1/users/${encodeURIComponent(idOf(r))}/tokens`)
    .then((a) => setTokens(a.items ?? [])).catch(() => setTokens([]));
  useEffect(() => { if (r.spec?.service) listTokens(); }, [r.meta.name, r.spec?.service]); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    listEvery(SCHEMA.find((c) => c.id === "projects")!, "").then((x) => {
      const out: { project: string; role: string }[] = [];
      for (const p of x.rows) for (const b of (p.spec?.bindings ?? []) as Binding[]) if (b.members.includes(idOf(r))) out.push({ project: idOf(p), role: b.role });
      setWhere(out);
    }).catch(() => setWhere([]));
  }, [r.meta.name]);

  return (
    <div className="grid gap-5">
      <div>
        <div className="mb-1 text-[11px] font-semibold uppercase tracking-[0.07em]" style={{ color: "var(--text-muted)" }}>Bound in</div>
        {where == null ? <p className="text-xs" style={{ color: "var(--text-faint)" }}>Reading…</p>
          : where.length === 0 ? <p className="text-xs" style={{ color: "var(--text-faint)" }}>{r.spec?.cellAdmin ? "Everywhere — a cell operator." : "No project yet. Add them under a project's Members."}</p>
          : <ul className="grid gap-1 text-xs">{where.map((w) => <li key={w.project + w.role}><a href={`#/c/projects/${encodeURIComponent(w.project)}`} className="font-mono" style={{ color: "var(--brand)" }}>{w.project}</a> <span style={{ color: "var(--text-muted)" }}>· {w.role}</span></li>)}</ul>}
        {r.spec?.cellAdmin && where && where.length > 0 && <p className="mt-1 text-[11px]" style={{ color: "var(--text-faint)" }}>Also a cell operator, so the bindings above are moot.</p>}
      </div>

      {!r.spec?.service && (
        <form className="grid gap-2" onSubmit={(e) => e.preventDefault()}>
          <div className="text-[11px] font-semibold uppercase tracking-[0.07em]" style={{ color: "var(--text-muted)" }}>Password</div>
          <div className="flex flex-wrap items-center gap-2">
            {me && <Input type="password" value={current} onChange={(e) => setCurrent(e.target.value)} placeholder="current password" className="h-8 w-52 text-xs" autoComplete="current-password" />}
            <Input type="password" value={pw} onChange={(e) => setPw(e.target.value)} placeholder="new password" className="h-8 w-52 text-xs" autoComplete="new-password" />
            <Pressed size="sm" variant="secondary" disabled={pw.length < 8 || (me && !current)} onPress={async () => {
              try {
                await call("setPassword", "PUT", `/api/v1/users/${encodeURIComponent(idOf(r))}/password`, undefined, me ? { current, password: pw } : { password: pw });
                toast(me ? "Your password is set; every other session of yours is ended." : `${idOf(r)}'s password is set; their other sessions are ended.`); setPw(""); setCurrent("");
              } catch (e) { toast.error((e as Error).message); }
            }}>Set password</Pressed>
          </div>
          <p className="text-[11px]" style={{ color: "var(--text-faint)" }}>At least 8 characters. {me ? "Your current one is needed." : "As a cell operator, no current one is needed."}</p>
        </form>
      )}

      {r.spec?.service && (
        <div className="grid gap-2">
          <div className="text-[11px] font-semibold uppercase tracking-[0.07em]" style={{ color: "var(--text-muted)" }}>Tokens</div>
          <div className="flex flex-wrap items-center gap-2">
            <Input value={purpose} onChange={(e) => setPurpose(e.target.value)} placeholder="what this token is for — ci, backup-runner…" className="h-8 w-72 text-xs" />
            <Pressed size="sm" variant="secondary" onPress={async () => {
              try { setMinted(await call("mint-token", "POST", `/api/v1/users/${encodeURIComponent(idOf(r))}/tokens`, undefined, { purpose: purpose || undefined })); toast("Token minted — copy it now."); setPurpose(""); listTokens(); }
              catch (e) { toast.error((e as Error).message); }
            }}>Mint a token</Pressed>
          </div>
          {minted && (
            <div className="grid gap-1 rounded-[4px] border p-3" style={{ borderColor: "var(--drifting)", background: "var(--surface-sunken)" }}>
              <div className="flex items-center gap-2 text-xs" style={{ color: "var(--drifting)" }}>Shown once. Copy it now; it cannot be read back.
                <Button size="icon-xs" variant="ghost" className="ml-auto" title="Copy" onClick={() => navigator.clipboard?.writeText(minted.token).then(() => toast("Copied."))}><Copy className="size-3" /></Button>
              </div>
              <code className="break-all font-mono text-xs" style={{ color: "var(--text-body)" }}>{minted.token}</code>
            </div>
          )}
          <table className="w-full text-xs">
            <thead><tr style={{ color: "var(--text-faint)" }}><th className="pb-1 text-left font-medium">What it is for</th><th className="pb-1 text-left font-medium">Minted</th><th /></tr></thead>
            <tbody>
              {tokens?.map((t) => (
                <tr key={t.id} className="border-t" style={{ borderColor: "var(--border-subtle)" }}>
                  <td className="py-1.5">{t.purpose || <span style={{ color: "var(--text-faint)" }}>no purpose given</span>}</td>
                  <td className="py-1.5" style={{ color: "var(--text-muted)" }}>{t.issuedAt ? ago(t.issuedAt) : "—"}</td>
                  <td className="py-1.5 text-right">
                    <Pressed size="sm" variant="destructive" title="This token stops working immediately" onPress={async () => {
                      if (!confirm(`Revoke this token${t.purpose ? ` (${t.purpose})` : ""}? Whatever is using it stops being able to sign in at once.`)) return;
                      try { await call("revoke-token", "DELETE", `/api/v1/users/${encodeURIComponent(idOf(r))}/tokens/${encodeURIComponent(t.id)}`); toast("Token revoked."); listTokens(); }
                      catch (e) { toast.error((e as Error).message); }
                    }}>Revoke</Pressed>
                  </td>
                </tr>
              ))}
              {tokens?.length === 0 && <tr><td colSpan={3} className="py-2" style={{ color: "var(--text-faint)" }}>No tokens yet.</td></tr>}
            </tbody>
          </table>
          <p className="text-[11px]" style={{ color: "var(--text-faint)" }}>Several may exist at once, so a rotation has no gap. A token cannot be read back — a lost one is revoked and replaced.</p>
        </div>
      )}
    </div>
  );
}

/** Disable or enable, and make or unmake a cell operator. */
export function UserQuick({ r, c, reload }: { r: Resource; c: Collection; reload: () => void }) {
  const who = useStore((s) => s.who);
  const me = who?.subject === idOf(r);
  const flip = async (spec: Record<string, unknown>, said: string) => {
    try {
      await call("patch:users", "PATCH", `/api/v1/${c.id}/${encodeURIComponent(idOf(r))}`, undefined, { spec }, r.meta.revision ? { "if-match": String(r.meta.revision) } : undefined);
      toast(said); reload();
    } catch (e) { toast.error((e as Error).message); }
  };
  return (
    <>
      <Pressed size="sm" variant={r.spec?.disabled ? "secondary" : "destructive"} disabled={me} title={me ? "Not your own account" : r.spec?.disabled ? "Let them sign in again" : "They cannot sign in; bindings stay, live sessions end"}
        onPress={async () => {
          if (!r.spec?.disabled && !confirm(`Disable ${idOf(r)}? They cannot sign in and their sessions end. Their bindings stay for when they are enabled again.`)) return;
          await flip({ disabled: !r.spec?.disabled }, r.spec?.disabled ? `${idOf(r)} may sign in again.` : `${idOf(r)} is disabled.`);
        }}>{r.spec?.disabled ? "Enable" : "Disable"}</Pressed>
      <Pressed size="sm" variant="secondary" disabled={me} title={r.spec?.cellAdmin ? "Take cell operator away" : "Everything, everywhere — the provider's own role"}
        onPress={async () => {
          if (!confirm(r.spec?.cellAdmin ? `${idOf(r)} stops being a cell operator?` : `Make ${idOf(r)} a cell operator? That is everything, everywhere, including every tenant's data.`)) return;
          await flip({ cellAdmin: !r.spec?.cellAdmin }, r.spec?.cellAdmin ? `${idOf(r)} is a tenant account now.` : `${idOf(r)} is a cell operator.`);
        }}>{r.spec?.cellAdmin ? "Revoke cell operator" : "Make cell operator"}</Pressed>
    </>
  );
}
