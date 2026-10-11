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
import { idOf, nameOf, type Resource } from "@/lib/model";
import { pathOf, projectOf, type Collection } from "@/lib/schema";
import { useCan } from "@/lib/iam";
import { useStore } from "@/app/store";
import { Pressed } from "./Pressed";

type Rfb = { disconnect: () => void; sendCtrlAltDel: () => void; addEventListener: (k: string, f: (e: any) => void) => void; scaleViewport: boolean; resizeSession: boolean; viewOnly: boolean; focus: () => void };

export function Screen(props: { r: Resource; coll: Collection }) {
  const project = useStore((s) => s.project);
  const can = useCan();
  if (!can("operate", props.coll, projectOf(nameOf(props.r)) ?? project)) {
    return <p className="text-xs" style={{ color: "var(--text-muted)" }}>Read-only access is available in Console. Screen requires permission to operate this instance.</p>;
  }
  return <ScreenSession key={nameOf(props.r)} {...props} />;
}

function ScreenSession({ r, coll }: { r: Resource; coll: Collection }) {
  const project = useStore((s) => s.project);
  const host = useRef<HTMLDivElement>(null);
  const rfb = useRef<Rfb | null>(null);
  const attempt = useRef(0);
  const popupRef = useRef<Window | null>(null);
  const [state, setState] = useState<"closed" | "asking" | "open" | "gone">("closed");
  const [why, setWhy] = useState("");
  const [popup, setPopup] = useState<Window | null>(null);
  const [readOnly, setReadOnly] = useState(false);
  const [encrypted, setEncrypted] = useState<boolean | null>(null);

  useEffect(() => () => {
    attempt.current++;
    rfb.current?.disconnect();
    const win = popupRef.current;
    popupRef.current = null;
    win?.close();
  }, []);

  const disconnect = () => {
    attempt.current++;
    rfb.current?.disconnect();
    rfb.current = null;
    setState("closed");
    setEncrypted(null);
  };

  const attach = async () => {
    const current = ++attempt.current;
    rfb.current?.disconnect();
    rfb.current = null;
    setWhy(""); setState("asking");
    setEncrypted(null);
    try {
      const path = `${pathOf(coll, r, project)}/${encodeURIComponent(idOf(r))}`;
      const grant = await call("console", "POST", `${path}:console`, undefined, { kind: "Vnc" });
      const url = `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}${path}:consoleStream?session=${encodeURIComponent(grant.session)}&ticket=${encodeURIComponent(grant.ticket)}`;
      const { default: RFB } = await import("@novnc/novnc");
      if (attempt.current !== current || !host.current) return;
      const client: Rfb = new RFB(host.current!, url, { wsProtocols: [] });
      client.scaleViewport = true;
      client.resizeSession = false;
      client.viewOnly = grant.readOnly === true;
      setReadOnly(client.viewOnly);
      setEncrypted(grant.encrypted === true);
      client.addEventListener("connect", () => {
        if (attempt.current !== current) return;
        setState("open"); client.focus();
      });
      client.addEventListener("disconnect", (e) => {
        if (attempt.current !== current) return;
        setState("gone");
        if (e?.detail?.clean === false) setWhy("The screen closed on its own.");
      });
      client.addEventListener("securityfailure", (e) => {
        if (attempt.current !== current) return;
        setWhy("Refused: " + (e?.detail?.reason ?? "security")); setState("gone");
      });
      rfb.current = client;
    } catch (e) {
      if (attempt.current !== current) return;
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
    disconnect();
    popupRef.current = win;
    win.addEventListener("beforeunload", () => {
      if (popupRef.current !== win) return;
      popupRef.current = null;
      disconnect();
      setPopup(null);
    });
    setPopup(win);
  };

  const display = <div ref={host} className="h-[420px] overflow-hidden rounded-[4px] border"
    style={{ borderColor: "var(--border)", background: "#000", display: state === "closed" ? "none" : "block", ...(popup ? { width: "100vw", height: "100vh", border: 0 } : {}) }} />;

  return (
    <div className="grid gap-2">
      <div className="flex flex-wrap items-center gap-2 text-xs" style={{ color: "var(--text-muted)" }}>
        {state === "closed" && <Pressed size="sm" onPress={attach}>Open screen</Pressed>}
        {!popup && state !== "asking" && <Button size="sm" variant="secondary" onClick={popOut}>Open in window</Button>}
        {state === "asking" && <span>Asking for the screen…</span>}
        {state === "open" && <>
          <span style={{ color: "var(--settled)" }}>● connected{readOnly ? " · Read-only session" : " — click the screen and type"}</span>
          {!readOnly && <Button size="sm" variant="secondary" onClick={() => rfb.current?.sendCtrlAltDel()}>Ctrl-Alt-Del</Button>}
          <Button size="sm" variant="secondary" onClick={disconnect}>Close</Button>
        </>}
        {state === "gone" && <><span>Closed.</span><Button size="sm" variant="secondary" onClick={attach}>Open again</Button></>}
        {why && <span role="alert" style={{ color: "var(--failing)" }}>{why}</span>}
      </div>
      {encrypted === true && <span className="text-xs" style={{ color: "var(--text-muted)" }}>Encrypted end to end</span>}
      {encrypted === false && <span role="status" className="text-xs" style={{ color: "var(--pending)" }}>The connection from the API to this node is not encrypted.</span>}
      {popup ? createPortal(display, popup.document.body) : display}
    </div>
  );
}
