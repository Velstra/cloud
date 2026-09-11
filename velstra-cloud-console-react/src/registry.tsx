// Where a collection is allowed to be special.
//
// Everything generic lives in the features; this is the one file that knows a
// collection by name. A custom cell, an extra action, an extra panel on the
// detail — registered here, and the generic code asks. Adding a bespoke surface
// (a console, a map) is adding an entry, not editing a renderer.

import type { ReactNode } from "react";
import type { Resource } from "@/lib/model";
import type { Collection } from "@/lib/schema";
import operations from "@/api/operations.json";
import { lazy, Suspense } from "react";
const Terminal = lazy(() => import("@/features/Terminal").then((m) => ({ default: m.Terminal })));
const Screen = lazy(() => import("@/features/Screen").then((m) => ({ default: m.Screen })));
const CloudInit = lazy(() => import("@/features/CloudInit").then((m) => ({ default: m.CloudInit })));
const Ceph = lazy(() => import("@/features/Ceph").then((m) => ({ default: m.Ceph })));
const InstanceQuick = lazy(() => import("@/features/Quick").then((m) => ({ default: m.InstanceQuick })));
const VolumeQuick = lazy(() => import("@/features/Quick").then((m) => ({ default: m.VolumeQuick })));
const Connect = lazy(() => import("@/features/Quick").then((m) => ({ default: m.Connect })));
const NodeQuick = lazy(() => import("@/features/Quick").then((m) => ({ default: m.NodeQuick })));
const Members = lazy(() => import("@/features/Members").then((m) => ({ default: m.Members })));
const RuleList = lazy(() => import("@/features/Structured").then((m) => ({ default: m.RuleList })));
const ListenerList = lazy(() => import("@/features/Structured").then((m) => ({ default: m.ListenerList })));
const PoolList = lazy(() => import("@/features/Structured").then((m) => ({ default: m.PoolList })));
const DiskList = lazy(() => import("@/features/Structured").then((m) => ({ default: m.DiskList })));
const GrantsEditor = lazy(() => import("@/features/Members").then((m) => ({ default: m.GrantsEditor })));
const Quota = lazy(() => import("@/features/Quota").then((m) => ({ default: m.Quota })));
const Account = lazy(() => import("@/features/Account").then((m) => ({ default: m.Account })));
const UserQuick = lazy(() => import("@/features/Account").then((m) => ({ default: m.UserQuick })));

export type Action = {
  id: string;           // operationId
  label: string;        // the verb, short: "Explain placement"
  summary: string;      // what the API says it does, for the tooltip
  method: string;
  path: string;         // with {project} / {name}
  destructive?: boolean;
  needsBody?: boolean;
};

export type Panel = {
  id: string;
  title: string;
  sub?: string;
  render: (r: Resource, c: Collection, reload: () => void) => ReactNode;
};

export type FieldEditor = (p: { value: any; onChange: (v: any) => void; disabled: boolean }) => ReactNode;

type Entry = {
  cells?: Record<string, (r: Resource) => ReactNode>;
  actions?: Action[];
  panels?: Panel[];
  fieldEditors?: Record<string, FieldEditor>;
  /** Buttons in the object's head, before the API's own verbs: the edits
   *  somebody reaches for first (start, stop, attach, back up), which are
   *  spec changes rather than verbs and so cannot be found in the OpenAPI. */
  quick?: (r: Resource, c: Collection, reload: () => void) => ReactNode;
};

const custom: Record<string, Entry> = {};

export const register = (coll: string, entry: Entry) => { custom[coll] = { ...custom[coll], ...entry }; };
export const entry = (coll: string): Entry => custom[coll] ?? {};

// ---- actions the API documents, found rather than written ---------------
//
// `POST …/{name}:explainPlacement` is an action on one object; `…/nodes:explainCapacity`
// is an action on the collection. Both come out of the OpenAPI document, so a
// new one on the API is a new button here without anyone writing the button.

type Op = { id: string; method: string; path: string; summary: string; tags: string[]; body: boolean };

const verb = (id: string) => {
  const m = /:([a-zA-Z]+)$/.exec(id) ?? /([a-zA-Z]+)$/.exec(id);
  const w = (m?.[1] ?? id).replace(/([a-z])([A-Z])/g, "$1 $2");
  return w.charAt(0).toUpperCase() + w.slice(1);
};

const collectionOf = (path: string): string | null => {
  const m = /\/api\/v1\/(?:projects\/\{project\}\/)?([a-z-]+)(?:\/\{[a-z]+\})?(?::|$)/.exec(path);
  return m?.[1] ?? null;
};

/**
 * Verbs that are in the OpenAPI and are not buttons.
 *
 * The list is scraped from the document, which is right — a verb the API grows
 * should reach the console without anybody remembering. What was wrong is that
 * it was scraped *whole*:
 *
 * * `:reportStatus` is how a **node agent** writes what it observed. It refuses
 *   any token that is not an agent's, so it was a button nobody could ever
 *   press — drawn on a tenant's own guest, where pressing it gave a 403 about
 *   an endpoint they had never heard of.
 * * `:issueCredential` mints a registration token that is shown once and stored
 *   only as a hash. It belongs behind a deliberate flow that shows the token
 *   and says so, not on a row of grey buttons beside "Explain placement".
 */
const NOT_A_BUTTON = /:(reportStatus|issueCredential)$/;

/** Verbs only a cell operator may ask. The API says so; this keeps the console
 *  from offering what it would refuse. */
