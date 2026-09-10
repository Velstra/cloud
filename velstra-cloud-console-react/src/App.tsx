import { listEvery } from "@/lib/listing";
// Wiring: sign in, sweep the census the rail and the inbox are drawn from,
// and route between the overview and a board with its detail pane beside it.

import { useCallback, useEffect, useState } from "react";
import { Toaster } from "@/components/ui/sonner";
import { TooltipProvider } from "@/components/ui/tooltip";
import { ResizableHandle, ResizablePanel, ResizablePanelGroup } from "@/components/ui/resizable";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { call, setToken, token, whenSessionEnds } from "@/api/transport";
import { verdict, type Resource } from "@/lib/model";
import { SCHEMA, collection } from "@/lib/schema";
import { useRoute, go } from "@/app/router";
import { getState, setState, useStore } from "@/app/store";
import { Shell, type Census } from "@/app/Shell";
import { setCensusRows, type CensusRows } from "@/app/census";
import { lazy, Suspense } from "react";
const Topology = lazy(() => import("@/features/Topology").then((m) => ({ default: m.Topology })));
import { Board } from "@/features/Board";
import { Detail } from "@/features/Detail";
import { Form } from "@/features/Form";
import { Overview } from "@/features/Overview";
import { Pressed } from "@/features/Pressed";
import { toast } from "sonner";

export default function App() {
  const who = useStore((s) => s.who);
  const project = useStore((s) => s.project);
  const route = useRoute();
  const [census, setCensus] = useState<Census>({});

  useEffect(() => {
    if (!token()) return;
    call("session", "GET", "/api/v1/sessions/current").then((w) => {
      const projects: Record<string, string> = w.projects ?? {};
      setState({ who: { subject: w.subject, displayName: w.displayName ?? w.subject, cellAdmin: !!w.cellAdmin, projects } });
      // A tenant lands in a project they are bound in, not in whatever the
      // last person on this browser had picked.
      const mine = Object.keys(projects);
      if (!w.cellAdmin && mine.length && !projects[getState().project]) setState({ project: mine[0] });
    }).catch(() => setToken(""));
  }, []);

  // The API is the one that knows a session ended; when it says so, the shell
  // shows the sign-in form rather than a signed-in frame that refuses.
  useEffect(() => whenSessionEnds(() => setState({ who: null })), []);

  const sweep = useCallback(async () => {
    if (!who) return;
    const out: Census = {};
    const all: CensusRows = {};
    // Records — audit entries, usage readings — are facts about the past, not
    // objects anybody manages, and a cell keeps hundreds of thousands of them.
    // The census counts what converges; those two are read where they are shown.
    await Promise.all(SCHEMA.filter((c) => (who.cellAdmin || c.scope === "project") && c.id !== "audit" && c.id !== "usage").map(async (c) => {
      try {
        const items: Resource[] = (await listEvery(c, project)).rows;
        all[c.id] = items;
        out[c.id] = { total: items.length, unsettled: c.condition === "" ? [] : items.filter((r) => verdict(r, c).kind !== "settled") };
      } catch { /* the board says why when it is opened */ }
    }));
    setCensusRows(all);
    setCensus(out);
  }, [who, project]);
  useEffect(() => { sweep(); }, [sweep]);

  if (!who) return <SignIn />;

  const coll = route.view === "board" ? collection(route.coll) : undefined;

  return (
    <TooltipProvider>
      <Shell census={census} onSweep={sweep}>
        {route.view === "map" ? (
          <Suspense fallback={<p className="p-8 text-sm" style={{ color: "var(--text-muted)" }}>Drawing the map…</p>}><Topology /></Suspense>
        ) : route.view === "overview" || !coll ? (
          <div className="h-full overflow-y-auto px-8 py-6"><Overview /></div>
        ) : (
          <ResizablePanelGroup orientation="horizontal" className="h-full">
            <ResizablePanel defaultSize={route.id || route.mode === "new" ? "62%" : "100%"} minSize="35%">
              <div className="flex h-full flex-col px-6 py-5">
                <div className="mb-3 flex items-baseline gap-3">
                  <h1 className="text-[26px] font-bold leading-tight" style={{ color: "var(--text-strong)" }}>{coll.title}</h1>
                  <p className="truncate text-sm" style={{ color: "var(--text-muted)" }}>{coll.blurb}</p>
                </div>
                <div className="min-h-0 flex-1"><Board coll={coll} selectedId={route.id} narrow={!!(route.id || route.mode === "new")} /></div>
              </div>
            </ResizablePanel>
            {(route.id || route.mode === "new") && (
              <>
                <ResizableHandle withHandle />
                <ResizablePanel defaultSize="38%" minSize="26%">
                  <div className="h-full border-l" style={{ background: "var(--surface)", borderColor: "var(--border)" }}>
                    {route.mode === "new" ? (
                      <div className="h-full overflow-y-auto">
                        <div className="border-b px-5 py-4" style={{ borderColor: "var(--border)" }}>
                          <h2 className="text-lg font-semibold" style={{ color: "var(--text-strong)" }}>New {coll.singular}</h2>
                          <p className="text-xs" style={{ color: "var(--text-faint)" }}>{coll.blurb}</p>
                        </div>
                        <div className="px-5 py-4">
                          <Form coll={coll}
                            onDone={(r) => { toast(`${r.meta.name.split("/").pop()} created.`); sweep(); go({ view: "board", coll: coll.id, id: r.meta.name.split("/").pop()! }); }}
                            onCancel={() => go({ view: "board", coll: coll.id })} />
                        </div>
                      </div>
                    ) : (
                      <Detail key={route.id} coll={coll} id={route.id!} mode={route.mode} onChanged={sweep} />
                    )}
                  </div>
                </ResizablePanel>
              </>
            )}
          </ResizablePanelGroup>
        )}
      </Shell>
      <Toaster position="bottom-right" />
    </TooltipProvider>
  );
}

