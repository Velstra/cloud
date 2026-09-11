// The board: every object in a collection, as a table that sorts, filters,
// selects, hides columns, scrolls a thousand rows without stutter, and answers
// the keyboard — j/k to move, Enter to open, Space to pick, Escape to clear.

import { useEffect, useMemo, useRef, useState } from "react";
import {
  type ColumnDef, type SortingState, type VisibilityState, flexRender,
  getCoreRowModel, getFilteredRowModel, getSortedRowModel, useReactTable,
} from "@tanstack/react-table";
import { useVirtualizer } from "@tanstack/react-virtual";
import { ArrowDown, ArrowUp, Columns3, Search } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import {
  DropdownMenu, DropdownMenuCheckboxItem, DropdownMenuContent, DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Skeleton } from "@/components/ui/skeleton";
import { toast } from "sonner";
import { call } from "@/api/transport";
import { cellText, idOf, nameOf, verdict, VERDICT_ORDER, type Resource, type Verdict } from "@/lib/model";
import { ALL, at, basePath, projectOf, routeId, type Collection } from "@/lib/schema";
import { entry, collectionActions } from "@/registry";
import { getState, setState, useStore } from "@/app/store";
import { useCan } from "@/lib/iam";
import { go } from "@/app/router";
import { useCollection } from "@/hooks/useCollection";
import { Pressed } from "./Pressed";
import { State } from "./State";
import { Named, useAsk } from "@/features/Ask";
import type { WatchState } from "@/api/transport";

