// What the guest is handed on first boot, three ways: the basics as a form
// that writes the `#cloud-config` for you, the file as it is, or a file from
// disk. All three edit the same value — the schema's `userData` — so nothing
// is stored twice and the raw view always shows exactly what will be sent.

import { useMemo, useRef, useState } from "react";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { Textarea } from "@/components/ui/textarea";
import { Button } from "@/components/ui/button";

type Basics = {
  hostname: string; user: string; password: string; sudo: boolean;
  sshKeys: string; packages: string; runcmd: string; timezone: string; updates: boolean;
};

const EMPTY: Basics = { hostname: "", user: "", password: "", sudo: true, sshKeys: "", packages: "", runcmd: "", timezone: "", updates: false };

/** A YAML string, quoted only where cloud-init would otherwise misread it. */
const y = (s: string) => (/^[A-Za-z0-9_./:-]+$/.test(s) ? s : JSON.stringify(s));
const lines = (s: string) => s.split("\n").map((x) => x.trim()).filter(Boolean);

export function render(b: Basics): string {
  const out = ["#cloud-config"];
  if (b.hostname) out.push(`hostname: ${y(b.hostname)}`, `manage_etc_hosts: true`);
  if (b.timezone) out.push(`timezone: ${y(b.timezone)}`);
  if (b.user) {
    out.push("users:", `  - name: ${y(b.user)}`, "    shell: /bin/bash");
    if (b.sudo) out.push("    sudo: ALL=(ALL) NOPASSWD:ALL");
    if (b.password) out.push("    lock_passwd: false");
    const keys = lines(b.sshKeys);
    if (keys.length) { out.push("    ssh_authorized_keys:"); for (const k of keys) out.push(`      - ${y(k)}`); }
  } else if (lines(b.sshKeys).length) {
    out.push("ssh_authorized_keys:"); for (const k of lines(b.sshKeys)) out.push(`  - ${y(k)}`);
  }
  if (b.password && b.user) out.push("chpasswd:", "  expire: false", "  users:", `    - name: ${y(b.user)}`, `      password: ${y(b.password)}`, "      type: text", "ssh_pwauth: true");
  if (b.updates) out.push("package_update: true", "package_upgrade: true");
  const pk = lines(b.packages);
  if (pk.length) { out.push("packages:"); for (const p of pk) out.push(`  - ${y(p)}`); }
  const rc = lines(b.runcmd);
  if (rc.length) { out.push("runcmd:"); for (const c of rc) out.push(`  - ${JSON.stringify(c)}`); }
  return out.join("\n") + "\n";
}

