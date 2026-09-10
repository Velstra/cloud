// The verdict, as a word and a dot. Colour is never the only carrier, and a
// dot that pulses is one whose object is being worked on right now.

import { verdict, type Resource } from "@/lib/model";
import type { Collection } from "@/lib/schema";

export function State({ of, coll, detail }: { of: Resource; coll?: Collection; detail?: boolean }) {
  const v = verdict(of, coll);
  const tone = v.kind === "unreported" || v.kind === "deleting" ? "var(--text-muted)" : `var(--${v.kind})`;
  const dot = v.kind === "unreported" || v.kind === "deleting" ? undefined : `var(--dot-${v.kind})`;
  return (
    <span className="inline-flex items-center gap-2 whitespace-nowrap text-xs" style={{ color: tone }}>
      <span className={"size-2 shrink-0 rounded-full" + (v.busy ? " breathing" : "")}
        style={dot ? { background: dot } : { border: "1px solid currentColor" }} />
      {v.word}
      {detail && v.reason && <span style={{ color: "var(--text-faint)" }}>· {v.reason}</span>}
    </span>
  );
}
