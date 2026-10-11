import type { Resource } from "@/lib/model";

const size = (value: unknown) => typeof value === "number" && Number.isFinite(value)
  ? `${value.toLocaleString()} GiB` : "—";

export function Volume({ r }: { r: Resource }) {
  const current = r.status?.actualSizeGib;
  const requested = r.spec?.sizeGib;
  return <dl aria-label="Volume storage" className="grid grid-cols-2 gap-x-6 gap-y-3 text-sm">
    <div>
      <dt className="text-xs text-muted-foreground">Capacity</dt>
      <dd className="mt-1 font-medium tabular-nums">{r.status?.provisioned ? size(current) : "Not allocated"}</dd>
      {current !== requested && <dd className="mt-1 text-xs text-muted-foreground">Requested: {size(requested)}</dd>}
    </div>
    <div className="min-w-0">
      <dt className="text-xs text-muted-foreground">Storage pool</dt>
      <dd className="mt-1 break-words font-medium">{r.status?.pool || r.spec?.pool || "Not assigned"}</dd>
    </div>
  </dl>;
}