function SignIn() {
  const [u, setU] = useState("operator");
  const [p, setP] = useState("");
  const [err, setErr] = useState("");
  return (
    <div className="flex h-full items-center justify-center" style={{ background: "var(--bg-app)" }}>
      <form className="w-[26rem] rounded-[6px] border p-6" style={{ background: "var(--surface)", borderColor: "var(--border)" }}
        onSubmit={(e) => e.preventDefault()}>
        <h1 className="mb-1 text-xl font-bold" style={{ color: "var(--text-strong)" }}>Velstra <span style={{ color: "var(--product)" }}>Cloud</span></h1>
        <p className="mb-5 text-sm" style={{ color: "var(--text-muted)" }}>Sign in to the cell.</p>
        <div className="grid gap-4">
          <div className="grid gap-1.5"><Label htmlFor="u">Username</Label><Input id="u" value={u} onChange={(e) => setU(e.target.value)} autoFocus /></div>
          <div className="grid gap-1.5"><Label htmlFor="p">Passphrase</Label><Input id="p" type="password" value={p} onChange={(e) => setP(e.target.value)} /></div>
          {err && <p className="text-sm" role="alert" style={{ color: "var(--failing)" }}>{err}</p>}
          <Pressed type="submit" onPress={async () => {
            setErr("");
            try {
              const s = await call("signIn", "POST", "/api/v1/sessions", undefined, { username: u, password: p });
              setToken(s.token);
              // The sign-in answer names the person; the session says where they are
              // bound, so that is asked for next rather than guessed at.
              const w = await call("session", "GET", "/api/v1/sessions/current").catch(() => s);
              const projects: Record<string, string> = w.projects ?? {};
              setState({ who: { subject: w.subject ?? s.subject, displayName: w.displayName ?? s.displayName ?? s.subject, cellAdmin: !!w.cellAdmin, projects } });
              const mine = Object.keys(projects); if (!w.cellAdmin && mine.length && !projects[getState().project]) setState({ project: mine[0] });
            } catch (e) { setErr((e as Error).message || "That was not accepted."); }
          }}>Sign in</Pressed>
          <Button type="button" variant="ghost" size="sm" onClick={() => setP("a test operator passphrase")}>Use the test passphrase</Button>
        </div>
      </form>
    </div>
  );
}
