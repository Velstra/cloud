// A press that waits says so. Same contract as the current console's `btn()`:
// disables, says the verb in the present tense, comes back either way, and
// ignores a stale completion when a second press replaced the first.

import { useRef, useState } from "react";
import { Button } from "@/components/ui/button";

const VERB = /^(Create|Save|Add|Refresh|Delete|Remove|Start|Stop|Attach|Detach|Migrate|Explain|Issue|Report|Drain|Reboot|Apply)\b/;
export const presentTense = (label: string) =>
  (VERB.exec(label)?.[0] ?? "Working").replace(/e?$/, "") + "ing…";

export function Pressed({ children, onPress, busyLabel, ...rest }:
  React.ComponentProps<typeof Button> & { onPress: () => Promise<unknown> | unknown; busyLabel?: string }) {
  const [busy, setBusy] = useState(false);
  const run = useRef(0);
  return (
    <Button {...rest} disabled={busy || rest.disabled} aria-busy={busy || undefined}
      onClick={async (e) => {
        e.preventDefault();
        const answer = onPress();
        if (!answer || typeof (answer as Promise<unknown>).then !== "function") return;
        const mine = ++run.current;
        setBusy(true);
        try { await answer; } catch { /* the caller shows it */ }
        finally { if (mine === run.current) setBusy(false); }
      }}>
      {busy && <span className="inline-block size-3 animate-spin rounded-full border-2 border-current border-r-transparent" />}
      {busy ? (busyLabel ?? presentTense(String(children))) : children}
    </Button>
  );
}
