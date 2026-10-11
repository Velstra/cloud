import type { Resource } from "@/lib/model";
import { bytes } from "@/lib/model";

export function usageTime(r: Resource): string {
  const at = Number(r.spec?.at);
  if (!Number.isFinite(at) || at <= 0) return "Unknown time";
  const date = new Date(at);
  return Number.isNaN(date.getTime()) ? "Unknown time" : date.toISOString().slice(0, 16).replace("T", " ") + " UTC";
}

const value = (n: unknown, unit = "") => typeof n === "number" && Number.isFinite(n)
  ? n.toLocaleString() + (unit ? ` ${unit}` : "") : "—";
const traffic = (n: unknown) => typeof n === "number" && Number.isFinite(n) ? bytes(n) : "—";

export function Usage({ r }: { r: Resource }) {
  const used = r.spec?.used ?? {};
  const totals = r.spec?.traffic ?? {};
  const primary = [
    ["Instances", value(used.instances)], ["vCPUs", value(used.vcpus)],
    ["Memory", value(used.memoryMib, "MiB")], ["Volumes", value(used.volumes)],
    ["Storage", value(used.volumeGib, "GiB")], ["Public IPs", value(used.floatingIps)],
  ];
  const additional = [
    ["Load balancers", value(used.loadBalancers)], ["Devices", value(used.devices)],
    ["Snapshots", value(used.snapshots)], ["Snapshot storage", value(used.snapshotGib, "GiB")],
    ["Backups", value(used.backups)], ["Backup storage", value(used.backupGib, "GiB")],
    ["Traffic in", traffic(totals.rxBytes)], ["Traffic out", traffic(totals.txBytes)],
  ];
  const readings = (items: string[][]) => <dl className="grid grid-cols-2 gap-x-6 gap-y-3 text-sm">
    {items.map(([label, amount]) => <div key={label} className="min-w-0">
      <dt className="text-xs text-muted-foreground">{label}</dt>
      <dd className="mt-1 break-words font-medium tabular-nums">{amount}</dd>
    </div>)}
  </dl>;
  return <div className="grid gap-4">
    {readings(primary)}
    <details className="text-sm"><summary>More usage</summary><div className="mt-3">{readings(additional)}</div></details>
  </div>;
}
