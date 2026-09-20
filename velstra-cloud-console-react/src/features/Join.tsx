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
import { call, token } from "@/api/transport";
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
export function MintedBox({ minted, what, resource }: { minted: Minted; what: string; resource?: Resource }) {
  const join = minted.joinToken;
  const bare = minted.nodeToken ?? minted.poolToken;
  const spec = resource?.spec ?? {};
  const backend = String(spec.backend ?? "External").toLowerCase();
  const target = String(spec.backendTarget ?? "");
  const setup = minted.poolToken ? [
    `VELSTRA_POOL=${idOf(resource ?? ({ meta: { name: minted.target ?? "pool" } } as Resource))}`,
    backend === "ceph" ? `VELSTRA_POOL_BACKEND=ceph\nVELSTRA_CEPH_POOL=${target}`
      : backend === "lvm" ? `VELSTRA_POOL_BACKEND=lvm\nVELSTRA_LVM_GROUP=${target}${spec.thinPool ? `\nVELSTRA_LVM_THIN_POOL=${spec.thinPool}` : ""}`
        : backend === "directory" ? `VELSTRA_POOL_BACKEND=directory\nVELSTRA_POOL_DIR=${target}`
          : "# Keep the backend already configured for this agent",
  ].join("\n") : "";
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
      {setup && <div className="grid gap-1"><div className="text-[11px] font-semibold uppercase tracking-[0.07em]" style={{ color: "var(--text-muted)" }}>Pool agent configuration</div><pre className="overflow-x-auto rounded bg-background p-2 font-mono text-xs">{setup}</pre></div>}
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
/**
 * The same credential, as a file somebody can carry to the machine.
 *
 * A join token is about 1.3 KB of base64. Pasting it is fine into a shell and
 * impossible at a console, which is where an installer runs — so the platform
 * answers with the artefact instead of the string: `velstra/join` on a stick
 * the installer finds by itself, or a `#cloud-config` for a machine that boots
 * Debian or Ubuntu.
 *
 * Downloaded rather than shown. It is a credential, and a credential in a
 * `<pre>` is one somebody screenshots; it is also the wrong shape to read —
 * what you do with it is put it on a medium, and this hands you the file to
 * put there.
 */
function MediumButton({
  node,
  verb,
  filename,
  label,
  title,
}: {
  node: string;
  verb: string;
  filename: string;
  label: string;
  title: string;
}) {
  return (
    <Pressed
      size="sm"
      variant="secondary"
      title={title}
      onPress={async () => {
        try {
          const r = await fetch(`/api/v1/nodes/${encodeURIComponent(node)}:${verb}`, {
            method: "POST",
            headers: { ...(token() ? { authorization: "Bearer " + token() } : {}) },
          });
          const text = await r.text();
          if (!r.ok) {
            // The API's refusal is JSON even when the success is not.
            let said = text;
            try {
              said = JSON.parse(text)?.error?.message ?? text;
            } catch {
              /* it was not JSON; the body is the sentence */
            }
            toast.error(said);
            return;
          }
          const url = URL.createObjectURL(new Blob([text], { type: "text/plain" }));
          const a = document.createElement("a");
          a.href = url;
          a.download = filename;
          a.click();
          URL.revokeObjectURL(url);
          toast(`${filename} — it is a credential; anything holding it can register as ${node}.`);
        } catch (e) {
          toast.error((e as Error).message);
        }
      }}
    >
      {label}
    </Pressed>
  );
}

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
      {c.id === "nodes" && (
        <>
          <MediumButton
            node={idOf(r)}
            verb="joinFile"
            filename={`${idOf(r)}.join`}
            label="Join file"
            title="The token as the file the installer looks for. Put it at velstra/join on any medium you plug into the machine and it offers it by name — no typing."
          />
          <MediumButton
            node={idOf(r)}
            verb="cloudInit"
            filename={`${idOf(r)}-cloud-init.yaml`}
            label="cloud-init"
            title="The same token as a #cloud-config, for a machine that boots Debian or Ubuntu: it writes the join file, runs the installer against it, and shreds it."
          />
        </>
      )}
      {minted && <div className="basis-full"><MintedBox minted={minted} what={`the ${noun}`} resource={r} /></div>}
    </>
  );
}