const OPERATORS_ONLY = /:(explainPlacement|explainMigration|explainRecovery|explainMaintenance|explainCapacity|explainCpu)$/;

const shown = (o: Op, cellAdmin: boolean) =>
  !NOT_A_BUTTON.test(o.path) && (cellAdmin || !OPERATORS_ONLY.test(o.path));

export function objectActions(coll: string, cellAdmin = true): Action[] {
  return (operations as Op[])
    .filter((o) => o.path.includes(":") && /\{(name|id)\}:/.test(o.path) && collectionOf(o.path) === coll)
    .filter((o) => shown(o, cellAdmin))
    .map((o) => ({
      id: o.id, label: verb(o.path), summary: o.summary, method: o.method, path: o.path,
      needsBody: o.body, destructive: /delete|revoke|drain|evacuate|reboot/i.test(o.path),
    }));
}

export function collectionActions(coll: string, cellAdmin = true): Action[] {
  return (operations as Op[])
    .filter((o) => /[a-z]:[a-zA-Z]+$/.test(o.path) && !/\{(name|id)\}:/.test(o.path) && collectionOf(o.path) === coll)
    .filter((o) => shown(o, cellAdmin))
    .map((o) => ({ id: o.id, label: verb(o.path), summary: o.summary, method: o.method, path: o.path, needsBody: o.body }));
}

// ---- the bespoke surfaces, declared honestly ----------------------------

const loading = (what: string) => <p className="text-xs" style={{ color: "var(--text-faint)" }}>Loading {what}…</p>;

register("instances", {
  quick: (r, c, reload) => <Suspense fallback={null}><InstanceQuick r={r} c={c} reload={reload} /></Suspense>,
  panels: [
    {
      id: "connect", title: "Connect", sub: "its addresses, and the line that logs in",
      render: (r) => <Suspense fallback={loading("the addresses")}><Connect r={r} /></Suspense>,
    },
    {
      id: "screen", title: "Screen", sub: "the guest's display, over VNC",
      render: (r, c) => <Suspense fallback={loading("the screen")}><Screen r={r} coll={c} /></Suspense>,
    },
    {
      id: "console", title: "Console", sub: "the guest's serial line — attach to read and type",
      render: (r, c) => <Suspense fallback={loading("the terminal")}><Terminal r={r} coll={c} /></Suspense>,
    },
  ],
  fieldEditors: {
    userData: (p) => <Suspense fallback={loading("the editor")}><CloudInit {...p} /></Suspense>,
  },
});

register("projects", {
  panels: [
    {
      id: "members", title: "Members", sub: "who may do what here",
      render: (r, c, reload) => <Suspense fallback={loading("the members")}><Members r={r} coll={c} reload={reload} /></Suspense>,
    },
    {
      id: "quota", title: "Limits and use", sub: "what it may use, what it has, and the largest guest that could start",
      render: (r) => <Suspense fallback={loading("the limits")}><Quota project={r.meta.name.split("/").pop() ?? ""} /></Suspense>,
    },
  ],
});

register("users", {
  quick: (r, c, reload) => <Suspense fallback={null}><UserQuick r={r} c={c} reload={reload} /></Suspense>,
  panels: [{
    id: "account", title: "Access", sub: "where it is bound, its password, its tokens",
    render: (r) => <Suspense fallback={loading("the account")}><Account r={r} /></Suspense>,
  }],
});

register("roles", {
  fieldEditors: {
    grants: (p) => <Suspense fallback={loading("the editor")}><GrantsEditor {...p} /></Suspense>,
  },
});

register("security-groups", {
  fieldEditors: { rules: (p) => <Suspense fallback={loading("the rules")}><RuleList {...p} /></Suspense> },
});

register("load-balancers", {
  fieldEditors: { listeners: (p) => <Suspense fallback={loading("the listeners")}><ListenerList {...p} /></Suspense> },
});

register("ceph-clusters", {
  fieldEditors: {
    pools: (p) => <Suspense fallback={loading("the pools")}><PoolList {...p} /></Suspense>,
    osds: (p) => <Suspense fallback={loading("the disks")}><DiskList {...p} /></Suspense>,
  },
});

register("nodes", {
  quick: (r, c, reload) => <Suspense fallback={null}><NodeQuick r={r} c={c} reload={reload} /></Suspense>,
});

register("volumes", {
  quick: (r, c, reload) => <Suspense fallback={null}><VolumeQuick r={r} c={c} reload={reload} /></Suspense>,
});

register("ceph-clusters", {
  panels: [{
    id: "cluster", title: "The cluster", sub: "asked for, beside what is up",
    render: (r, c, reload) => <Suspense fallback={loading("the cluster")}><Ceph r={r} coll={c} reload={reload} /></Suspense>,
  }],
});

register("migrations", {
  panels: [{
    id: "transfer", title: "Transfer", sub: "what was copied",
    render: (r) => (
      <pre className="overflow-x-auto rounded-[4px] p-3 font-mono text-xs"
        style={{ background: "var(--surface-sunken)" }}>
        {JSON.stringify(r.status?.transfer ?? r.status ?? {}, null, 1)}
      </pre>
    ),
  }],
});

register("networks", {
  panels: [{
    id: "map", title: "Map", sub: "this network among the others",
    render: () => <a href="#/map" className="text-xs underline-offset-2 hover:underline" style={{ color: "var(--brand)" }}>Open the map →</a>,
  }],
});
