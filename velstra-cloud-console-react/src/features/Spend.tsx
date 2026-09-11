// What this project used, for the month.
//
// The API has summed it all along — `projects/{p}:explainUsage` adds the
// hourly readings the way a bill does — and the only way to it from here was
// a generic "Explain usage" button on a *project's* detail, which is an
// operator screen. A customer, whose bill it is, had no route to it at all.
//
// Two things this screen does that a flat key/value dump cannot. It picks the
// month, because the API takes one and only this month was ever visible. And
// it puts `hours` next to `hoursInMonthSoFar`, which is the one thing the
// answer says about its own trustworthiness: a cell that was down took no
// readings, and those hours are missing from the sum rather than invented. Two
// unrelated rows in a table hide exactly that.

import { useEffect, useState } from "react";
import { call, ApiError } from "@/api/transport";
import { useStore } from "@/app/store";
import { ALL } from "@/lib/schema";
import { bytes, number } from "@/lib/model";

type Usage = {
  month: string;
  hours: number;
  hoursInMonthSoFar: number;
  vcpuHours: number;
  memoryGibHours: number;
  volumeGibHours: number;
  instanceHours: number;
  floatingIpHours: number;
  loadBalancerHours: number;
  deviceHours: number;
  snapshotGibHours: number;
  backupGibHours: number;
  rxBytes: number;
  txBytes: number;
};

/** The last twelve months, newest first, as the API spells them. */
function months(): string[] {
  const out: string[] = [];
  const d = new Date();
  for (let i = 0; i < 12; i++) {
    out.push(`${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}`);
    d.setMonth(d.getMonth() - 1);
  }
  return out;
}

export function Spend() {
  const project = useStore((s) => s.project);
  const choices = months();
  const [month, setMonth] = useState(choices[0]);
  const [u, setU] = useState<Usage | null>(null);
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    if (!project || project === ALL) { setLoading(false); return; }
    setLoading(true); setError("");
    call("explainUsage", "GET", `/api/v1/projects/${encodeURIComponent(project)}:explainUsage`, { month })
      .then((x) => setU(x))
      .catch((e) => { setU(null); setError(e instanceof ApiError ? e.message : String(e)); })
      .finally(() => setLoading(false));
  }, [project, month]);

  if (!project || project === ALL) {
    return <p className="text-sm" style={{ color: "var(--text-muted)" }}>Choose one project at the top; a bill is per project.</p>;
  }

  // Every hour the month has held so far that took no reading. The gap is the
  // headline, not a footnote: it is what says whether the rest is complete.
  const missing = u ? Math.max(0, u.hoursInMonthSoFar - u.hours) : 0;
  const complete = u ? u.hoursInMonthSoFar === 0 || u.hours >= u.hoursInMonthSoFar : false;

  const lines: [string, string, string][] = u ? [
    ["Guests", `${number(u.instanceHours)} hours`, "one guest running for one hour is one"],
    ["vCPU", `${number(u.vcpuHours)} hours`, "vCPUs summed hour by hour"],
    ["Memory", `${number(u.memoryGibHours)} GiB-hours`, ""],
    ["Volumes", `${number(u.volumeGibHours)} GiB-hours`, "what was provisioned, not what was written"],
    ["Snapshots", `${number(u.snapshotGibHours)} GiB-hours`, ""],
    ["Backups", `${number(u.backupGibHours)} GiB-hours`, ""],
    ["Public addresses", `${number(u.floatingIpHours)} hours`, "held, whether or not anything used them"],
    ["Load balancers", `${number(u.loadBalancerHours)} hours`, ""],
    ["Passed-through devices", `${number(u.deviceHours)} hours`, ""],
    ["Traffic in", bytes(u.rxBytes), "summed, never differenced — a gap is a gap, not a spike"],
    ["Traffic out", bytes(u.txBytes), ""],
  ] : [];

  return (
    <div className="grid gap-4">
      <div className="flex flex-wrap items-baseline gap-3">
        <h1 className="text-[26px] font-bold leading-tight" style={{ color: "var(--text-strong)" }}>Spend</h1>
        <p className="text-sm" style={{ color: "var(--text-muted)" }}>
          What <span className="font-mono">{project}</span> used, summed from the hourly readings.
        </p>
        <label className="ml-auto flex items-center gap-2 text-xs" style={{ color: "var(--text-muted)" }}>
          Month
          <select value={month} onChange={(e) => setMonth(e.target.value)}
            className="rounded-[3px] border px-2 py-1 text-xs"
            style={{ borderColor: "var(--border)", background: "var(--surface)", color: "var(--text-body)" }}>
            {choices.map((m) => <option key={m} value={m}>{m}</option>)}
          </select>
        </label>
      </div>

      {loading && <p className="text-sm" style={{ color: "var(--text-muted)" }}>Adding it up…</p>}
      {error && <p className="text-sm" style={{ color: "var(--failing)" }}>{error}</p>}

      {u && (
        <>
          <div className="rounded-[4px] border px-4 py-3"
            style={{ borderColor: complete ? "var(--border)" : "var(--drifting)", background: "var(--surface-sunken)" }}>
            <p className="text-xs" style={{ color: complete ? "var(--text-muted)" : "var(--drifting)" }}>
              {complete
                ? `${number(u.hours)} of ${number(u.hoursInMonthSoFar)} hours read — the month is complete so far.`
                : `${number(u.hours)} of ${number(u.hoursInMonthSoFar)} hours read. ${number(missing)} ${missing === 1 ? "hour" : "hours"} took no reading, so those are missing from every number below rather than estimated.`}
            </p>
          </div>

          <table className="w-full text-sm">
            <tbody>
              {lines.map(([label, value, note]) => (
                <tr key={label} className="border-t" style={{ borderColor: "var(--border-subtle)" }}>
                  <td className="py-2 pr-4 align-top" style={{ color: "var(--text-body)" }}>
                    {label}
                    {note && <div className="text-[11px]" style={{ color: "var(--text-faint)" }}>{note}</div>}
                  </td>
                  <td className="py-2 text-right align-top font-mono tabular-nums" style={{ color: "var(--text-strong)" }}>{value}</td>
                </tr>
              ))}
            </tbody>
          </table>

          <p className="text-[11px]" style={{ color: "var(--text-faint)" }}>
            Each reading is one hour at what the reading says — the same arithmetic a bill uses.
            The readings themselves are on the Usage board.
          </p>
        </>
      )}
    </div>
  );
}
