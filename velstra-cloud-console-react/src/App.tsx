import { listEvery, projectNames } from "@/lib/listing";
import { collectionChanged } from "@/hooks/useCollection";
// Wiring: sign in, sweep the census the rail and the inbox are drawn from,
// and route between the overview and a board with its detail pane beside it.

import { useCallback, useEffect, useRef, useState } from "react";
import { Toaster } from "@/components/ui/sonner";
import { TooltipProvider } from "@/components/ui/tooltip";
import { AskProvider } from "@/features/Ask";
import { ResizableHandle, ResizablePanel, ResizablePanelGroup } from "@/components/ui/resizable";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { call, setToken, token, whenSessionEnds } from "@/api/transport";
import { verdict } from "@/lib/model";
import { ALL, SCHEMA, collection, routeId } from "@/lib/schema";
import { useRoute, go } from "@/app/router";
import { getState, setState, useStore } from "@/app/store";
import { Shell, type Census } from "@/app/Shell";
import { setCensus as setCensusStore, type CensusRows } from "@/app/census";
import { lazy, Suspense } from "react";
const Topology = lazy(() => import("@/features/Topology").then((m) => ({ default: m.Topology })));
import { Board } from "@/features/Board";
import { Detail } from "@/features/Detail";
import { Form as ResourceForm } from "@/features/Form";
import { Overview } from "@/features/Overview";
import { Me } from "@/features/Me";
import { Spend } from "@/features/Spend";
import { Pressed } from "@/features/Pressed";
import { toast } from "sonner";
import { MintedBox, hasMinted, type Minted } from "@/features/Join";
import { Cloud, Eye, EyeOff } from "lucide-react";

