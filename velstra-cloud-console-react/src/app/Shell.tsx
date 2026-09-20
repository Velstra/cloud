// The frame around every screen: a rail you can fold, a bar that says who and
// where, an attention inbox that follows you, and a palette (⌘K) that reaches
// anything by name — collections, objects, actions, the places you were.
//
// Narrow screens get the same console, not a lesser one. The rail becomes a
// drawer over the content instead of a column beside it, and everything else
// stays where it was. The reason is the one case that matters: somebody woken
// at three in the morning has a phone, and "cordon this node" is four taps or
// it is a drive to a desk.

import { useEffect, useMemo, useRef, useState } from "react";
import { Bell, ChevronDown, ChevronRight, Command as Cmd, LogOut, Menu, Moon, Rows3, Search, Sun, UserRound, Box, Database, Network, Server, Shield, Activity, LayoutDashboard, Workflow, Cloud, X, PanelLeftClose, PanelLeftOpen } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  CommandDialog, CommandEmpty, CommandGroup, CommandInput, CommandItem, CommandList, CommandSeparator,
} from "@/components/ui/command";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import {
  DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { useCan } from "@/lib/iam";
import { call, clearToken } from "@/api/transport";
import { listEvery } from "@/lib/listing";
import { attentionName, idOf, nameOf, verdict, VERDICT_ORDER, type Resource, type Verdict } from "@/lib/model";
import { ALL, SCHEMA, collection, groups, navigable, routeId, type Collection } from "@/lib/schema";
import { go, href, useRoute } from "@/app/router";
import { setState, useStore } from "@/app/store";
import { State } from "@/features/State";

export type Census = Record<string, { total: number; unsettled: Resource[] }>;

export function Shell({ census, onSweep, children }: {
  census: Census; onSweep: () => void; children: React.ReactNode;
}) {
  const route = useRoute();
  const who = useStore((s) => s.who);
  const project = useStore((s) => s.project);
  const theme = useStore((s) => s.theme);
  const density = useStore((s) => s.density);
  const collapsed = useStore((s) => s.railCollapsed);
  const compact = useStore((s) => s.railCompact);
  const [paletteOpen, setPaletteOpen] = useState(false);
  // Not persisted: which way the drawer was left on a phone is not a
  // preference, it is where the last tap put it.
  const [railOpen, setRailOpen] = useState(false);
  const [projects, setProjects] = useState<string[]>([project]);
  const navInput = useRef<HTMLInputElement>(null);
  const [navQuery, setNavQuery] = useState("");
  const [expandedSections, setExpandedSections] = useState<Record<string, boolean>>({});
  const primarySections = new Set(["instances", "image-families", "volumes", "ceph-clusters", "pools", "networks", "subnets", "security-groups", "nodes", "maintenance", "projects", "users", "audit", "usage"]);

  useEffect(() => {
    const on = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") { e.preventDefault(); setPaletteOpen((o) => !o); }
    };
    addEventListener("keydown", on); return () => removeEventListener("keydown", on);
  }, []);
  useEffect(() => {
    if (!who?.cellAdmin) return;
    call("list:projects", "GET", "/api/v1/projects", { pageSize: 200 })
      .then((a) => setProjects((a.items ?? []).map((p: Resource) => idOf(p)))).catch(() => {});
  }, [who]);

  const attention = useMemo(() => {
    const out: { coll: Collection; r: Resource; kind: Verdict }[] = [];
    for (const c of SCHEMA) for (const r of census[c.id]?.unsettled ?? []) out.push({ coll: c, r, kind: verdict(r, c).kind });
    return out.sort((a, b) => VERDICT_ORDER[a.kind] - VERDICT_ORDER[b.kind]);
  }, [census]);
  const failing = attention.filter((a) => a.kind === "failing").length;
  const current = route.view === "board" ? collection(route.coll) : undefined;

  return (
    <div className="flex h-full" style={{ background: "var(--bg-app)" }}>
      {/* The scrim: only under `md`, and only while the drawer is open. It
          closes the drawer, which is what tapping beside a drawer means
          everywhere else. */}
      {railOpen && (
        <button
          className="fixed inset-0 z-30 md:hidden"
          aria-label="Close the navigation"
          onClick={() => setRailOpen(false)}
          style={{ background: "color-mix(in oklab, var(--bg-app) 70%, transparent)" }}
        />
      )}
      <aside
        id="rail"
        data-open={railOpen}
        className={`flex shrink-0 flex-col border-r transition-[width] max-md:fixed max-md:inset-y-0 max-md:left-0 max-md:z-40 max-md:w-[232px] max-md:-translate-x-full max-md:transition-transform max-md:data-[open=true]:translate-x-0 ${compact ? "w-16" : "w-[232px]"}`}
        style={{ background: "var(--sidebar-bg)", borderColor: "var(--border)" }}
      >
        <div className={`flex items-center gap-2.5 pb-4 pt-5 text-[17px] font-semibold tracking-tight ${compact ? "justify-center px-2" : "px-4"}`} style={{ color: "var(--text-strong)" }}>
          <span className="grid size-8 shrink-0 place-items-center rounded-lg bg-primary text-primary-foreground"><Cloud className="size-5" /></span>
          {!compact && <><span>Velstra <span style={{ color: "var(--product)" }}>Cloud</span></span><span className="ml-auto" /></>}
          <Tooltip><TooltipTrigger render={<Button size="icon" variant="ghost" className="hidden size-7 md:inline-flex" aria-label={compact ? "Expand navigation" : "Collapse navigation"} onClick={() => setState({ railCompact: !compact })} />}>
            {compact ? <PanelLeftOpen className="size-4" /> : <PanelLeftClose className="size-4" />}
          </TooltipTrigger><TooltipContent>{compact ? "Expand navigation" : "Collapse navigation"}</TooltipContent></Tooltip>
        </div>
        {!compact && <><button onClick={() => setPaletteOpen(true)} className="mx-3 mb-3 flex items-center gap-2 rounded-[4px] border px-2.5 py-1.5 text-xs"
          style={{ borderColor: "var(--border)", color: "var(--text-muted)", background: "var(--surface-sunken)" }}>
          <Search className="size-3.5" /> Jump to… <kbd className="ml-auto rounded border px-1 font-mono text-[10px]" style={{ borderColor: "var(--border-strong)" }}>⌘K</kbd>
        </button>
        <div className="relative mx-3 mb-3"><Search className="absolute left-2.5 top-2.5 size-3.5 text-muted-foreground" /><input ref={navInput} aria-label="Filter navigation" value={navQuery} onChange={(e) => setNavQuery(e.target.value)} placeholder="Find a section" className="h-9 w-full rounded-lg border border-border bg-background pl-8 pr-7 text-xs" />{navQuery && <button aria-label="Clear navigation filter" onClick={() => { setNavQuery(""); navInput.current?.focus(); }} className="absolute right-2 top-2.5"><X className="size-3.5" /></button>}</div></>}
        {/* A link followed from in here closes the drawer, on the click that
            caused it rather than on the route it produced. Without this,
            tapping a collection on a phone loads the board underneath a rail
            that is still covering it — and a *link* rather than any click,
            because folding a group open is not leaving the rail. */}
        <nav
          className="min-h-0 flex-1 overflow-y-auto px-2 pb-4"
          onClick={(e) => {
            if ((e.target as HTMLElement).closest("a")) setRailOpen(false);
          }}
        >
          <RailLink compact={compact} active={route.view === "overview"} to={href({ view: "overview" })} label="Overview"
            badge={attention.length ? <Badge n={attention.length} tone={failing ? "failing" : "drifting"} /> : null} />
          {!compact && <RailLink compact={compact} active={route.view === "map"} to={href({ view: "map" })} label="Map" />}
          {groups(!!who?.cellAdmin).map((g) => {
            const items = g.items.filter((c) => (who?.cellAdmin || c.scope === "project") && (!navQuery || c.title.toLowerCase().includes(navQuery.toLowerCase())));
            if (!items.length) return null;
            const open = !!navQuery || !(collapsed[g.name] ?? ["Records", "Access"].includes(g.name));
            const expanded = !!navQuery || (expandedSections[g.name] ?? items.some((c) => c.id === current?.id && !primarySections.has(c.id)));
            const shown = expanded ? items : items.filter((c) => primarySections.has(c.id));
            return (
              <div key={g.name} className={compact ? "mt-1" : "mt-3"}>
                {!compact && <>
                <button aria-expanded={open} onClick={() => setState({ railCollapsed: { ...collapsed, [g.name]: open } })}
                  className="flex w-full items-center gap-1 px-2 pb-1 text-[11px] font-semibold uppercase tracking-[0.07em]" style={{ color: "var(--text-muted)" }}>
                  {open ? <ChevronDown className="size-3" /> : <ChevronRight className="size-3" />} {g.name}
                  {!open && <span className="ml-auto font-mono text-[10px] normal-case tracking-normal">{items.reduce((n, c) => n + (census[c.id]?.unsettled.length ?? 0), 0) || ""}</span>}
                </button>
                </>}
                <div className="fold" data-closed={!compact && !open}><div>
                  {shown.map((c) => {
                    const seen = census[c.id]; const un = seen?.unsettled.length ?? 0;
                    return (
                      <RailLink compact={compact} key={c.id} active={current?.id === c.id} to={href({ view: "board", coll: c.id })} label={c.title}
                        badge={un ? <Badge n={un} tone={seen!.unsettled.some((r) => verdict(r, c).kind === "failing") ? "failing" : "drifting"} />
                          : <span className="font-mono text-[11px]" style={{ color: "var(--text-faint)" }}>{seen ? seen.total : ""}</span>} />
                    );
                  })}
                  {!compact && !navQuery && items.some((c) => !primarySections.has(c.id)) && <button className="ml-8 mt-1 text-xs text-muted-foreground hover:text-foreground" aria-expanded={!!expanded} onClick={() => setExpandedSections((old) => ({ ...old, [g.name]: !expanded }))}>{expanded ? "Show less" : `More (${items.length - shown.length})`}</button>}
                  {/* The bill belongs with the readings it is summed from —
                      one group, evidence and total. It is not a collection, so
                      the schema cannot put it here and this does. */}
                  {g.items.some((c) => c.id === "usage") && (
                    <RailLink compact={compact} active={route.view === "spend"} to={href({ view: "spend" })} label="Spend" />
                  )}
                </div></div>
              </div>
            );
          })}
        </nav>
      </aside>

      <div className="flex min-w-0 flex-1 flex-col">
        <header className="flex h-12 shrink-0 items-center gap-3 border-b px-3 md:px-5" style={{ borderColor: "var(--border)", background: "var(--surface)" }}>
          <Button
            size="sm"
            variant="ghost"
            className="md:hidden"
            aria-label="Open the navigation"
            aria-expanded={railOpen}
            aria-controls="rail"
            onClick={() => setRailOpen(true)}
          >
            <Menu className="size-4" />
          </Button>
          {/* The breadcrumb scrolls rather than truncates. Cutting it would
              have to cut one end, and both ends identify the screen: the
              project on the left, the object on the right. */}
          <nav className="flex min-w-0 items-center gap-1.5 overflow-x-auto whitespace-nowrap font-mono text-xs" style={{ color: "var(--text-muted)" }}>
            {who?.cellAdmin ? (
              <DropdownMenu>
                <DropdownMenuTrigger className="rounded-[3px] border px-2 py-0.5" style={{ borderColor: "var(--border)" }}>{project === ALL ? "all projects" : project} ▾</DropdownMenuTrigger>
                <DropdownMenuContent align="start">
                  <DropdownMenuItem onClick={() => setState({ project: ALL })}>All projects</DropdownMenuItem>
                  {projects.map((p) => <DropdownMenuItem key={p} onClick={() => setState({ project: p })}>{p}</DropdownMenuItem>)}
                </DropdownMenuContent>
              </DropdownMenu>
            ) : Object.keys(who?.projects ?? {}).length > 1 ? (
              <DropdownMenu>
                <DropdownMenuTrigger className="rounded-[3px] border px-2 py-0.5" style={{ borderColor: "var(--border)" }}>{project} ▾</DropdownMenuTrigger>
                <DropdownMenuContent align="start">
                  {Object.entries(who?.projects ?? {}).map(([p, rung]) => <DropdownMenuItem key={p} onClick={() => setState({ project: p })}>{p}<span className="ml-2 text-[11px]" style={{ color: "var(--text-faint)" }}>{rung}</span></DropdownMenuItem>)}
                </DropdownMenuContent>
              </DropdownMenu>
            ) : <span title={who?.projects?.[project] ? `you are ${who.projects[project]} here` : undefined}>{project}</span>}
            <span>/</span>
            {route.view === "overview" ? <span>overview</span> : route.view === "map" ? <span>map</span>
              : route.view === "me" ? <span>your account</span>
              : route.view === "spend" ? <span>spend</span> : <>
              <a href={href({ view: "board", coll: route.coll })} className="hover:underline">{route.coll}</a>
              {route.id && <><span>/</span><span style={{ color: "var(--text-strong)" }}>{route.id}</span></>}
            </>}
          </nav>
          <div className="ml-auto flex items-center gap-1">
            <Popover>
              <Tooltip><TooltipTrigger render={
                <PopoverTrigger render={<Button size="sm" variant="ghost" aria-label={`${attention.length} objects need attention`} className="relative" />}>
                  <Bell className="size-4" />
                  {attention.length > 0 && <span className="absolute -right-0.5 -top-0.5 rounded-full px-1 font-mono text-[10px]" style={{ background: failing ? "var(--dot-failing)" : "var(--dot-drifting)", color: "#fff" }}>{attention.length}</span>}
                </PopoverTrigger>
              } /><TooltipContent>Attention inbox</TooltipContent></Tooltip>
              <PopoverContent align="end" className="w-[min(26rem,calc(100vw-1.5rem))] p-0">
                <div className="flex items-center justify-between border-b px-3 py-2 text-xs" style={{ borderColor: "var(--border-subtle)" }}>
                  <span className="font-semibold" style={{ color: "var(--text-strong)" }}>{attention.length ? `${attention.length} not settled` : "Everything has settled"}</span>
                  <button className="hover:underline" style={{ color: "var(--text-muted)" }} onClick={onSweep}>Sweep again</button>
                </div>
                <ul className="max-h-[24rem] overflow-y-auto">
                  {attention.slice(0, 60).map(({ coll, r }) => (
                    <li key={nameOf(r)}>
                      <a href={href({ view: "board", coll: coll.id, id: routeId(coll, r, project) })} className="grid grid-cols-[minmax(0,1fr)_auto] items-center gap-3 px-3 py-2 text-xs hover:bg-[var(--surface-hover)]">
                        <span className="truncate font-mono" style={{ color: "var(--text-body)" }}>{attentionName(r, coll)}</span>
                        <State of={r} coll={coll} />
                      </a>
                    </li>
                  ))}
                </ul>
              </PopoverContent>
            </Popover>
            <Tooltip><TooltipTrigger render={<Button size="sm" variant="ghost" aria-label="Your account"
              onClick={() => go({ view: "me" })} />}>
              <UserRound className="size-4" />
            </TooltipTrigger><TooltipContent>Your account</TooltipContent></Tooltip>
            <Tooltip><TooltipTrigger render={<Button size="sm" variant="ghost" aria-label="Sign out" onClick={async () => {
              try { await call("signOut", "DELETE", "/api/v1/sessions/current"); } catch { /* the token is going either way */ }
              clearToken(); setState({ who: null }); location.reload();
            }} />}>
              <LogOut className="size-4" />
            </TooltipTrigger><TooltipContent>Sign out</TooltipContent></Tooltip>
            <Tooltip><TooltipTrigger render={<Button size="sm" variant="ghost" className="hidden md:inline-flex" aria-label="Toggle density" onClick={() => setState({ density: density === "compact" ? "comfortable" : "compact" })} />}>
              <Rows3 className="size-4" />
            </TooltipTrigger><TooltipContent>{density === "compact" ? "Comfortable rows" : "Compact rows"}</TooltipContent></Tooltip>
            <Tooltip><TooltipTrigger render={<Button size="sm" variant="ghost" aria-label="Toggle appearance" onClick={() => setState({ theme: theme === "dark" ? "light" : theme === "light" ? "system" : "dark" })} />}>
              {theme === "light" ? <Sun className="size-4" /> : <Moon className="size-4" />}
            </TooltipTrigger><TooltipContent>Appearance: {theme}</TooltipContent></Tooltip>
            <Button size="sm" variant="ghost" className="hidden md:inline-flex" aria-label="Open the palette" onClick={() => setPaletteOpen(true)}><Cmd className="size-4" /></Button>
            <span className="ml-2 hidden text-xs lg:inline" style={{ color: "var(--text-muted)" }}>{who?.displayName ?? "—"}{who?.cellAdmin ? " · administrator" : ""}</span>
          </div>
        </header>
        <main className="min-h-0 flex-1">{children}</main>
      </div>

      <Palette open={paletteOpen} onOpenChange={setPaletteOpen} census={census} />
    </div>
  );
}

