// What a project may use and what it has used, read from `:explainQuota` —
// which also says the one thing a limit table never does: the largest guest
// that could actually start now, and what bounds it.

import { useEffect, useState } from "react";
import { call } from "@/api/transport";
import { bytes, humanise } from "@/lib/model";
import { useStore } from "@/app/store";

type Dim = { name: string; used: number; limit: number; left: number | null; unlimited: boolean; exhausted: boolean };
type Largest = { none: boolean; vcpus?: number; memoryMib?: number; memoryLimitedBy?: string; vcpusLimitedBy?: string };
type Answer = { dimensions: Dim[]; largestStartable?: Largest };

const fmt = (name: string, n: number) => name === "memoryMib" ? bytes(n * 1024 * 1024) : name === "volumeGib" ? bytes(n * 1024 ** 3) : String(n);
const label = (name: string) => ({ memoryMib: "Memory", volumeGib: "Volume space", floatingIps: "Public IPs", loadBalancers: "Load balancers", vcpus: "vCPU" } as Record<string, string>)[name] ?? humanise(name);

export function useQuota(project: string) {
  const [q, setQ] = useState<Answer | null>(null); const [err, setErr] = useState("");
  useEffect(() => {
    if (!project || project === "*") { setQ(null); return; }
    call("explainQuota", "GET", `/api/v1/projects/${encodeURIComponent(project)}:explainQuota`).then(setQ).catch((e) => setErr((e as Error).message));
  }, [project]);
  return { q, err };
}

export function QuotaBars({ q, compact }: { q: Answer; compact?: boolean }) {
  const dims = compact ? q.dimensions.filter((d) => ["instances", "vcpus", "memoryMib", "volumeGib"].includes(d.name)) : q.dimensions;
  const l = q.largestStartable;
  return (
    <div className="grid gap-3">
      <div className={`grid gap-x-6 gap-y-2 ${compact ? "grid-cols-2 lg:grid-cols-4" : "grid-cols-1 md:grid-cols-2"}`}>
        {dims.map((d) => {
          const p = d.unlimited || !d.limit ? null : Math.min(100, Math.round((d.used / d.limit) * 100));
          const colour = d.exhausted ? "var(--failing)" : p != null && p >= 80 ? "var(--drifting)" : "var(--brand)";
          return (
            <div key={d.name} className="grid gap-1 text-xs">
              <div className="flex items-baseline justify-between">
                <span style={{ color: "var(--text-muted)" }}>{label(d.name)}</span>
                <span className="font-mono tabular-nums" style={{ color: d.exhausted ? "var(--failing)" : "var(--text-body)" }}>
                  {fmt(d.name, d.used)}{d.unlimited ? <span style={{ color: "var(--text-faint)" }}> · no limit</span> : <span style={{ color: "var(--text-faint)" }}> / {fmt(d.name, d.limit)}{p != null ? ` · ${p}%` : ""}</span>}
                </span>
              </div>
              <div className="h-1.5 overflow-hidden rounded-full" style={{ background: "var(--border-strong)" }}>
                <div className="h-full rounded-full" style={{ width: `${p ?? (d.used ? 8 : 0)}%`, background: d.unlimited ? "var(--text-faint)" : colour }} />
              </div>
            </div>
          );
        })}
      </div>
      {l && (
        <p className="text-xs" style={{ color: l.none ? "var(--failing)" : "var(--text-muted)" }}>
          {l.none ? "Nothing more could start right now." : <>Largest guest that could start now: <span className="font-mono" style={{ color: "var(--text-body)" }}>{l.vcpus} vCPU · {bytes((l.memoryMib ?? 0) * 1024 * 1024)}</span>{l.memoryLimitedBy ? ` — memory limited by the ${l.memoryLimitedBy}` : ""}{l.vcpusLimitedBy && l.vcpusLimitedBy !== l.memoryLimitedBy ? `, vCPU by the ${l.vcpusLimitedBy}` : ""}.</>}
        </p>
      )}
    </div>
  );
}

export function Quota({ project }: { project: string }) {
  const who = useStore((s) => s.who);
  const { q, err } = useQuota(project);
  if (err) return <p className="text-xs" style={{ color: "var(--failing)" }}>{err}</p>;
  if (!q) return <p className="text-xs" style={{ color: "var(--text-faint)" }}>Reading…</p>;
  return (
    <div className="grid gap-3">
      <QuotaBars q={q} />
      <p className="text-[11px]" style={{ color: "var(--text-faint)" }}>
        {who?.cellAdmin ? "Limits are set under Edit; zero means no limit." : "Limits are the cell's to set — ask your operator for more room."}
      </p>
    </div>
  );
}