export default function App() {
  const who = useStore((s) => s.who);
  const project = useStore((s) => s.project);
  const route = useRoute();
  const [census, setCensus] = useState<Census>({});
  const sweepId = useRef(0);
  // A registration's credential, held on screen until it is copied. It is
  // shown once by the API — only a digest is kept — so the page must not move
  // on by itself the way every other create does.
  const [minted, setMinted] = useState<{ coll: string; id: string; minted: Minted } | null>(null);

  useEffect(() => {
    if (!token()) return;
    call("session", "GET", "/api/v1/sessions/current").then((w) => {
      const projects: Record<string, string> = w.projects ?? {};
      setState({ who: { subject: w.subject, displayName: w.displayName ?? w.subject, cellAdmin: !!w.cellAdmin, projects } });
      // A tenant lands in a project they are bound in, not in whatever the
      // last person on this browser had picked.
      const mine = Object.keys(projects);
      if (!w.cellAdmin && mine.length && !projects[getState().project]) setState({ project: mine[0] });
      // And an operator lands somewhere that exists. Their `projects` map is
      // empty — being an operator is not a binding — so the check above can
      // never help them, and whatever this browser had stored was kept even
      // when this cell has no such project. On a cell whose projects are not
      // named like the contract server's, that is every project-scoped board
      // reading zero while the cell is full. `ALL` is the operator's view and
      // is true of any cell; a deliberate pick that still exists is kept.
      if (w.cellAdmin) {
        const picked = getState().project;
        projectNames()
          .then((names) => {
            // One project is the whole cell, so land in it: `ALL` fans the
            // list out per project and cannot hold a watch, so a board there
            // says "no live updates" — which is the honest answer for a cell
            // with several and a needless one for a cell with one.
            if (names.length === 1) {
              if (picked !== names[0]) setState({ project: names[0] });
              return;
            }
            if (picked !== ALL && !names.includes(picked)) setState({ project: ALL });
          })
          .catch(() => { if (picked !== ALL) setState({ project: ALL }); });
      }
    }).catch(() => setToken(""));
  }, []);

  // The API is the one that knows a session ended; when it says so, the shell
  // shows the sign-in form rather than a signed-in frame that refuses.
  useEffect(() => whenSessionEnds(() => setState({ who: null })), []);

  const sweep = useCallback(async () => {
    if (!who) return;
    const request = ++sweepId.current;
    const out: Census = {};
    const all: CensusRows = {};
    // Records — audit entries, usage readings — are facts about the past, not
    // objects anybody manages, and a cell keeps hundreds of thousands of them.
    // The census counts what converges; those two are read where they are shown.
    // Everything this person can read, including the plumbing: the map and the
    // relations panel are drawn from it, and a port that is not in the census
    // is a wire missing from the picture. What it does *not* sweep is what the
    // API would refuse — a tenant's census used to ask for `migrations` and
    // `nodes` and count two silent 403s as "nothing there".
    const mine = SCHEMA.filter((c) =>
      (who.cellAdmin ? c.audience !== undefined : c.audience !== "operator")
      && c.id !== "audit" && c.id !== "usage");
    // What could not be read, and what was cut short — kept, not swallowed.
    // The relations panel puts a claim beside the Delete button that is only
    // true of a sweep that saw everything, and it cannot tell whether this was
    // one unless the sweep says so.
    const missing: Record<string, string> = {};
    const truncated: string[] = [];
    await Promise.all(mine.map(async (c) => {
      try {
        const page = await listEvery(c, project);
        all[c.id] = page.rows;
        if (page.truncated) truncated.push(c.id);
        out[c.id] = { total: page.rows.length, unsettled: c.condition === "" ? [] : page.rows.filter((r) => verdict(r, c).kind !== "settled") };
      } catch (e) {
        // The board says why when it is opened — and until somebody opens it,
        // this is the only record that the question was asked and not answered.
        missing[c.id] = (e as Error).message;
      }
    }));
    if (request !== sweepId.current || getState().project !== project || getState().who !== who) return;
    setCensusStore({ rows: all, missing, truncated });
    setCensus(out);
  }, [who, project]);
  useEffect(() => {
    setCensus({});
    setCensusStore({ rows: {}, missing: {}, truncated: [], sweptAt: 0 });
    void sweep();
    const timer = setInterval(() => { if (!document.hidden) void sweep(); }, 15000);
    return () => { ++sweepId.current; clearInterval(timer); };
  }, [sweep]);

  if (!who) return <SignIn />;

  const coll = route.view === "board" ? collection(route.coll) : undefined;

  return (
    <TooltipProvider>
      <AskProvider>
      <Shell census={census} onSweep={sweep}>
        {route.view === "spend" ? (
          <div className="h-full overflow-y-auto px-8 py-6"><Spend /></div>
        ) : route.view === "me" ? (
          <div className="h-full overflow-y-auto"><Me /></div>
        ) : route.view === "map" ? (
          <Suspense fallback={<p className="p-8 text-sm" style={{ color: "var(--text-muted)" }}>Drawing the map…</p>}><Topology /></Suspense>
        ) : route.view !== "board" || !coll ? (
          <div className="h-full overflow-y-auto px-4 py-5 md:px-7 md:py-6"><Overview onRefresh={sweep} /></div>
        ) : (
          <ResizablePanelGroup orientation="horizontal" className="resource-workspace h-full" data-detail={!!(route.id || route.mode === "new")}>
            <ResizablePanel defaultSize={route.id || route.mode === "new" ? "42%" : "100%"} minSize="28%">
              <div className="flex h-full flex-col px-6 py-5">
                <div className="mb-3 flex items-baseline gap-3">
                  <h1 className="text-[26px] font-bold leading-tight" style={{ color: "var(--text-strong)" }}>{coll.title}</h1>
                  <details className="relative text-xs text-muted-foreground"><summary>About</summary><p className="absolute right-0 top-6 z-20 w-72 rounded-lg border border-border bg-popover p-3 shadow-lg">{coll.blurb}</p></details>
                </div>
                <div className="min-h-0 flex-1"><Board coll={coll} selectedId={route.id} narrow={!!(route.id || route.mode === "new")} /></div>
              </div>
            </ResizablePanel>
            {(route.id || route.mode === "new") && (
              <>
                <ResizableHandle withHandle />
                <ResizablePanel defaultSize="58%" minSize="40%">
                  <div className="h-full border-l" style={{ background: "var(--surface)", borderColor: "var(--border)" }}>
                    {route.mode === "new" ? (
                      <div className="h-full overflow-y-auto">
                        <div className="border-b px-5 py-4" style={{ borderColor: "var(--border)" }}>
                          <h2 className="text-lg font-semibold" style={{ color: "var(--text-strong)" }}>New {coll.singular}</h2>
                        </div>
                        <div className="px-5 py-4">
                          {minted && minted.coll === coll.id ? (
                            <div className="grid gap-3">
                              <p className="text-sm" style={{ color: "var(--text-body)" }}>
                                <span className="font-medium" style={{ color: "var(--text-strong)" }}>{minted.id}</span> is registered. This is what the machine joins with.
                              </p>
                              <MintedBox minted={minted.minted} what={`the ${coll.singular}`} />
                              <div>
                                <Pressed size="sm" onPress={() => { const id = minted.id; setMinted(null); go({ view: "board", coll: coll.id, id }); }}>I have copied it</Pressed>
                              </div>
                            </div>
                          ) : (
                          <ResourceForm coll={coll}
                            onDone={(r, answer) => {
                              const id = r.meta.name.split("/").pop()!;
                              sweep(); collectionChanged(coll.id);
                              if (hasMinted(answer)) {
                                // Shown once and only here: a node or pool
                                // answers its create with the credential a
                                // machine joins with, and the object's page
                                // cannot show it later. Stay until it is
                                // copied.
                                setMinted({ coll: coll.id, id, minted: answer });
                                return;
                              }
                              toast.success(`${id} created`, { description: "Tracking deployment status." });
                              go({ view: "board", coll: coll.id, id: routeId(coll, r, project) });
                            }}
                            onCancel={() => go({ view: "board", coll: coll.id })} />
                          )}
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
      </AskProvider>
    </TooltipProvider>
  );
}

function SignIn() {
  const [u, setU] = useState("");
  const [p, setP] = useState("");
  const [err, setErr] = useState("");
  const [visible, setVisible] = useState(false);
  return (
    <div className="flex h-full items-center justify-center" style={{ background: "var(--bg-app)" }}>
      <form noValidate className="arrive-up mx-4 w-full max-w-[26rem] rounded-2xl border p-8" style={{ background: "var(--surface)", borderColor: "var(--border)" }}
        onSubmit={(e) => e.preventDefault()}>
        <span className="mb-5 grid size-11 place-items-center rounded-xl bg-primary text-primary-foreground"><Cloud className="size-6" /></span>
        <h1 className="mb-1 text-2xl font-semibold tracking-tight" style={{ color: "var(--text-strong)" }}>Velstra <span style={{ color: "var(--product)" }}>Cloud</span></h1>
        <p className="mb-6 text-sm" style={{ color: "var(--text-muted)" }}>Sign in to your workspace.</p>
        <div className="grid gap-4">
          <div className="grid gap-1.5"><Label htmlFor="u">Username</Label><Input id="u" autoComplete="username" value={u} onChange={(e) => setU(e.target.value)} autoFocus /></div>
          <div className="grid gap-1.5"><Label htmlFor="p">Password</Label><div className="relative"><Input id="p" autoComplete="current-password" type={visible ? "text" : "password"} value={p} onChange={(e) => setP(e.target.value)} className="pr-10" /><Button type="button" variant="ghost" size="icon-sm" className="absolute right-1 top-0.5" aria-label={visible ? "Hide password" : "Show password"} onClick={() => setVisible(!visible)}>{visible ? <EyeOff /> : <Eye />}</Button></div></div>
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
        </div>
      </form>
    </div>
  );
}
