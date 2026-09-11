// "Are you sure?" — in this console's own words and its own vessel.
//
// It was `window.confirm`, fourteen times. Every one of those sentences was
// already well written; what was wrong was the box around them. A native
// dialog is unthemed, blocks the event loop, cannot be reached through the DOM
// by a test, and — on the bulk path — counts the objects where it should name
// them. Naming them is the difference between "delete 12?" and seeing `db-1`
// in the list and stopping.
//
// One mounted dialog, driven by a promise, so a call site reads the way the
// old one did: `if (!(await ask({...}))) return;`.

import { createContext, useCallback, useContext, useRef, useState, type ReactNode } from "react";
import { Button } from "@/components/ui/button";
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle,
} from "@/components/ui/dialog";

export type Question = {
  title: string;
  /** The consequence, in the words the call site already had. */
  body?: ReactNode;
  confirmLabel?: string;
  /** `danger` for anything that destroys something; nothing here is undoable. */
  tone?: "danger" | "normal";
};

type Asker = (q: Question) => Promise<boolean>;

const AskContext = createContext<Asker>(async () => false);

/** Ask, and wait for the answer. `false` when it is dismissed. */
export const useAsk = () => useContext(AskContext);

export function AskProvider({ children }: { children: ReactNode }) {
  const [open, setOpen] = useState(false);
  const [q, setQ] = useState<Question | null>(null);
  // The pending answer. Held in a ref because the dialog's own close handler
  // has to be able to answer `false` for a dismissal it did not cause.
  const answer = useRef<((yes: boolean) => void) | null>(null);

  const settle = useCallback((yes: boolean) => {
    setOpen(false);
    const reply = answer.current;
    answer.current = null;
    reply?.(yes);
  }, []);

  const ask = useCallback<Asker>((question) => {
    // A second question while one is open answers the first with "no" rather
    // than leaving its caller waiting for ever.
    answer.current?.(false);
    setQ(question);
    setOpen(true);
    return new Promise<boolean>((resolve) => { answer.current = resolve; });
  }, []);

  return (
    <AskContext.Provider value={ask}>
      {children}
      <Dialog open={open} onOpenChange={(next) => { if (!next) settle(false); }}>
        <DialogContent className="max-w-md">
          <DialogHeader>
            <DialogTitle>{q?.title}</DialogTitle>
            {/* The description renders a `<p>`, so only a sentence goes in it.
                A body that is markup — the list of what is about to go —
                stands beside it, or the browser splits the paragraph and the
                list ends up outside the dialog's own layout. */}
            {typeof q?.body === "string" && <DialogDescription className="text-xs">{q.body}</DialogDescription>}
          </DialogHeader>
          {q?.body && typeof q.body !== "string" && (
            <div className="text-xs" style={{ color: "var(--text-muted)" }}>{q.body}</div>
          )}
          <DialogFooter>
            <Button variant="ghost" size="sm" onClick={() => settle(false)}>Cancel</Button>
            <Button size="sm" variant={q?.tone === "danger" ? "destructive" : "default"}
              autoFocus onClick={() => settle(true)}>
              {q?.confirmLabel ?? "Yes"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </AskContext.Provider>
  );
}

/**
 * A list of what is about to happen to, named rather than counted.
 *
 * Five and then a count: a dialog that scrolls is one nobody reads, and the
 * first five are enough to notice the one that should not be in there.
 */
export function Named({ ids }: { ids: string[] }) {
  const shown = ids.slice(0, 5);
  return (
    <div className="mt-1">
      <ul className="grid gap-0.5 font-mono text-xs" style={{ color: "var(--text-body)" }}>
        {shown.map((id) => <li key={id}>{id}</li>)}
      </ul>
      {ids.length > shown.length && (
        <p className="mt-1 text-xs" style={{ color: "var(--text-faint)" }}>
          and {ids.length - shown.length} more
        </p>
      )}
    </div>
  );
}
