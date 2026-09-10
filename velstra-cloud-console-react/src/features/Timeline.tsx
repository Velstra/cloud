// Convergence as a process, not a word: what was asked at which generation,
// what has been observed, and what each condition said and when. Read left
// to right, it is the object's story so far.

import { ago, verdict, type Resource } from "@/lib/model";
import type { Collection } from "@/lib/schema";

export function Timeline({ r, coll }: { r: Resource; coll: Collection }) {
  const v = verdict(r, coll);
  const gen = Number(r.meta.generation ?? 0);
  const obs = Number(r.status?.observedGeneration ?? 0);
  const conds = (r.status?.conditions ?? []) as { kind: string; status: string; reason?: string; message?: string; lastTransition?: number; observedGeneration?: number }[];

  const steps: { label: string; sub?: string; tone: string; when?: number; filled: boolean }[] = [
    { label: `Asked · generation ${gen}`, sub: r.meta.createdAt ? `created ${ago(r.meta.createdAt)}` : undefined, tone: "var(--brand)", filled: true },
    obs
      ? { label: `Observed · generation ${obs}`, sub: obs < gen ? `${gen - obs} behind` : "caught up", tone: obs < gen ? "var(--dot-drifting)" : "var(--text-faint)", filled: true }
      : { label: "Observed · nothing yet", sub: "no agent has reported", tone: "var(--border-strong)", filled: false },
    ...conds.map((c) => ({
      label: `${c.kind} · ${c.status}`,
      sub: [c.reason, c.lastTransition ? ago(c.lastTransition) : ""].filter(Boolean).join(" · "),
      tone: c.status === "True" ? "var(--dot-settled)" : c.status === "False" ? "var(--dot-failing)" : "var(--dot-drifting)",
      when: c.lastTransition, filled: true,
    })),
  ];

  return (
    <ol className="relative grid gap-0 pl-4" aria-label="Convergence so far">
      <span className="absolute left-[5px] top-2 bottom-2 w-px" style={{ background: "var(--border)" }} aria-hidden />
      {steps.map((s, i) => (
        <li key={i} className="relative py-1.5 text-xs">
          <span className="absolute -left-4 top-[9px] size-[11px] rounded-full border-2"
            style={{ borderColor: s.tone, background: s.filled ? s.tone : "var(--surface)" }} aria-hidden />
          <div style={{ color: "var(--text-body)" }}>{s.label}</div>
          {s.sub && <div style={{ color: "var(--text-faint)" }}>{s.sub}</div>}
        </li>
      ))}
      <li className="relative py-1.5 text-xs font-medium" style={{ color: v.kind === "unreported" ? "var(--text-muted)" : `var(--${v.kind})` }}>
        <span className="absolute -left-4 top-[9px] size-[11px] rounded-full" style={{ background: v.kind === "unreported" ? "var(--border-strong)" : `var(--dot-${v.kind})` }} aria-hidden />
        So: {v.word.toLowerCase()}{v.reason ? ` — ${v.reason}` : ""}
      </li>
    </ol>
  );
}
