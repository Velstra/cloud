// Your own account.
//
// `users` is the cell's collection of everybody — a customer neither sees it
// nor should — so there was no route from this console to one's own password
// at all, though the API has permitted a self-service change all along. This is
// that one screen, and it is not a board filtered down to a single row.
//
// The read is best effort. A tenant may not be allowed to read the user object
// (it is an operator collection), and the password change does not need it:
// the PUT is addressed by subject, which the session already knows.

import { useEffect, useState } from "react";
import { call } from "@/api/transport";
import { useStore } from "@/app/store";
import { Account } from "@/features/Account";
import type { Resource } from "@/lib/model";

export function Me() {
  const who = useStore((s) => s.who);
  const [r, setR] = useState<Resource | null>(null);
  const [looked, setLooked] = useState(false);

  useEffect(() => {
    if (!who) return;
    call("get:users", "GET", `/api/v1/users/${encodeURIComponent(who.subject)}`)
      .then((x) => setR(x))
      .catch(() => setR(null))
      .finally(() => setLooked(true));
  }, [who?.subject]); // eslint-disable-line react-hooks/exhaustive-deps

  if (!who) return null;
  // What the account screen needs when the object itself is out of reach: the
  // subject, and the fact that this is you. Everything the panel draws from the
  // spec then reads as empty, which is honest — it was not read.
  const stand: Resource = r ?? {
    meta: { name: `users/${who.subject}`, labels: {} },
    spec: {},
    status: {},
  } as unknown as Resource;

  return (
    <div className="mx-auto grid max-w-3xl gap-5 px-8 py-6">
      <div>
        <h1 className="text-[26px] font-bold leading-tight" style={{ color: "var(--text-strong)" }}>
          {who.displayName || who.subject}
        </h1>
        <p className="text-sm" style={{ color: "var(--text-muted)" }}>
          Signed in as <span className="font-mono">{who.subject}</span>
          {who.cellAdmin ? " · cell operator" : ""}
        </p>
      </div>
      {looked && !r && (
        <p className="rounded-[4px] border px-3 py-2 text-xs"
          style={{ borderColor: "var(--border)", color: "var(--text-faint)", background: "var(--surface-sunken)" }}>
          Your account object is not readable from here, which is ordinary for a project member.
          Changing your password below does not need it.
        </p>
      )}
      <Account r={stand} />
    </div>
  );
}
