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
  kind: "accepted" | "refused" | "changed";
};

/** Records about one object, both kinds, newest first.
 *
 * The API filters by target (`?target=…`) for exactly these two collections,
 * so the browser is not handed a cell's worth of audit to sift. */
async function historyOf(name: string): Promise<Entry[]> {
  const project = projectOf(name) ?? "";
  const out: Entry[] = [];

  const ops = collection("operations");
  if (ops) {
    const answer = await call("list:operations", "GET", basePath(ops, project), {
      target: name,
      pageSize: 50,
    });
    for (const r of (answer.items ?? []) as Resource[]) {
      const spec = (r.spec ?? {}) as Record<string, unknown>;
      const status = (r.status ?? {}) as Record<string, unknown>;
      const done = String(status.state ?? "");
      out.push({
        at: Number(r.meta.createdAt ?? 0),
        who: String(spec.requestedBy ?? "somebody"),
        what: String(spec.verb ?? "changed it"),
        detail: done ? `${done}${status.message ? ` — ${status.message}` : ""}` : "",
        kind: "accepted",
      });
    }
  }

  const audit = collection("audit");
  if (audit) {
    const answer = await call("list:audit", "GET", basePath(audit, ""), {
      target: name,
      pageSize: 50,
    });
    for (const r of (answer.items ?? []) as Resource[]) {
      const spec = (r.spec ?? {}) as Record<string, unknown>;
      const kind = String(spec.kind ?? "");
      out.push({
        at: Number(spec.at ?? r.meta.createdAt ?? 0),
        who: String(spec.subject ?? "somebody"),
        what: String(spec.verb ?? ""),
        detail: String(spec.detail ?? ""),
        kind: kind === "Refused" ? "refused" : "changed",
      });
    }
  }

  return out.sort((a, b) => b.at - a.at);
}

const TONE: Record<Entry["kind"], string> = {
  accepted: "var(--dot-settled)",
  changed: "var(--brand)",
  refused: "var(--dot-failing)",
};

const WORD: Record<Entry["kind"], string> = {
  accepted: "accepted",
  changed: "changed",
  refused: "refused",
};

export function History({ r }: { r: Resource }) {
  const name = nameOf(r);
  const [entries, setEntries] = useState<Entry[] | null>(null);
  const [problem, setProblem] = useState("");

  useEffect(() => {
    let current = true;
    setEntries(null);
    setProblem("");
    historyOf(name)
      .then((found) => current && setEntries(found))
      .catch((e) => current && setProblem(String((e as Error).message)));
    return () => {
      current = false;
    };
  }, [name]);

  if (problem) {
    return (
      <p className="text-xs" style={{ color: "var(--text-faint)" }}>
        The history could not be read: {problem}
      </p>
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
      <p className="text-xs" style={{ color: "var(--text-faint)" }}>
        Nothing has been asked of this object since it was created.
      </p>
    );
  }

  return (
    <ol className="grid gap-0" aria-label="What has happened to this object">
      {entries.map((e, i) => (
        <li
          key={`${e.at}-${i}`}
          className="grid gap-0.5 border-t py-2 pl-3"
          style={{ borderColor: "var(--border-subtle)", borderLeft: `3px solid ${TONE[e.kind]}` }}
        >
          <p className="text-sm" style={{ color: "var(--text-body)" }}>
            <span className="font-medium">{e.who}</span> · {e.what} ·{" "}
            <span style={{ color: e.kind === "refused" ? "var(--failing)" : "var(--text-faint)" }}>
              {WORD[e.kind]}
            </span>
          </p>
          {e.detail && (
            <p className="text-xs" style={{ color: "var(--text-faint)" }}>
              {e.detail}
            </p>
          )}
          <p className="font-mono text-[11px]" style={{ color: "var(--text-faint)" }}>
            {e.at ? ago(e.at) : "—"}
          </p>
        </li>
      ))}
    </ol>
  );
}
