// An explanation the API gives, drawn as the answer it is rather than as the
// JSON it came in. "Why is this not placed?" is a list of machines, each with
// the reason it said no; "what fits?" is a set of numbers with units.

import { humanise } from "@/lib/model";

type Rejected = { node: string; why: string; detail?: string };

export function Explain({ answer }: { answer: unknown }) {
  if (answer == null) return null;
  if (typeof answer !== "object") return <p className="text-sm">{String(answer)}</p>;
  const a = answer as Record<string, unknown>;

  // Placement: where it went, or every machine that turned it away and why.
  if ("rejected" in a && Array.isArray(a.rejected)) {
    const placed = a.placed as string | null;
    const rejected = a.rejected as Rejected[];
    return (
      <div className="grid gap-2">
        <p className="text-sm" style={{ color: placed ? "var(--settled)" : "var(--failing)" }}>
          {placed ? `Placed on ${placed}.` : `Not placed. ${rejected.length} ${rejected.length === 1 ? "machine" : "machines"} said no:`}
        </p>
        {rejected.length > 0 && (
          <table className="w-full text-xs" style={{ tableLayout: "auto" }}>
            <thead><tr style={{ color: "var(--text-faint)" }}>
              <th className="pb-1 text-left font-medium">Machine</th><th className="pb-1 text-left font-medium">Refused because</th><th className="pb-1 text-left font-medium">Detail</th>
            </tr></thead>
            <tbody>
              {rejected.map((x) => (
                <tr key={x.node} className="border-t" style={{ borderColor: "var(--border-subtle)" }}>
                  <td className="py-1.5 font-mono"><a href={`#/c/nodes/${encodeURIComponent(x.node)}`} style={{ color: "var(--brand)" }}>{x.node}</a></td>
                  <td className="py-1.5" style={{ color: "var(--failing)" }}>{humanise(x.why)}</td>
                  <td className="py-1.5" style={{ color: "var(--text-muted)" }}>{x.detail ?? ""}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
    );
  }

  // Anything else that is flat: keys and values, numbers with their units read
  // off the key. Anything nested: shown as it came, but not pretended to be
  // understood.
  const flat = Object.entries(a).filter(([, v]) => v == null || typeof v !== "object");
  const nested = Object.entries(a).filter(([, v]) => v != null && typeof v === "object");
  return (
    <div className="grid gap-3">
      {flat.length > 0 && (
        <dl className="grid grid-cols-[180px_1fr] gap-x-7 gap-y-1.5 text-xs">
          {flat.map(([k, v]) => (
            <div key={k} className="contents">
              <dt style={{ color: "var(--text-muted)" }}>{humanise(k)}</dt>
              <dd className="font-mono" style={{ color: "var(--text-body)" }}>{v == null ? "—" : String(v)}</dd>
            </div>
          ))}
        </dl>
      )}
      {nested.map(([k, v]) => (
        <details key={k} className="text-xs">
          <summary className="cursor-pointer" style={{ color: "var(--text-muted)" }}>{humanise(k)}</summary>
          <pre className="mt-1 overflow-x-auto rounded-[4px] p-3 font-mono" style={{ background: "var(--surface-sunken)" }}>{JSON.stringify(v, null, 1)}</pre>
        </details>
      ))}
    </div>
  );
}
