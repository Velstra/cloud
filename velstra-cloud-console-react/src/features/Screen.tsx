// The guest's display, over noVNC. Same door as the terminal — `POST …:console`
// with `kind: "Vnc"` grants a session and a one-time ticket, the ticket is
// spent by opening `…:consoleStream` as a WebSocket — and RFB is what travels
// on it. noVNC speaks RFB rather than this file, which is the decision the
// hand-rolled client in the current console argued against and lost: a
// protocol that carries keystrokes into machines is not a place to be clever.
//
// Only VNC. The cell's API has no SPICE grant — nothing in the API, the agent
// or the contract mentions it — so there is nothing here to speak SPICE to.

import { useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { Button } from "@/components/ui/button";
import { call } from "@/api/transport";
import { idOf, type Resource } from "@/lib/model";
import { pathOf, type Collection } from "@/lib/schema";
import { useStore } from "@/app/store";
import { Pressed } from "./Pressed";

type Rfb = { disconnect: () => void; sendCtrlAltDel: () => void; addEventListener: (k: string, f: (e: any) => void) => void; scaleViewport: boolean; resizeSession: boolean; focus: () => void };

export function Screen({ r, coll }: { r: Resource; coll: Collection }) {
  const project = useStore((s) => s.project);
  const host = useRef<HTMLDivElement>(null);
  const rfb = useRef<Rfb | null>(null);
  const [state, setState] = useState<"closed" | "asking" | "open" | "gone">("closed");
  const [why, setWhy] = useState("");
  const [popup, setPopup] = useState<Window | null>(null);

  useEffect(() => () => { rfb.current?.disconnect(); popup?.close(); }, [popup]);

  const attach = async () => {
    setWhy(""); setState("asking");
    try {
      const path = `${pathOf(coll, r, project)}/${encodeURIComponent(idOf(r))}`;
      const grant = await call("console", "POST", `${path}:console`, undefined, { kind: "Vnc" });
      const url = `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}${path}:consoleStream?session=${encodeURIComponent(grant.session)}&ticket=${encodeURIComponent(grant.ticket)}`;
      const { default: RFB } = await import("@novnc/novnc");
      const client: Rfb = new RFB(host.current!, url, { wsProtocols: [] });
      client.scaleViewport = true;
      client.resizeSession = false;
      client.addEventListener("connect", () => { setState("open"); client.focus(); });
      client.addEventListener("disconnect", (e) => { setState("gone"); if (e?.detail?.clean === false) setWhy("The screen closed on its own."); });
      client.addEventListener("securityfailure", (e) => { setWhy("Refused: " + (e?.detail?.reason ?? "security")); setState("gone"); });
      rfb.current = client;
    } catch (e) {
      setWhy((e as Error).message); setState("closed");
    }
  };

  useEffect(() => {
    if (popup && state === "closed" && host.current) void attach();
    // `popup` is the deliberate trigger. Re-running this when `state` changes
    // would spend a second one-time console ticket.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [popup]);

  const popOut = () => {
    const win = window.open("", `velstra-vnc-${idOf(r)}`, "popup,width=1280,height=820");
    if (!win) { setWhy("The browser blocked the screen window."); return; }
    win.document.title = `${idOf(r)} · Velstra screen`;
    win.document.body.style.cssText = "margin:0;background:#000;overflow:hidden";
    win.addEventListener("beforeunload", () => { rfb.current?.disconnect(); setPopup(null); });
    setPopup(win);
  };

  const display = <div ref={host} className="h-[420px] overflow-hidden rounded-[4px] border"
    style={{ borderColor: "var(--border)", background: "#000", display: state === "closed" ? "none" : "block", ...(popup ? { width: "100vw", height: "100vh", border: 0 } : {}) }} />;

  return (
    <div className="grid gap-2">
      <div className="flex flex-wrap items-center gap-2 text-xs" style={{ color: "var(--text-muted)" }}>
        {state === "closed" && <Pressed size="sm" onPress={attach}>Open screen</Pressed>}
        {state === "closed" && <Button size="sm" variant="secondary" onClick={popOut}>Open in window</Button>}
        {state === "asking" && <span>Asking for the screen…</span>}
        {state === "open" && <>
          <span style={{ color: "var(--settled)" }}>● connected — click the screen and type</span>
          <Button size="sm" variant="secondary" onClick={() => rfb.current?.sendCtrlAltDel()}>Ctrl-Alt-Del</Button>
          <Button size="sm" variant="secondary" onClick={() => rfb.current?.disconnect()}>Close</Button>
        </>}
        {state === "gone" && <><span>Closed.</span><Button size="sm" variant="secondary" onClick={attach}>Open again</Button></>}
        {why && <span role="alert" style={{ color: "var(--failing)" }}>{why}</span>}
      </div>
      {popup ? createPortal(display, popup.document.body) : display}
    </div>
  );
}
