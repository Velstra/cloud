// The guest's serial line, in the page. The same contract the other console
// speaks: `POST …:console` grants a session and a one-time ticket, the ticket
// is spent by opening `…:consoleStream` as a WebSocket, and text goes both
// ways. Not opened on its own — a ticket is spent by attaching, and a guest
// nobody is watching should not have one open.

import { useEffect, useRef, useState } from "react";
import { Terminal as XTerm } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";
import { Button } from "@/components/ui/button";
import { call } from "@/api/transport";
import { idOf, type Resource } from "@/lib/model";
import { pathOf, type Collection } from "@/lib/schema";
import { useStore } from "@/app/store";
import { Pressed } from "./Pressed";

export function Terminal({ r, coll }: { r: Resource; coll: Collection }) {
  const project = useStore((s) => s.project);
  const host = useRef<HTMLDivElement>(null);
  const term = useRef<XTerm | null>(null);
  const socket = useRef<WebSocket | null>(null);
  const [state, setState] = useState<"closed" | "asking" | "open" | "gone">("closed");
  const [why, setWhy] = useState("");
  // Whether the hop from the API to the node carrying this guest is private.
  // The browser's own leg is TLS whenever this page is; this is the one behind
  // it, across whatever network the cell's machines share. Said out loud
  // because what somebody types into a serial line is very often a root
  // password, and a screen that stays silent is one that gets treated as
  // private.
  const [encrypted, setEncrypted] = useState<boolean | null>(null);

  useEffect(() => () => { socket.current?.close(); term.current?.dispose(); }, []);

  const attach = async () => {
    setWhy(""); setState("asking");
    try {
      const path = `${pathOf(coll, r, project)}/${encodeURIComponent(idOf(r))}`;
      const grant = await call("console", "POST", `${path}:console`, undefined, {});
      setEncrypted(grant.encrypted === true);
      const url = `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}${path}:consoleStream?session=${encodeURIComponent(grant.session)}&ticket=${encodeURIComponent(grant.ticket)}`;
      if (!term.current && host.current) {
        const t = new XTerm({
          fontFamily: "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace", fontSize: 12, cursorBlink: true,
          theme: { background: getComputedStyle(document.documentElement).getPropertyValue("--ink-950").trim() || "#080c11" },
        });
        const fit = new FitAddon(); t.loadAddon(fit); t.open(host.current); fit.fit();
        addEventListener("resize", () => fit.fit());
        term.current = t;
      }
      const ws = new WebSocket(url);
      socket.current = ws;
      ws.onopen = () => { setState("open"); term.current?.focus(); };
      ws.onmessage = (e) => term.current?.write(typeof e.data === "string" ? e.data : new Uint8Array(e.data as ArrayBuffer));
      ws.onclose = () => setState("gone");
      ws.onerror = () => { setWhy("The stream did not open."); setState("gone"); };
      term.current?.onData((d) => { if (ws.readyState === WebSocket.OPEN) ws.send(d); });
    } catch (e) {
      setWhy((e as Error).message); setState("closed");
    }
  };

  return (
    <div className="grid gap-2">
      <div className="flex items-center gap-2 text-xs" style={{ color: "var(--text-muted)" }}>
        {state === "closed" && <Pressed size="sm" onPress={attach}>Attach</Pressed>}
        {state === "asking" && <span>Asking for a session…</span>}
        {state === "open" && <><span style={{ color: "var(--settled)" }}>● attached</span><Button size="sm" variant="secondary" onClick={() => socket.current?.close()}>Detach</Button></>}
        {state === "gone" && <><span>Detached.</span><Button size="sm" variant="secondary" onClick={attach}>Attach again</Button></>}
        {why && <span role="alert" style={{ color: "var(--failing)" }}>{why}</span>}
        {encrypted === false && (
          // Not an error and not decoration: it is the one thing somebody
          // needs to know before they type a password into this box.
          <span role="status" style={{ color: "var(--drifting)" }}>
            ⚠ Not encrypted past this browser — what you type crosses the cell's own network in
            the clear. Give the node a console certificate to change that.
          </span>
        )}
        {encrypted === true && (
          <span style={{ color: "var(--settled)" }}>Encrypted end to end</span>
        )}
        <span className="ml-auto">{Number(r.status?.consoleBytes ?? 0).toLocaleString()} bytes written so far</span>
      </div>
      <div ref={host} className="h-[280px] overflow-hidden rounded-[4px] border"
        style={{ borderColor: "var(--border)", background: "var(--ink-950, #080c11)", display: state === "closed" && !term.current ? "none" : "block" }} />
    </div>
  );
}
