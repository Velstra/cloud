// What a verdict is, and how a value reads. Ported from the current console's
// model.js so the two can never disagree about whether something has settled.

import { at, type Collection, type Column } from "./schema";

export type Verdict = "settled" | "drifting" | "failing" | "unreported" | "deleting";

export type Resource = {
  meta: {
    name: string;
    uid?: string;
    generation?: number;
    revision?: string;
    createdAt?: number;
    deletedAt?: number | null;
    labels?: Record<string, string>;
    finalizers?: string[];
  };
  spec?: Record<string, any>;
  status?: Record<string, any> & {
    observedGeneration?: number;
    conditions?: { kind: string; status: string; reason?: string; message?: string; at?: number }[];
  };
};

export const idOf = (r: Resource) => r.meta.name.split("/").pop() ?? r.meta.name;
export const nameOf = (r: Resource) => r.meta.name;

export const VERDICT_ORDER: Record<Verdict, number> = {
  failing: 0, drifting: 1, unreported: 2, deleting: 3, settled: 4,
};

export const VERDICT_WORD: Record<Verdict, string> = {
  settled: "Settled", drifting: "Applying", failing: "Failing",
  unreported: "Not reported", deleting: "Deleting",
};

export function verdict(r: Resource, c?: Collection): {
  kind: Verdict; word: string; reason?: string; detail?: string; busy?: boolean;
} {
  // The same rules, in the same order, as the console this is compared with.
  if (c && c.condition === "") return { kind: "settled", word: "Recorded" };
  if (r.status?.done === true) {
    return { kind: "settled", word: "Finished", detail: r.status?.error ? String(r.status.error) : undefined };
  }
  const named = c?.condition || "Ready";
  const ready = (r.status?.conditions ?? []).find((x) => x.kind === named);
  if (r.meta.deletedAt) return { kind: "deleting", word: "Deleting", busy: true };
  const gen = Number(r.meta.generation ?? 0);
  const obs = Number(r.status?.observedGeneration ?? 0);
  // Failing only when the refusal is about *this* generation: a stale False
  // from before the last edit is not a verdict on what was just asked for.
  const decided = ready && ready.status === "False" &&
    Number((ready as any).observedGeneration ?? 0) === gen;
  if (decided) return { kind: "failing", word: "Failing", reason: ready!.reason, detail: ready!.message };
  // A guest that nothing has reported on yet is being made, and "not reported"
  // reads as a fault to somebody arriving from a cloud that says "pending". The
  // word says the verb; the kind stays honest.
  //
  // **`Unknown` is not a state, it is the absence of one.** `InstanceStatus`
  // defaults its `state` to the string `"Unknown"`, and a truthiness test on it
  // is therefore always true — so a guest one second old read as "reported, and
  // nothing is happening", the screen stopped following it, and a person
  // watched "Not reported / Unknown" until they reloaded the page. That is the
  // opposite of what this branch is for.
  const reported = !!r.status?.state && r.status.state !== "Unknown";
  const fresh = !reported && (Date.now() - Number(r.meta.createdAt ?? 0)) < 15 * 60_000;
  if (obs === 0) return fresh ? { kind: "unreported", word: "Creating", busy: true } : { kind: "unreported", word: "Not reported" };
  if (obs < gen) return { kind: "drifting", word: underway(r) || "Applying", busy: true, reason: ready?.reason };
  if (!ready) return { kind: "unreported", word: "Not reported" };
  if (ready.status === "True") return { kind: "settled", word: "Settled" };
  return { kind: "drifting", word: underway(r) || "Applying", busy: true, reason: ready.reason, detail: ready.message };
}

/** The verb, when what was asked for and what is are both known and differ. */
export function underway(r: Resource): string {
  const asked = r.spec?.desiredState, is = r.status?.state;
  if (!asked || !is || asked === is) return "";
  if (asked === "Stopped") return "Stopping";
  if (asked === "Running") return is === "Stopped" ? "Starting" : "Restarting";
  return "";
}