function RailLink({ active, to, label, badge, compact = false }: { active: boolean; to: string; label: string; badge?: React.ReactNode; compact?: boolean }) {
  const group = SCHEMA.find((c) => c.title === label)?.group;
  const Icon = label === "Overview" ? LayoutDashboard : label === "Map" ? Workflow : group === "Storage" ? Database : group === "Network" ? Network : group === "Hardware" ? Server : group === "Access" ? Shield : group === "Records" ? Activity : Box;
  return (
    <a href={to} aria-current={active ? "page" : undefined} title={compact ? label : undefined}
      className={`my-0.5 flex items-center rounded-lg py-2 text-[13px] transition-colors hover:bg-[var(--surface-hover)] focus-visible:outline-none focus-visible:ring-[3px] ${compact ? "justify-center px-2" : "gap-2.5 px-2.5"}`}
      style={{ background: active ? "color-mix(in srgb, var(--brand) 16%, transparent)" : undefined, color: active ? "var(--text-strong)" : "var(--text-body)" }}>
      <Icon className="size-4 shrink-0 text-muted-foreground" />{!compact && <><span className="flex-1">{label}</span>{badge}</>}
    </a>
  );
}

function Badge({ n, tone }: { n: number; tone: "failing" | "drifting" }) {
  return <span key={n} className="ticked inline-flex items-center gap-1 font-mono text-[11px]" style={{ color: `var(--${tone})` }}>
    <span className="size-[7px] rounded-full" style={{ background: `var(--dot-${tone})` }} />{n}
  </span>;
}

