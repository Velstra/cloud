// A press that waits says so. Same contract as the current console's `btn()`:
// disables, says the verb in the present tense, comes back either way, and
// ignores a stale completion when a second press replaced the first.

import { useRef, useState } from "react";
import { Button } from "@/components/ui/button";

const VERB = /^(Create|Save|Add|Refresh|Delete|Remove|Start|Stop|Attach|Detach|Migrate|Explain|Issue|Report|Drain|Reboot|Apply)\b/;

/// The two verbs the `-e` rule gets wrong on its own.
///
/// `Stop` doubles its consonant. Everything else in the list above is either
/// "drop a trailing e" (Save, Create, Migrate, Delete, Remove, Issue) or "add
/// nothing" (Start, Add, Drain, Apply), which the rule already does.
const IRREGULAR: Record<string, string> = { Stop: "Stopping" };

export const presentTense = (label: string) => {
  const verb = VERB.exec(label)?.[0];
  // The fallback is already a present participle and must not be put through
  // the rule: doing so produced "Workinging…", which is what the sign-in
  // button said while it was signing somebody in — the first words anybody
  // reads in this console.
  if (!verb) return "Working…";
  return (IRREGULAR[verb] ?? verb.replace(/e?$/, "") + "ing") + "…";
};

export function Pressed({ children, onPress, busyLabel, ...rest }:
  React.ComponentProps<typeof Button> & { onPress: () => Promise<unknown> | unknown; busyLabel?: string }) {
  const [busy, setBusy] = useState(false);
  const pending = useRef(false);
  const run = useRef(0);
  return (
    <Button {...rest} disabled={busy || rest.disabled} aria-busy={busy || undefined}
      onClick={async (e) => {
        e.preventDefault();
        if (pending.current) return;
        pending.current = true;
        const mine = ++run.current;
        setBusy(true);
        try { await onPress(); } catch { /* the caller shows it */ }
        finally { if (mine === run.current) { pending.current = false; setBusy(false); } }
      }}>
      <span className="relative inline-grid place-items-center">
        <span className={busy ? "invisible inline-flex items-center gap-1.5" : "inline-flex items-center gap-1.5"}>{children}</span>
        {busy && <span className="absolute inset-0 flex items-center justify-center" role="status" aria-label={busyLabel ?? presentTense(String(children))}><span className="size-4 animate-spin rounded-full border-2 border-current border-r-transparent" /></span>}
      </span>
    </Button>
  );
}
