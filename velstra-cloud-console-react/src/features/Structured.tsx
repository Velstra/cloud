// The lists that are not lists of strings: firewall rules, load-balancer
// listeners, Ceph pools. Each is a shape the wire fixes exactly — a rule's
// ports are `{from,to}` and its remote is one of `{cidr}` or `{group}`, a
// listener's back-end port is `memberPort`, a Ceph pool's name is `pool` —
// so each gets an editor that writes that shape. A generic row of text boxes
// guessing the key names writes fields the API does not have, which serde
// drops in silence: a listener meant for 443 → 8080 arrives as 443 → 443.

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";

type Row = Record<string, any>;
const Cell = ({ label, children }: { label: string; children: React.ReactNode }) => (
  <label className="grid gap-1">
    <span className="text-[11px]" style={{ color: "var(--text-muted)" }}>{label}</span>
    {children}
  </label>
);
const Select = ({ value, onChange, disabled, options }: { value: string; onChange: (v: string) => void; disabled?: boolean; options: [string, string][] }) => (
  <select disabled={disabled} value={value} onChange={(e) => onChange(e.target.value)}
    className="h-9 rounded-[4px] border px-2 text-sm" style={{ borderColor: "var(--border)", background: "var(--surface)", color: "var(--text-body)" }}>
    {options.map(([v, l]) => <option key={v} value={v}>{l}</option>)}
  </select>
);
const num = (v: string) => (v === "" ? undefined : Number(v));

function Rows({ value, onChange, disabled, add, addLabel, columns, render }: {
  value: Row[]; onChange: (v: Row[]) => void; disabled: boolean; add: () => Row; addLabel: string;
  columns: string; render: (row: Row, set: (patch: Row) => void) => React.ReactNode;
}) {
  const rows = value ?? [];
  return (
    <div className="grid gap-2">
      {rows.map((row, i) => (
        <div key={i} className="grid items-end gap-2" style={{ gridTemplateColumns: columns }}>
          {render(row, (patch) => onChange(rows.map((r, j) => (j === i ? { ...r, ...patch } : r))))}
          <Button type="button" variant="ghost" size="sm" disabled={disabled} onClick={() => onChange(rows.filter((_, j) => j !== i))}>Remove</Button>
        </div>
      ))}
      <div><Button type="button" variant="secondary" size="sm" disabled={disabled} onClick={() => onChange([...rows, add()])}>{addLabel}</Button></div>
    </div>
  );
}

