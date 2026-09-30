// What has happened to this object: what was asked for, and what was refused.
//
// Two collections answer it and neither is the object itself. `operations` is
// what the platform accepted — who asked, for what, and whether it finished.
// `audit` is what it wrote down — every change with its author, and every
// refusal carrying *the same sentence the person was given*.
//
// Reading only the first is how "I clicked it and nothing happened" goes in
// circles: the accepted list is empty precisely because the thing was refused,
// and the refusal is the answer. So both are here, in one column, newest
// first.

import { useEffect, useState } from "react";
import { call } from "@/api/transport";
import { ago, nameOf, type Resource } from "@/lib/model";
import { collection, basePath, projectOf } from "@/lib/schema";

type Entry = {
  at: number;
  who: string;
  what: string;
  detail: string;
  /** Which of the two it came from, for the marker down the left. */
  kind: "accepted" | "failed" | "refused" | "changed";
};

/** Records about one object, both kinds, newest first.
 *
 * The API filters by target (`?target=…`) for exactly these two collections,
 * so the browser is not handed a cell's worth of audit to sift. */
async function historyOf(name: string): Promise<{ entries: Entry[]; warning: string }> {
  const project = projectOf(name) ?? "";
  const out: Entry[] = [];
  const problems: string[] = [];

  const ops = collection("operations");
  const audit = collection("audit");
  const [operationsResult, auditResult] = await Promise.allSettled([
    ops ? call("list:operations", "GET", project ? basePath(ops, project) : "/api/v1/operations", {
      target: name,
      pageSize: 50,
      orderBy: "createdAt desc",
    }) : Promise.resolve({ items: [] }),
    audit ? call("list:audit", "GET", basePath(audit, ""), {
      target: name,
      pageSize: 50,
      orderBy: "createdAt desc",
    }) : Promise.resolve({ items: [] }),
  ]);
  if (operationsResult.status === "fulfilled") {
    const answer = operationsResult.value;
    for (const r of (answer.items ?? []) as Resource[]) {
      const spec = (r.spec ?? {}) as Record<string, unknown>;
      const status = (r.status ?? {}) as Record<string, unknown>;
      const error = typeof status.error === "string" ? status.error.trim() : "";
      out.push({
        at: Number(r.meta.createdAt ?? 0),
        who: String(spec.requestedBy ?? "somebody"),
        what: String(spec.verb ?? "changed it"),
        detail: error || (status.done === true ? "Completed" : "In progress"),
        kind: error ? "failed" : "accepted",
      });
    }
  } else problems.push(`Operations unavailable: ${String((operationsResult.reason as Error)?.message ?? operationsResult.reason)}`);

  if (auditResult.status === "fulfilled") {
    const answer = auditResult.value;
    for (const r of (answer.items ?? []) as Resource[]) {
      const spec = (r.spec ?? {}) as Record<string, unknown>;
      const kind = String(spec.kind ?? "");
      out.push({
        at: Number(spec.at ?? r.meta.createdAt ?? 0),
        who: String(spec.subject ?? "somebody"),
        what: String(spec.verb ?? ""),
        detail: String(spec.detail ?? ""),
        kind: kind.toLowerCase() === "refused" ? "refused" : "changed",
      });
    }
  } else problems.push(`Audit unavailable: ${String((auditResult.reason as Error)?.message ?? auditResult.reason)}`);

  return { entries: out.sort((a, b) => b.at - a.at), warning: problems.join(" · ") };
}

const TONE: Record<Entry["kind"], string> = {
  accepted: "var(--dot-settled)",
  failed: "var(--dot-failing)",
  changed: "var(--brand)",
  refused: "var(--dot-failing)",
};

const WORD: Record<Entry["kind"], string> = {
  accepted: "accepted",
  failed: "failed",
  changed: "changed",
  refused: "refused",
};

const readable = (verb: string) => ({
  create: "Created", update: "Updated", patch: "Updated", delete: "Deletion requested",
  start: "Started", stop: "Stopped", restart: "Restarted", migrate: "Migration requested",
  attach: "Attached", detach: "Detached",
}[verb.toLowerCase()] ?? verb.replace(/^./, (c) => c.toUpperCase()));

export function History({ r }: { r: Resource }) {
  const name = nameOf(r);
  const [entries, setEntries] = useState<Entry[] | null>(null);
  const [problem, setProblem] = useState("");
  const [refresh, setRefresh] = useState(0);

  useEffect(() => {
    let current = true;
    setProblem("");
    historyOf(name)
      .then((found) => { if (current) { setEntries(found.entries); setProblem(found.warning); } })
      .catch((e) => current && setProblem(String((e as Error).message)));
    return () => {
      current = false;
    };
  }, [name, refresh]);

  if (problem && !entries?.length) {
    return (
      <div className="grid gap-2"><p role="alert" className="text-xs text-destructive">The history could not be read: {problem}</p>
        <button className="w-fit text-xs text-primary hover:underline" onClick={() => setRefresh((n) => n + 1)}>Retry activity</button></div>
    );
  }
  if (!entries) {
    return (
      <p className="text-xs" style={{ color: "var(--text-faint)" }}>
        Reading what has happened…
      </p>
    );
  }
  if (entries.length === 0) {
    return (
      <div className="grid gap-2"><p className="text-xs text-muted-foreground">Nothing has been asked of this object since it was created.</p>
        <button className="w-fit text-xs text-primary hover:underline" onClick={() => setRefresh((n) => n + 1)}>Refresh activity</button></div>
    );
  }

  return (
    <div className="grid gap-2">
      <div className="flex items-center justify-between gap-2">
        {problem ? <p role="alert" className="text-xs text-destructive">Some activity is unavailable: {problem}</p> : <span />}
        <button className="shrink-0 text-xs text-primary hover:underline" onClick={() => setRefresh((n) => n + 1)}>Refresh activity</button>
      </div>
      <ol className="grid gap-0" aria-label="What has happened to this object">
      {entries.map((e, i) => (
        <li
          key={`${e.at}-${i}`}
          className="grid gap-0.5 border-t py-2 pl-3"
          style={{ borderColor: "var(--border-subtle)", borderLeft: `3px solid ${TONE[e.kind]}` }}
        >
          <p className="text-sm" style={{ color: "var(--text-body)" }}>
            <span className="font-medium">{readable(e.what)}</span>{" "}
            <span style={{ color: e.kind === "refused" || e.kind === "failed" ? "var(--failing)" : "var(--text-faint)" }}>
              · {WORD[e.kind]}
            </span>
          </p>
          {e.detail && (
            <p className="text-xs" style={{ color: "var(--text-faint)" }}>
              {e.detail}
            </p>
          )}
          <p className="font-mono text-[11px]" style={{ color: "var(--text-faint)" }}>
            {e.who} · {e.at ? ago(e.at) : "—"}
          </p>
        </li>
      ))}
      </ol>
    </div>
  );
}
