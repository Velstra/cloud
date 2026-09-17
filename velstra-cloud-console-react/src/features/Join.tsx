// The join token, shown once: what a new machine pastes to become part of
// this cell.
//
// One box for two moments — right after a node or pool is created, and later
// from the object's own page when the first token was lost. Both are the same
// thing shown the same way: a warning that it will not be shown again, the
// string, and a copy button. The API keeps only a digest, so "shown once" is a
// fact about the platform, not a policy of this page.
//
// This used to be worse than the old console: a create answers `202 {
// operation, target, nodeToken }`, and nothing read the third field, so the
// token shown once was shown to nobody.

import { useState } from "react";
import { toast } from "sonner";
import { Copy } from "lucide-react";
import { Button } from "@/components/ui/button";
import { call } from "@/api/transport";
import { idOf, type Resource } from "@/lib/model";
import type { Collection } from "@/lib/schema";
import { Pressed } from "./Pressed";

/** The fields a registration answers with. Any of them may be absent. */
export type Minted = { joinToken?: string; nodeToken?: string; poolToken?: string; target?: string };

export function hasMinted(a: unknown): a is Minted {
  return !!a && typeof a === "object" && ("joinToken" in a || "nodeToken" in a || "poolToken" in a);
}

const copy = (value: string) => navigator.clipboard?.writeText(value).then(() => toast("Copied."));

/** What a freshly registered machine is told, laid out to be copied. */
export function MintedBox({ minted, what }: { minted: Minted; what: string }) {
  const join = minted.joinToken;
  const bare = minted.nodeToken ?? minted.poolToken;
  return (
    <div className="grid gap-2 rounded-[4px] border p-3" style={{ borderColor: "var(--drifting)", background: "var(--surface-sunken)" }}>
      <div className="text-xs" style={{ color: "var(--drifting)" }}>
        Shown once. Copy it now — the cell keeps only a digest and cannot show it again. If it is lost, mint another from {what}'s page; the old one keeps working until you revoke it.
      </div>
      {join && (
        <div className="grid gap-1">
          <div className="flex items-center gap-2 text-[11px] font-semibold uppercase tracking-[0.07em]" style={{ color: "var(--text-muted)" }}>
            Join token
            <span className="font-normal normal-case tracking-normal" style={{ color: "var(--text-faint)" }}>
              — paste into the installer, or <code className="font-mono">velstra-cloud-node setup --join …</code>
            </span>
            <Button size="icon-xs" variant="ghost" className="ml-auto" title="Copy the join token" onClick={() => copy(join)}><Copy className="size-3" /></Button>
          </div>
          <code className="max-h-24 overflow-y-auto break-all font-mono text-xs" style={{ color: "var(--text-body)" }}>{join}</code>
        </div>
      )}
      {bare && (
        <div className="grid gap-1">
          <div className="flex items-center gap-2 text-[11px] font-semibold uppercase tracking-[0.07em]" style={{ color: "var(--text-muted)" }}>
            {minted.nodeToken ? "Node token" : "Pool token"}
            <span className="font-normal normal-case tracking-normal" style={{ color: "var(--text-faint)" }}>— the bare credential, for a seed file or <code className="font-mono">VELSTRA_TOKEN</code></span>
            <Button size="icon-xs" variant="ghost" className="ml-auto" title="Copy the token" onClick={() => copy(bare)}><Copy className="size-3" /></Button>
          </div>
          <code className="break-all font-mono text-xs" style={{ color: "var(--text-body)" }}>{bare}</code>
        </div>
      )}
    </div>
  );
}

/**
 * Mint a fresh credential for a machine that already exists, and show it.
 *
 * `:issueCredential` is deliberately not one of the grey action buttons — the
 * registry hides it — because a token shown in a 300-character toast is a
 * token nobody can copy. This is the deliberate flow that comment asks for.
 * Issuing is additive: the credential the machine holds keeps working until
 * it is revoked, so a mistyped paste never takes an agent down.
 */
export function JoinTokenButton({ r, c }: { r: Resource; c: Collection }) {
  const [minted, setMinted] = useState<Minted | null>(null);
  const noun = c.id === "pools" ? "pool" : "node";
  return (
    <>
      <Pressed size="sm" variant="secondary"
        title={`Mint a join token for this ${noun}. The one it has keeps working until revoked.`}
        onPress={async () => {
          try {
            const a = await call(`issue:${c.id}`, "POST", `/api/v1/${c.id}/${encodeURIComponent(idOf(r))}:issueCredential`, undefined, {});
            if (hasMinted(a)) { setMinted(a); toast("Minted — copy it now."); }
            else toast.error("The cell answered without a token.");
          } catch (e) { toast.error((e as Error).message); }
        }}>Join token</Pressed>
      {minted && <div className="basis-full"><MintedBox minted={minted} what={`the ${noun}`} /></div>}
    </>
  );
}