/** A security group's rules: direction, protocol, port range, and where from. */
export function RuleList({ value, onChange, disabled }: { value: Row[]; onChange: (v: unknown) => void; disabled: boolean }) {
  return (
    <>
      <Rows value={value} onChange={onChange} disabled={disabled} addLabel="Add a rule"
        columns="minmax(0,0.9fr) minmax(0,0.9fr) minmax(0,0.7fr) minmax(0,0.7fr) minmax(0,1.1fr) minmax(0,1.4fr) auto"
        add={() => ({ direction: "ingress", protocol: "tcp", ports: { from: 443, to: 443 }, remote: { cidr: "0.0.0.0/0" } })}
        render={(row, set) => {
          const remoteKind = row.remote && "group" in row.remote ? "group" : "cidr";
          const ports = row.ports ?? {};
          return (
            <>
              <Cell label="Direction"><Select disabled={disabled} value={row.direction ?? "ingress"} onChange={(v) => set({ direction: v })} options={[["ingress", "ingress — into the guest"], ["egress", "egress — out of it"]]} /></Cell>
              <Cell label="Protocol"><Select disabled={disabled} value={row.protocol ?? "tcp"} onChange={(v) => set({ protocol: v, ports: v === "tcp" || v === "udp" ? (row.ports ?? { from: 443, to: 443 }) : undefined })}
                options={[["tcp", "tcp"], ["udp", "udp"], ["icmp", "icmp"], ["any", "any"]]} /></Cell>
              <Cell label="Port from"><Input disabled={disabled || !["tcp", "udp"].includes(row.protocol ?? "tcp")} type="number" value={ports.from ?? ""} className="text-sm"
                onChange={(e) => set({ ports: { ...ports, from: num(e.target.value), to: ports.to ?? num(e.target.value) } })} /></Cell>
              <Cell label="to"><Input disabled={disabled || !["tcp", "udp"].includes(row.protocol ?? "tcp")} type="number" value={ports.to ?? ""} className="text-sm"
                onChange={(e) => set({ ports: { ...ports, to: num(e.target.value) } })} /></Cell>
              <Cell label="From"><Select disabled={disabled} value={remoteKind} onChange={(v) => set({ remote: v === "cidr" ? { cidr: "0.0.0.0/0" } : { group: "" } })}
                options={[["cidr", "an address range"], ["group", "a security group"]]} /></Cell>
              <Cell label={remoteKind === "cidr" ? "Range" : "Group"}>
                <Input disabled={disabled} className="font-mono text-sm" placeholder={remoteKind === "cidr" ? "10.0.0.0/8" : "projects/p/security-groups/web"}
                  value={(remoteKind === "cidr" ? row.remote?.cidr : row.remote?.group) ?? ""}
                  onChange={(e) => set({ remote: remoteKind === "cidr" ? { cidr: e.target.value } : { group: e.target.value } })} />
              </Cell>
            </>
          );
        }} />
      <p className="mt-1 text-[11px]" style={{ color: "var(--text-faint)" }}>
        The datapath programs each rule as written: <code>any</code>, and tcp/udp without a port range, are accepted here and refused when the port is programmed. Give a protocol and a range.
      </p>
    </>
  );
}

/** A load balancer's listeners: what the client connects to, and where it goes. */
export function ListenerList({ value, onChange, disabled }: { value: Row[]; onChange: (v: unknown) => void; disabled: boolean }) {
  return (
    <Rows value={value} onChange={onChange} disabled={disabled} addLabel="Add a listener"
      columns="minmax(0,0.8fr) minmax(0,1fr) minmax(0,1fr) auto"
      add={() => ({ protocol: "tcp", port: 443, memberPort: 443 })}
      render={(row, set) => (
        <>
          <Cell label="Protocol"><Select disabled={disabled} value={row.protocol ?? "tcp"} onChange={(v) => set({ protocol: v })} options={[["tcp", "tcp"], ["udp", "udp"]]} /></Cell>
          <Cell label="Listens on"><Input disabled={disabled} type="number" className="text-sm" value={row.port ?? ""} onChange={(e) => set({ port: num(e.target.value) })} /></Cell>
          <Cell label="Reaches the member on"><Input disabled={disabled} type="number" className="text-sm" placeholder="same as above" value={row.memberPort ?? ""} onChange={(e) => set({ memberPort: num(e.target.value) })} /></Cell>
        </>
      )} />
  );
}

/** A Ceph cluster's pools: the RBD name, and how many copies. */
export function PoolList({ value, onChange, disabled }: { value: Row[]; onChange: (v: unknown) => void; disabled: boolean }) {
  return (
    <Rows value={value} onChange={onChange} disabled={disabled} addLabel="Add a pool"
      columns="minmax(0,1.4fr) minmax(0,0.8fr) minmax(0,0.8fr) auto"
      add={() => ({ pool: "", size: 3, minSize: 2 })}
      render={(row, set) => (
        <>
          <Cell label="Pool"><Input disabled={disabled} className="font-mono text-sm" placeholder="velstra-volumes" value={row.pool ?? ""} onChange={(e) => set({ pool: e.target.value })} /></Cell>
          <Cell label="Copies"><Input disabled={disabled} type="number" className="text-sm" value={row.size ?? ""} onChange={(e) => set({ size: num(e.target.value) })} /></Cell>
          <Cell label="Least, to serve"><Input disabled={disabled} type="number" className="text-sm" value={row.minSize ?? ""} onChange={(e) => set({ minSize: num(e.target.value) })} /></Cell>
        </>
      )} />
  );
}