export function CloudInit({ value, onChange, disabled }: { value: string; onChange: (v: string) => void; disabled?: boolean }) {
  const [b, setB] = useState<Basics>(EMPTY);
  const [tab, setTab] = useState<string>(value ? "raw" : "basics");
  const file = useRef<HTMLInputElement>(null);
  const generated = useMemo(() => render(b), [b]);
  const set = <K extends keyof Basics>(k: K, v: Basics[K]) => { const next = { ...b, [k]: v }; setB(next); onChange(render(next)); };
  const size = new TextEncoder().encode(value ?? "").length;

  return (
    <Tabs value={tab} onValueChange={setTab} className="flex w-full flex-col gap-1">
      <div className="flex items-center gap-3">
        <TabsList>
          <TabsTrigger value="basics">Basics</TabsTrigger>
          <TabsTrigger value="raw">The file</TabsTrigger>
          <TabsTrigger value="upload">From disk</TabsTrigger>
        </TabsList>
        <span className="ml-auto text-[11px]" style={{ color: "var(--text-faint)" }}>{size ? `${size.toLocaleString()} bytes` : "nothing yet"}</span>
      </div>

      <TabsContent value="basics">
        <div className="grid gap-4 pt-3">
          <p className="text-xs" style={{ color: "var(--text-muted)" }}>Answer what you know; the file is written as you type and can be read under <em>The file</em>.</p>
          <div className="grid grid-cols-2 gap-4">
            <Field label="Hostname"><Input disabled={disabled} value={b.hostname} onChange={(e) => set("hostname", e.target.value)} placeholder="web-4" /></Field>
            <Field label="Time zone"><Input disabled={disabled} value={b.timezone} onChange={(e) => set("timezone", e.target.value)} placeholder="Europe/Berlin" /></Field>
            <Field label="User"><Input disabled={disabled} value={b.user} onChange={(e) => set("user", e.target.value)} placeholder="admin" /></Field>
            <Field label="Password" help="Sent in clear in the file; prefer a key."><Input disabled={disabled} type="password" value={b.password} onChange={(e) => set("password", e.target.value)} /></Field>
          </div>
          <div className="flex items-center gap-3 text-xs" style={{ color: "var(--text-muted)" }}>
            <Switch checked={b.sudo} onCheckedChange={(v) => set("sudo", !!v)} disabled={disabled || !b.user} /> may use sudo without a password
            <span className="mx-2" />
            <Switch checked={b.updates} onCheckedChange={(v) => set("updates", !!v)} disabled={disabled} /> update and upgrade packages on first boot
          </div>
          <Field label="SSH public keys" help="One per line."><Textarea disabled={disabled} rows={2} className="font-mono text-xs" value={b.sshKeys} onChange={(e) => set("sshKeys", e.target.value)} placeholder="ssh-ed25519 AAAA… name@host" /></Field>
          <div className="grid grid-cols-2 gap-4">
            <Field label="Packages" help="One per line."><Textarea disabled={disabled} rows={3} className="font-mono text-xs" value={b.packages} onChange={(e) => set("packages", e.target.value)} placeholder={"nginx\ncurl"} /></Field>
            <Field label="Commands to run once" help="One per line, after packages."><Textarea disabled={disabled} rows={3} className="font-mono text-xs" value={b.runcmd} onChange={(e) => set("runcmd", e.target.value)} placeholder="systemctl enable --now nginx" /></Field>
          </div>
        </div>
      </TabsContent>

      <TabsContent value="raw">
        <div className="grid gap-2 pt-3">
          <Textarea disabled={disabled} rows={14} className="font-mono text-xs" value={value ?? ""} onChange={(e) => onChange(e.target.value)}
            placeholder={"#cloud-config\nhostname: web-4\n…"} spellCheck={false} />
          <div className="flex items-center gap-2 text-[11px]" style={{ color: "var(--text-faint)" }}>
            {value && !/^#cloud-config|^#!|^Content-Type: multipart/.test(value.trimStart()) && <span style={{ color: "var(--drifting)" }}>Does not start with <code>#cloud-config</code> or <code>#!</code> — cloud-init will ignore it.</span>}
            {value && value !== generated && tab === "raw" && <span>Edited by hand; the Basics tab no longer describes it.</span>}
            <Button type="button" size="sm" variant="ghost" className="ml-auto" disabled={disabled} onClick={() => onChange("")}>Clear</Button>
          </div>
        </div>
      </TabsContent>

      <TabsContent value="upload">
        <div className="grid gap-3 pt-3">
          <p className="text-xs" style={{ color: "var(--text-muted)" }}>A <code>#cloud-config</code>, a shell script, or a MIME multipart. Read into the file above; nothing is sent until you create the {"guest"}.</p>
          <input ref={file} type="file" accept=".yaml,.yml,.txt,.sh,.cfg,text/*" className="hidden" onChange={async (e) => {
            const f = e.target.files?.[0]; if (!f) return;
            onChange(await f.text()); setTab("raw"); e.target.value = "";
          }} />
          <div className="flex items-center gap-3">
            <Button type="button" variant="secondary" disabled={disabled} onClick={() => file.current?.click()}>Choose a file…</Button>
            <span className="text-[11px]" style={{ color: "var(--text-faint)" }}>Up to 16 KiB is what most images accept.</span>
          </div>
          <div className="rounded-[4px] border border-dashed p-6 text-center text-xs" style={{ borderColor: "var(--border-strong)", color: "var(--text-faint)" }}
            onDragOver={(e) => e.preventDefault()}
            onDrop={async (e) => { e.preventDefault(); const f = e.dataTransfer.files?.[0]; if (f) { onChange(await f.text()); setTab("raw"); } }}>
            …or drop it here
          </div>
        </div>
      </TabsContent>
    </Tabs>
  );
}

function Field({ label, help, children }: { label: string; help?: string; children: React.ReactNode }) {
  return (
    <div className="grid gap-1.5">
      <Label className="text-xs" style={{ color: "var(--text-strong)" }}>{label}</Label>
      {children}
      {help && <p className="text-[11px]" style={{ color: "var(--text-faint)" }}>{help}</p>}
    </div>
  );
}
