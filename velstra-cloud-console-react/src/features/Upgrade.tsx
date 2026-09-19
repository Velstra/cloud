// Moving machines to a release, and the medium that installs one from nothing.
//
// Two buttons, both of which start with the same question — which release —
// answered from the releases the cell holds. Neither shows a token: an upgrade
// makes a rollout object, and a medium is a download the browser takes to a
// file, so the one credential involved never touches this page.

import { useRef } from "react";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import { call } from "@/api/transport";
import { idOf, nameOf, type Resource } from "@/lib/model";
import { go } from "@/app/router";
import { useAsk } from "./Ask";
import { Pressed } from "./Pressed";

type Release = Resource & {
  status?: {
    version?: string;
    installer?: { fetched?: boolean };
    conditions?: { kind: string; status: string }[];
  };
};

const ready = (r: Release) =>
  (r.status?.conditions ?? []).some((c) => c.kind === "Ready" && c.status === "True");

async function releases(): Promise<Release[]> {
  const answer = await call("list:releases", "GET", "/api/v1/releases");
  return (answer?.items ?? []) as Release[];
}

/** The release picker inside a question. A ref, because a dialog answers yes
 *  or no and the choice has to survive the answer. */
function ReleasePick({ options, chosen }: { options: Release[]; chosen: React.MutableRefObject<string> }) {
  return (
    <select
      className="ml-2 rounded-[4px] border px-2 py-1 text-sm"
      style={{ background: "var(--surface)", borderColor: "var(--border)", color: "var(--text-strong)" }}
      defaultValue={chosen.current}
      onChange={(e) => { chosen.current = e.target.value; }}
    >
      {options.map((r) => (
        <option key={nameOf(r)} value={idOf(r)}>{idOf(r)} — {r.status?.version ?? "version not read yet"}</option>
      ))}
    </select>
  );
}

/** A rollout for the picked machines: one object, and the Rollouts board to watch it on. */
export function UpgradeButton({ picked, onDone }: { picked: string[]; onDone: () => void }) {
  const ask = useAsk();
  const chosen = useRef("");
  const evacuate = useRef(true);
  return (
    <Button size="sm" variant="secondary" onClick={async () => {
      try {
        const all = (await releases()).filter(ready);
        if (!all.length) {
          toast.error("No release is ready on this cell. Add one under Releases — the channel a published version was downloaded from — and wait until it is fetched.");
          return;
        }
        chosen.current = idOf(all[all.length - 1]);
        const names = picked.map((n) => n.split("/").pop()!);
        const yes = await ask({
          title: `Upgrade ${names.length} ${names.length === 1 ? "machine" : "machines"}?`,
          body: (
            <div className="grid gap-2 text-sm">
              <p>{names.join(", ")} — one at a time, the control plane last. A machine that does not come back stops the rollout by name and leaves the rest as they were.</p>
              <label>Release<ReleasePick options={all} chosen={chosen} /></label>
              <label className="flex items-center gap-2">
                <input type="checkbox" defaultChecked onChange={(e) => { evacuate.current = e.target.checked; }} />
                move guests off each machine first
              </label>
            </div>
          ),
          confirmLabel: "Upgrade",
        });
        if (!yes) return;
        const id = `upgrade-${Date.now().toString(36)}`;
        await call("create:rollouts", "POST", "/api/v1/rollouts", undefined, {
          id,
          spec: { release: chosen.current, nodes: names, evacuate: evacuate.current },
        });
        toast(`Rollout ${id} started.`);
        onDone();
        go({ view: "board", coll: "rollouts", id });
      } catch (e) { toast.error((e as Error).message); }
    }}>Upgrade…</Button>
  );
}

/** The installer ISO with this node's join file on its tail: one download, one stick. */
export function InstallMediumButton({ node }: { node: string }) {
  const ask = useAsk();
  const chosen = useRef("");
  return (
    <Pressed size="sm" variant="secondary"
      title="The installer with this machine's join file on it. Write it to a stick, boot the machine from it, and it joins as this node without anything typed."
      onPress={async () => {
        try {
          const all = (await releases()).filter((r) => r.status?.installer?.fetched);
          if (!all.length) {
            toast.error("No release on this cell holds an installer yet. Add one under Releases and wait until it is fetched.");
            return;
          }
          chosen.current = idOf(all[all.length - 1]);
          if (all.length > 1) {
            const yes = await ask({
              title: `Install medium for ${node}`,
              body: <label className="text-sm">Release<ReleasePick options={all} chosen={chosen} /></label>,
              confirmLabel: "Cut it",
            });
            if (!yes) return;
          }
          const cut = await call("installMedium-nodes", "POST", `/api/v1/nodes/${encodeURIComponent(node)}:installMedium`, undefined, { release: chosen.current });
          // A plain navigation: the answer is `attachment`, so the browser saves
          // it and stays here. Two gigabytes never pass through this page.
          window.location.assign(cut.url);
          toast(`${cut.filename} — write it to a stick with dd or Etcher and boot the machine from it; it joins as ${node}. The link works once.`);
        } catch (e) { toast.error((e as Error).message); }
      }}>Install medium…</Pressed>
  );
}