function Palette({ open, onOpenChange, census }: { open: boolean; onOpenChange: (o: boolean) => void; census: Census }) {
  const can = useCan();
  const recents = useStore((s) => s.recents);
  const who = useStore((s) => s.who);
  const project = useStore((s) => s.project);
  const [q, setQ] = useState("");
  const [found, setFound] = useState<{ coll: Collection; r: Resource }[]>([]);
  const route = useRoute();

  // Objects are searched against the live API, per collection the query could
  // mean, so the palette reaches an instance that is not on the current board.
  useEffect(() => {
    if (!open || q.trim().length < 2) { setFound([]); return; }
    let live = true;
    const targets = navigable(!!who?.cellAdmin).filter((c) => c.condition !== "").slice(0, 12);
    Promise.all(targets.map((c) =>
      listEvery(c, project).then((a) =>
        a.rows.filter((r: Resource) => idOf(r).toLowerCase().includes(q.toLowerCase())).slice(0, 4).map((r: Resource) => ({ coll: c, r })))
        .catch(() => []),
    )).then((all) => { if (live) setFound(all.flat().slice(0, 12)); });
    return () => { live = false; };
  }, [q, open, who, project]);

  const run = (fn: () => void) => { fn(); onOpenChange(false); setQ(""); };
  const current = route.view === "board" ? collection(route.coll) : undefined;

  return (
    <CommandDialog open={open} onOpenChange={onOpenChange} title="Jump" description="Collections, objects and actions, by name">
      <CommandInput placeholder="Where to, or what to do…" value={q} onValueChange={setQ} />
      <CommandList>
        <CommandEmpty>Nothing by that name.</CommandEmpty>
        {found.length > 0 && (
          <CommandGroup heading="Objects">
            {found.map(({ coll, r }) => (
              <CommandItem key={nameOf(r)} value={`${coll.id}/${idOf(r)}`} onSelect={() => run(() => go({ view: "board", coll: coll.id, id: routeId(coll, r, project) }))}>
                <span className="font-mono text-xs">{coll.id}/{idOf(r)}</span><span className="ml-auto"><State of={r} coll={coll} /></span>
              </CommandItem>
            ))}
          </CommandGroup>
        )}
        {recents.length > 0 && !q && (
          <CommandGroup heading="Recent">
            {recents.map((ref) => {
              const [c, id] = ref.split("/");
              return <CommandItem key={ref} value={"recent " + ref} onSelect={() => run(() => go({ view: "board", coll: c, id }))}><span className="font-mono text-xs">{ref}</span></CommandItem>;
            })}
          </CommandGroup>
        )}
        <CommandGroup heading="Actions">
          {current?.creatable && can("write", current) && <CommandItem value={`new ${current.singular}`} onSelect={() => run(() => go({ view: "board", coll: current.id, mode: "new" }))}>New {current.singular}</CommandItem>}
          <CommandItem value="theme dark" onSelect={() => run(() => setState({ theme: "dark" }))}>Appearance: dark</CommandItem>
          <CommandItem value="theme light" onSelect={() => run(() => setState({ theme: "light" }))}>Appearance: light</CommandItem>
          <CommandItem value="theme system" onSelect={() => run(() => setState({ theme: "system" }))}>Appearance: follow the system</CommandItem>
          <CommandItem value="density compact" onSelect={() => run(() => setState({ density: "compact" }))}>Compact rows</CommandItem>
          <CommandItem value="density comfortable" onSelect={() => run(() => setState({ density: "comfortable" }))}>Comfortable rows</CommandItem>
          <CommandItem value="motion reduced" onSelect={() => run(() => setState({ motion: "reduced" }))}>Motion: reduced</CommandItem>
          <CommandItem value="motion auto" onSelect={() => run(() => setState({ motion: "auto" }))}>Motion: as the system says</CommandItem>
        </CommandGroup>
        <CommandSeparator />
        <CommandGroup heading="Go to">
          <CommandItem value="overview" onSelect={() => run(() => go({ view: "overview" }))}>Overview</CommandItem>
          <CommandItem value="spend" onSelect={() => run(() => go({ view: "spend" }))}>Spend</CommandItem>
          <CommandItem value="account" onSelect={() => run(() => go({ view: "me" }))}>Your account</CommandItem>
          <CommandItem value="map topology network" onSelect={() => run(() => go({ view: "map" }))}>Map</CommandItem>
          {navigable(!!who?.cellAdmin).map((c) => (
            <CommandItem key={c.id} value={`${c.title} ${c.group}`} onSelect={() => run(() => go({ view: "board", coll: c.id }))}>
              {c.title}<span className="ml-auto text-xs" style={{ color: "var(--text-faint)" }}>{c.group}{census[c.id] ? ` · ${census[c.id].total}` : ""}</span>
            </CommandItem>
          ))}
        </CommandGroup>
      </CommandList>
    </CommandDialog>
  );
}