/** Every condition that says no, with its sentence — not only the one the
 *  verdict was read from. A guest can be "wanted Running, is Stopped" because
 *  a *different* condition, HostActions, says the image's bytes were wrong. */
export const refusals = (r: Resource) =>
  (r.status?.conditions ?? []).filter((c) => c.status === "False" && (c.message || c.reason));

/** Where what was asked for and what is disagree. */
export const disagreements = (r: Resource, c: Collection) =>
  c.agreements
    .map((a) => ({ ...a, askedValue: at(r.spec, a.asked), isValue: at(r.status, a.is) }))
    .filter((a) => a.askedValue !== undefined && a.isValue !== undefined &&
      String(a.askedValue) !== String(a.isValue));

// ---- formatting -----------------------------------------------------------

export const ago = (ms?: number) => {
  if (!ms) return "—";
  const s = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (s < 60) return `${s}s ago`;
  if (s < 3600) return `${Math.round(s / 60)}m ago`;
  if (s < 86400) return `${Math.round(s / 3600)}h ago`;
  return `${Math.round(s / 86400)}d ago`;
};

export const bytes = (n?: number) => {
  if (n == null) return "—";
  const u = ["B", "KiB", "MiB", "GiB", "TiB"];
  let i = 0; let v = n;
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
  return `${v < 10 && i ? v.toFixed(1) : Math.round(v)} ${u[i]}`;
};

export const number = (n: unknown) =>
  n == null || n === "" ? "—" : Number(n).toLocaleString("en-US");

/**
 * A resource name is long and its tail is what identifies it.
 *
 * The tail is what is shown; the whole name goes on the element for a pointer
 * and a screen reader, so nothing is actually hidden.
 */
export const shortName = (value: unknown) => {
  const s = String(value ?? "");
  return s.includes("/") ? s.split("/").slice(-2).join("/") : s;
};

/**
 * A cell's text — from the **column**, not from its tag alone.
 *
 * The tag was all this took, and so seventeen number columns dropped the unit
 * the schema gives them (a `100` beside another `100` standing for GiB and
 * MiB), and every boolean column read "yes"/"no" instead of its own words.
 * Those words are the object's vocabulary and they are not interchangeable
 * with a verdict: a node taken out of scheduling reads "draining", which is
 * the single thing somebody scans that board for.
 *
 * `yes` and `count` are answered before the blank check, for the same reason
 * the other console exempts them: `false` and "none" are answers, and an
 * em-dash in their place says "unknown" about something that is known.
 */
export function cellText(col: Column, v: unknown): string {
  switch (col.cell) {
    case "yes": return v ? col.yes : col.no;
    case "count": return String(Array.isArray(v) ? v.length : v == null || v === "" ? 0 : v);
    default: break;
  }
  if (v === undefined || v === null || v === "") return "—";
  switch (col.cell) {
    case "ago": return ago(Number(v));
    case "bytes": return bytes(Number(v));
    case "number": return number(v) + (col.unit ? " " + col.unit : "");
    case "mono": return shortName(v);
    default: return Array.isArray(v) ? v.join(", ") : typeof v === "object" ? JSON.stringify(v) : String(v);
  }
}

/** The label map the other console carries, for spec/status keys. */
export const humanise = (key: string) =>
  key.replace(/([a-z])([A-Z])/g, "$1 $2").replace(/_/g, " ").replace(/^./, (c) => c.toUpperCase())
    .replace(/\bMib\b/, "MiB").replace(/\bGib\b/, "GiB").replace(/\bVcpus\b/, "vCPUs")
    .replace(/\bCidr\b/, "CIDR").replace(/\bMtu\b/, "MTU").replace(/\bSsh\b/, "SSH")
    .replace(/\bIp\b/, "IP").replace(/\bDns\b/, "DNS").replace(/\bId\b/, "ID");