export function Board({ coll, selectedId, narrow }: { coll: Collection; selectedId?: string; narrow?: boolean }) {
  const ask = useAsk();
  const project = useStore((s) => s.project);
  const who = useStore((s) => s.who);
  const density = useStore((s) => s.density);
  // How this board was left: sort, columns, filters — kept per collection, so
  // the board you tuned yesterday is the board you get today.
  const saved = getState().views[coll.id];
  const [labels, setLabels] = useState(saved?.labels ?? "");
  const loaded = useCollection(coll, labels);
  const [sorting, setSorting] = useState<SortingState>((saved?.sorting as SortingState) ?? []);
  const [visibility, setVisibility] = useState<VisibilityState>(saved?.visibility ?? {});
  const [globalFilter, setGlobalFilter] = useState(saved?.filter ?? "");
  useEffect(() => {
    setState({ views: { ...getState().views, [coll.id]: { sorting, visibility, filter: globalFilter, labels } } });
  }, [coll.id, sorting, visibility, globalFilter, labels]);
  const [kind, setKind] = useState<Verdict | null>(null);
  const [selection, setSelection] = useState<Record<string, boolean>>({});
  const can = useCan();
  const [cursor, setCursor] = useState(0);
  // Beside an open detail the board is a list, not a table: the name, the
  // verdict and one column that tells rows apart. Ten columns in the width
  // that is left were an ellipsis in every cell — and the person reading the
  // detail was not reading them anyway. Every column comes back the moment the
  // detail closes, or on the switch for whoever wants it now.
  const [full, setFull] = useState(false);
  const asList = !!narrow && !full;
  const custom = entry(coll.id);

  const rows = useMemo(() => {
    const base = kind ? loaded.rows.filter((r) => verdict(r, coll).kind === kind) : loaded.rows;
    return [...base].sort((a, b) =>
      VERDICT_ORDER[verdict(a, coll).kind] - VERDICT_ORDER[verdict(b, coll).kind] ||
      nameOf(a).localeCompare(nameOf(b)));
  }, [loaded.rows, kind, coll]);

  const unsettled = loaded.rows.filter((r) => verdict(r, coll).kind !== "settled");
  const counts = unsettled.reduce<Partial<Record<Verdict, number>>>(
    (m, r) => { const k = verdict(r, coll).kind; m[k] = (m[k] ?? 0) + 1; return m; }, {});

  const columns = useMemo<ColumnDef<Resource>[]>(() => [
    {
      id: "pick", size: 36, enableSorting: false, enableHiding: false,
      header: ({ table }) => (
        <Checkbox aria-label="Select every row"
          checked={table.getIsAllRowsSelected()} indeterminate={!table.getIsAllRowsSelected() && table.getIsSomeRowsSelected()}
          onCheckedChange={(v) => table.toggleAllRowsSelected(!!v)} />
      ),
      cell: ({ row }) => (
        <Checkbox aria-label={`Select ${idOf(row.original)}`} checked={row.getIsSelected()}
          onCheckedChange={(v) => row.toggleSelected(!!v)} onClick={(e) => e.stopPropagation()} />
      ),
    },
    {
      id: "name", accessorFn: (r) => idOf(r), header: coll.singular === "project" ? "Project" : "Name", size: 240,
      cell: ({ row }) => <span className="font-medium" style={{ color: "var(--text-strong)" }}>{idOf(row.original)}</span>,
    },
    ...(coll.condition !== "" ? [...(project === ALL && coll.scope === "project" ? [{
      id: "project", accessorFn: (r: Resource) => projectOf(nameOf(r)) ?? "", header: "Project", size: 140,
      cell: (x: { getValue: () => unknown }) => <span className="font-mono text-xs" style={{ color: "var(--text-muted)" }}>{String(x.getValue() ?? "")}</span>,
    }] : []),
    {
      id: "verdict", accessorFn: (r: Resource) => VERDICT_ORDER[verdict(r, coll).kind], header: "Convergence", size: 160,
      cell: ({ row }: { row: { original: Resource } }) => <State of={row.original} coll={coll} />,
    }] : []),
    // The one column that tells rows apart: the first that carries a word or
    // a name, not a yes/no or a count.
    ...(asList ? [coll.columns.find((c) => c.cell === "text" || c.cell === "mono") ?? coll.columns[0]].filter(Boolean) : coll.columns).map<ColumnDef<Resource>>((c) => ({
      id: c.path, accessorFn: (r) => at(r, c.path) as any, header: c.label, size: Math.max(c.width, 120),
      cell: ({ row }) => {
        const override = custom.cells?.[c.path];
        if (override) return override(row.original);
        const v = at(row.original, c.path);
        return (
          <span className={c.cell === "mono" ? "font-mono text-xs" : ""}
            title={c.cell === "mono" ? String(v ?? "") : undefined}
            style={{ color: c.cell === "number" || c.cell === "count" ? "var(--text-body)" : "var(--text-muted)" }}>
            {cellText(c, v)}
          </span>
        );
      },
    })),
  ], [coll, custom, asList]);

  const table = useReactTable({
    data: rows, columns, getRowId: (r) => nameOf(r),
    state: { sorting, columnVisibility: visibility, globalFilter, rowSelection: selection },
    onSortingChange: setSorting, onColumnVisibilityChange: setVisibility,
    onGlobalFilterChange: setGlobalFilter, onRowSelectionChange: setSelection,
    getCoreRowModel: getCoreRowModel(), getSortedRowModel: getSortedRowModel(),
    getFilteredRowModel: getFilteredRowModel(), enableRowSelection: true,
    globalFilterFn: (row, _c, q) => JSON.stringify(row.original).toLowerCase().includes(String(q).toLowerCase()),
  });
  const visible = table.getRowModel().rows;

  const scroller = useRef<HTMLDivElement>(null);
  const rowH = density === "compact" ? 36 : 46;
  const [paneW, setPaneW] = useState(0);
  useEffect(() => {
    const el = scroller.current; if (!el) return;
    const ro = new ResizeObserver(() => setPaneW(el.clientWidth));
    ro.observe(el); setPaneW(el.clientWidth);
    return () => ro.disconnect();
  }, []);
  const tableW = Math.max(table.getTotalSize(), paneW);
  const virt = useVirtualizer({ count: visible.length, getScrollElement: () => scroller.current, estimateSize: () => rowH, overscan: 12 });

  // The keyboard drives the board. Not when a field has focus: `/` is how you
  // reach the filter, and typing j into it should type j.
  useEffect(() => {
    const on = (e: KeyboardEvent) => {
      // A key can arrive on the document itself, which has no ancestors to ask.
      const t = e.target;
      if (t instanceof Element && t.closest("input, textarea, select, [role=dialog], [cmdk-root]")) return;
      if (e.key === "j" || e.key === "ArrowDown") { e.preventDefault(); setCursor((c) => Math.min(visible.length - 1, c + 1)); }
      else if (e.key === "k" || e.key === "ArrowUp") { e.preventDefault(); setCursor((c) => Math.max(0, c - 1)); }
      else if (e.key === "Enter" && visible[cursor]) go({ view: "board", coll: coll.id, id: routeId(coll, visible[cursor].original, project) });
      else if (e.key === " " && visible[cursor]) { e.preventDefault(); visible[cursor].toggleSelected(); }
      else if (e.key === "/") { e.preventDefault(); (document.getElementById("boardfilter") as HTMLInputElement | null)?.focus(); }
      else if (e.key === "Escape") { setSelection({}); setKind(null); }
    };
    addEventListener("keydown", on); return () => removeEventListener("keydown", on);
  }, [visible, cursor, coll.id]);
  useEffect(() => { virt.scrollToIndex(cursor); }, [cursor, virt]);

  const picked = Object.keys(selection).filter((k) => selection[k]);
  const bulk = async (label: string, body: unknown | null, destructive = false) => {
    // **Named, not counted.** "Delete 12?" is a number somebody agrees with;
    // seeing `db-1` in the list is what makes them stop.
    if (destructive && !(await ask({
      title: `${label} ${picked.length} ${picked.length === 1 ? coll.singular : coll.title.toLowerCase()}?`,
      body: <Named ids={picked.map((n) => n.split("/").pop()!)} />,
      confirmLabel: label,
      tone: "danger",
    }))) return;
    let ok = 0; const bad: string[] = [];
    // What did not work, by name — because a second press should retry the
    // failures and nothing else. Partial success is the normal case here, not
    // an error path: half of these refusals are "something still holds it".
    const refused: Record<string, boolean> = {};
    for (const name of picked) {
      const id = name.split("/").pop()!;
      const base = basePath(coll, projectOf(name) ?? project);
      try {
        if (body === null) await call(`delete:${coll.id}`, "DELETE", `${base}/${encodeURIComponent(id)}`);
        else await call(`patch:${coll.id}`, "PATCH", `${base}/${encodeURIComponent(id)}`, undefined, body);
        ok++;
      } catch (e) { bad.push(`${id}: ${(e as Error).message}`); refused[name] = true; }
    }
    toast(ok ? `${label}: ${ok} done${bad.length ? `, ${bad.length} refused` : ""}` : "Nothing was accepted", { description: bad.slice(0, 3).join("\n") || undefined });
    setSelection(refused); loaded.refresh();
  };

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="flex flex-wrap items-center gap-2 pb-3">
        <div className="relative">
          <Search className="pointer-events-none absolute left-2 top-1/2 size-3.5 -translate-y-1/2" style={{ color: "var(--text-faint)" }} />
          <Input id="boardfilter" placeholder="Filter rows  /" className="h-8 w-56 pl-7 text-xs"
            value={globalFilter} onChange={(e) => setGlobalFilter(e.target.value)} />
        </div>
        <Input placeholder="labels: env=prod, tier=web" className="h-8 w-56 text-xs" value={labels}
          onChange={(e) => setLabels(e.target.value)} />
        {unsettled.length > 0 && (
          <div className="ml-1 flex items-center gap-1.5">
            <span className="text-xs" style={{ color: "var(--text-faint)" }}>{unsettled.length} not settled —</span>
            {(Object.keys(counts) as Verdict[]).sort((a, b) => VERDICT_ORDER[a] - VERDICT_ORDER[b]).map((k) => (
              <button key={k} aria-pressed={kind === k} onClick={() => setKind(kind === k ? null : k)}
                className="inline-flex items-center gap-1.5 rounded-[3px] border px-2 py-0.5 text-xs font-medium"
                style={{ background: "var(--surface-sunken)", borderColor: kind === k ? "var(--focus-ring)" : "var(--border)", color: kind === k ? "var(--text-strong)" : "var(--text-body)" }}>
                <span className="size-[7px] rounded-full" style={{ background: `var(--dot-${k === "unreported" ? "muted" : k})` }} />
                {counts[k]} {k}
              </button>
            ))}
          </div>
        )}
        <div className="ml-auto flex items-center gap-2">
          {collectionActions(coll.id, !!who?.cellAdmin).map((a) => (
            <Pressed key={a.id} size="sm" title={a.summary} variant="secondary" onPress={async () => {
              try {
                const r = await call(a.id, a.method, a.path.replace("{project}", project), undefined, a.needsBody ? {} : undefined);
                toast(a.label, { description: typeof r === "string" ? r : JSON.stringify(r).slice(0, 300) });
              } catch (e) { toast.error((e as Error).message); }
            }}>{a.label}</Pressed>
          ))}
          {narrow && (
            <Button size="sm" variant="ghost" aria-pressed={full} onClick={() => setFull((f) => !f)} title={full ? "Back to the list" : "Every column, scrolling sideways"}>
              {full ? "As a list" : "All columns"}
            </Button>
          )}
          <DropdownMenu>
            <DropdownMenuTrigger render={<Button size="sm" variant="ghost" />}><Columns3 className="size-4" /> Columns</DropdownMenuTrigger>
            <DropdownMenuContent align="end">
              {table.getAllLeafColumns().filter((c) => c.getCanHide()).map((c) => (
                <DropdownMenuCheckboxItem key={c.id} checked={c.getIsVisible()} onCheckedChange={(v) => c.toggleVisibility(!!v)}>
                  {String(c.columnDef.header)}
                </DropdownMenuCheckboxItem>
              ))}
            </DropdownMenuContent>
          </DropdownMenu>
          <Pressed size="sm" variant="secondary" onPress={loaded.refresh}>Refresh</Pressed>
          {coll.creatable && can("write", coll) && <Button size="sm" onClick={() => go({ view: "board", coll: coll.id, mode: "new" })}>New {coll.singular}</Button>}
        </div>
      </div>

      {picked.length > 0 && (
        <div className="mb-2 flex items-center gap-2 rounded-[4px] border px-3 py-1.5 text-xs"
          style={{ background: "var(--surface-sunken)", borderColor: "var(--border)" }}>
          <span style={{ color: "var(--text-strong)" }}>{picked.length} selected</span>
          {coll.id === "instances" && <>
            <Button size="sm" variant="secondary" onClick={() => bulk("Start", { spec: { desiredState: "Running" } })}>Start</Button>
            <Button size="sm" variant="secondary" onClick={() => bulk("Stop", { spec: { desiredState: "Stopped" } })}>Stop</Button>
          </>}
          {coll.deletable && <Button size="sm" variant="destructive" onClick={() => bulk("Delete", null, true)}>Delete</Button>}
          <Button size="sm" variant="ghost" className="ml-auto" onClick={() => setSelection({})}>Clear</Button>
        </div>
      )}

      {loaded.error && <p className="mb-2 text-sm" role="alert" style={{ color: "var(--failing)" }}>{loaded.error}</p>}

      <div ref={scroller} className="min-h-0 flex-1 overflow-auto rounded-[6px] border"
        style={{ background: "var(--surface)", borderColor: "var(--border)" }}>
        <table className="border-collapse text-[13px]" style={{ tableLayout: "fixed", width: tableW }}>
          <thead className="sticky top-0 z-10" style={{ background: "var(--surface-sunken)" }}>
            {table.getHeaderGroups().map((hg) => (
              <tr key={hg.id}>
                {hg.headers.map((h) => (
                  <th key={h.id} style={{ width: h.getSize(), borderColor: "var(--border)" }}
                    className="border-b px-4 py-2.5 text-left text-[12px] font-medium"
                    onClick={h.column.getCanSort() ? h.column.getToggleSortingHandler() : undefined}>
                    <span className="inline-flex cursor-pointer select-none items-center gap-1" style={{ color: "var(--text-muted)" }}>
                      {flexRender(h.column.columnDef.header, h.getContext())}
                      {h.column.getIsSorted() === "asc" && <ArrowUp className="size-3" />}
                      {h.column.getIsSorted() === "desc" && <ArrowDown className="size-3" />}
                    </span>
                  </th>
                ))}
              </tr>
            ))}
          </thead>
          <tbody style={{ height: virt.getTotalSize(), width: tableW }} className="relative block">
            {loaded.loading && !visible.length && [0, 1, 2, 3].map((i) => (
              <tr key={i} className="absolute left-0 flex w-full" style={{ top: i * rowH, height: rowH }}>
                <td className="px-3 py-3"><Skeleton className="h-3 w-[60%]" /></td>
              </tr>
            ))}
            {virt.getVirtualItems().map((vi) => {
              const row = visible[vi.index]; const r = row.original;
              const open = () => go({ view: "board", coll: coll.id, id: routeId(coll, r, project) });
              return (
                <tr key={row.id} data-changed={loaded.changed.has(nameOf(r)) || undefined}
                  className="absolute left-0 table cursor-pointer border-b"
                  style={{
                    top: vi.start, height: rowH, tableLayout: "fixed", width: tableW,
                    borderColor: "var(--border-subtle)",
                    background: idOf(r) === selectedId ? "var(--surface-hover)" : vi.index === cursor ? "color-mix(in srgb, var(--surface-hover) 55%, transparent)" : undefined,
                    boxShadow: vi.index === cursor ? "inset 2px 0 0 var(--brand)" : undefined,
                  }}
                  onClick={open} onMouseEnter={() => setCursor(vi.index)}>
                  {row.getVisibleCells().map((cell) => (
                    <td key={cell.id} style={{ width: cell.column.getSize() }} className="truncate px-4 align-middle">
                      {flexRender(cell.column.columnDef.cell, cell.getContext())}
                    </td>
                  ))}
                </tr>
              );
            })}
          </tbody>
        </table>
        {!loaded.loading && !visible.length && !loaded.error && (
          <p className="p-6 text-sm" style={{ color: "var(--text-muted)" }}>
            {globalFilter || kind || labels ? "Nothing matches." : `No ${coll.title.toLowerCase()} here yet.${coll.creatable ? " Create the first one above." : ""}`}
          </p>
        )}
      </div>
      <p className="pt-2 text-[11px]" style={{ color: "var(--text-faint)" }}>
        {visible.length} of {loaded.rows.length} · revision {loaded.revision || "—"} · <Live state={loaded.live} /> · j/k move, Enter opens, Space picks, / filters
        {loaded.truncated && <span style={{ color: "var(--drifting)" }}> · this list did not finish — narrow it with a filter or labels</span>}
      </p>
    </div>
  );
}

/**
 * Whether this board is keeping itself up to date.
 *
 * The third state is not decoration: it is the thing that tells somebody the
 * screen will not change on its own and they should press Refresh — which
 * otherwise they find out by waiting.
 */
function Live({ state }: { state: WatchState }) {
  const [label, tone, why] =
    state === "live" ? ["live", "var(--dot-settled)", "changes arrive as they happen"]
    : state === "connecting" ? ["connecting…", "var(--text-faint)", "opening the stream"]
    : state === "dropped" ? ["reconnecting…", "var(--dot-drifting)", "the stream dropped; it is being reopened"]
    : ["no live updates", "var(--text-faint)", "this board re-reads on its own clock and when you press Refresh"];
  return <span style={{ color: tone }} title={why}>{label}</span>;
}
