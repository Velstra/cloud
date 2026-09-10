// A form, from the schema's fields. Every kind the schema names renders here;
// the five structured-list kinds (disks, grants, listeners, pools, rules) get
// a row editor over their JSON shape rather than a bespoke mask each, which
// is honest about what this build knows and keeps them editable.

import { createContext, useContext, useEffect, useMemo, useState } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { Textarea } from "@/components/ui/textarea";
import {
  Select, SelectContent, SelectItem, SelectTrigger, SelectValue,
} from "@/components/ui/select";
import { call, ApiError } from "@/api/transport";
import { ALL, basePath, collection, projectOf, type Collection, type Field } from "@/lib/schema";
import { idOf, nameOf, type Resource } from "@/lib/model";
import { useStore } from "@/app/store";
import { projectNames } from "@/lib/listing";
import { Pressed } from "@/features/Pressed";
import { z } from "zod";
import { entry } from "@/registry";

type Values = Record<string, any>;

const initial = (c: Collection, r?: Resource): Values => {
  const v: Values = {};
  for (const f of c.fields) {
    const cur = r?.spec?.[f.key];
    v[f.key] = cur !== undefined ? cur
      : f.kind === "switch" ? false
      : /List$/.test(f.kind) ? []
      : f.kind === "number" ? "" : "";
  }
  return v;
};

export function Form({ coll, existing, onDone, onCancel }: {
  coll: Collection; existing?: Resource; onDone: (r: Resource) => void; onCancel: () => void;
}) {
  const storeProject = useStore((s) => s.project);
  // The project this form writes to: an existing object's own, or — when the
  // picker says every project — the one chosen at the top of the form.
  const [formProject, setFormProject] = useState(existing ? projectOf(nameOf(existing)) ?? storeProject : storeProject === ALL ? "" : storeProject);
  const project = formProject;
  const [names, setNames] = useState<string[]>([]);
  useEffect(() => { if (!existing && storeProject === ALL && coll.scope === "project") projectNames().then(setNames).catch(() => setNames([])); }, [existing, storeProject, coll.scope]);
  const [id, setId] = useState(existing ? idOf(existing) : "");
  const [values, setValues] = useState<Values>(() => initial(coll, existing));
  // The platform's only tagging mechanism. Every board can filter on labels
  // and the API takes them on create and change — and until now nothing but an
  // API client could write one, so cost attribution, environment separation
  // and `placementPolicy.requiredLabels` were all out of reach from here.
  const [labels, setLabels] = useState<string>(() =>
    Object.entries((existing?.meta.labels ?? {}) as Record<string, string>)
      .map(([k, v]) => `${k}=${v}`)
      .join(", "),
  );
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [problem, setProblem] = useState("");
  const [showAdvanced, setShowAdvanced] = useState(false);

  const fields = coll.fields.filter((f) => !f.derived && (existing ? !f.atCreation || true : true));
  const basic = fields.filter((f) => !f.advanced);
  const advanced = fields.filter((f) => f.advanced);

  const set = (k: string, v: unknown) => setValues((s) => ({ ...s, [k]: v }));

  // A flavor is a size. Picking one fills the three numbers it stands for, so
  // the form shows what will be asked for and nobody types a size twice; the
  // API copies them from the flavor anyway. Clearing the flavor leaves the
  // numbers, now as a custom size.
  const flavorField = fields.find((f) => f.kind === "ref" && f.collection === "flavors");
  useEffect(() => {
    const chosen = flavorField && values[flavorField.key];
    if (!chosen) return;
    const id = String(chosen).split("/").pop()!;
    call("get:flavors", "GET", `/api/v1/flavors/${encodeURIComponent(id)}`).then((fl) => {
      const spec = fl?.spec ?? {};
      setValues((s) => ({ ...s,
        ...(spec.vcpus != null ? { vcpus: spec.vcpus } : {}),
        ...(spec.memoryMib != null ? { memoryMib: spec.memoryMib } : {}),
        ...(spec.rootDiskGib != null ? { rootDiskGib: spec.rootDiskGib } : {}),
      }));
    }).catch(() => { /* the API says so on submit */ });
  }, [flavorField ? values[flavorField.key] : null]);
  const sized = !!(flavorField && values[flavorField.key]) || (values.vcpus && values.memoryMib);

  // What each field will accept, said once here and checked before the wire
  // is touched. The API still has the last word; this is the first.
  const shape = useMemo(() => z.object(Object.fromEntries(fields.map((f) => {
    let v: z.ZodTypeAny =
      f.kind === "number" ? z.union([z.literal(""), z.coerce.number({ message: "A number." }).finite()])
      : f.kind === "switch" ? z.boolean()
      : f.kind === "moment" ? z.union([z.literal(""), z.number().int().positive()])
      : /List$/.test(f.kind) ? z.array(z.any())
      : z.string();
    if (f.required) v = v.refine((x) => x !== "" && x != null && !(Array.isArray(x) && !x.length), { message: "Still needed." });
    return [f.key, v];
  }))), [fields]);

  const submit = async () => {
    setProblem(""); setErrors({});
    if (!existing && !/^[a-z0-9][a-z0-9-]{0,62}$/.test(id.trim())) { setProblem(id.trim() ? "A name is lowercase letters, digits and dashes, up to 63." : "A name is still needed."); return; }
    if (flavorField && !sized) { setErrors({ [flavorField.key]: "Pick a flavor, or set a custom size under advanced settings." }); setProblem("A size is still needed: pick a flavor, or set vCPUs and memory under advanced settings."); return; }
    const checked = shape.safeParse(values);
    if (!checked.success) {
      const errs: Record<string, string> = {};
      for (const issue of checked.error.issues) errs[String(issue.path[0])] = issue.message;
      setErrors(errs);
      setProblem("Fix " + Object.keys(errs).map((k) => fields.find((f) => f.key === k)?.label ?? k).join(", ") + " first.");
      return;
    }
    const spec: Values = {};
    for (const f of fields) {
      const v = values[f.key];
      if (v === "" || v == null) continue;
      if (existing && f.atCreation && existing.spec?.[f.key] !== undefined) continue;
      spec[f.key] = f.kind === "number" ? Number(v) : v;
    }
    if (coll.scope === "project" && !project) { setProblem("Choose the project first."); return; }
    const written = labelsFrom(labels);
    if (written === null) {
      setProblem("Labels are `key=value`, separated by commas.");
      return;
    }
    try {
      let saved: Resource;
      if (existing) {
        // The revision this edit was read at goes in `If-Match`, not in the
        // body — the API refuses `meta.revision` from a client — so a colleague's
        // change in between is refused here rather than overwritten.
        saved = await call(`patch:${coll.id}`, "PATCH",
          `${basePath(coll, project)}/${encodeURIComponent(idOf(existing))}`,
          undefined, { spec, labels: written }, existing.meta.revision ? { "if-match": String(existing.meta.revision) } : undefined);
      } else {
        const name = coll.scope === "project"
          ? `projects/${project}/${coll.id}/${id.trim()}` : `${coll.id}/${id.trim()}`;
        saved = await call(`create:${coll.id}`, "POST", basePath(coll, project), undefined,
          { meta: { name }, spec, labels: written });
      }
      onDone(saved);
    } catch (e) {
      const err = e as ApiError;
      const key = (err.field ?? "").replace(/^spec\./, "");
      if (key && fields.some((f) => f.key === key)) setErrors({ [key]: err.message });
      setProblem(err.message);
    }
  };

  return (
    <FormProject.Provider value={project}>
    <form className="grid gap-5" onSubmit={(e) => { e.preventDefault(); submit(); }}>
      {!existing && storeProject === ALL && coll.scope === "project" && (
        <Row label="Project" help="Every project is on the board; this one gets the new object." error={!project && problem ? "Still needed." : undefined}>
          <select value={project} onChange={(e) => setFormProject(e.target.value)} autoFocus
            className="h-9 rounded-[4px] border px-2 text-sm" style={{ borderColor: "var(--border)", background: "var(--surface)", color: "var(--text-body)" }}>
            <option value="">Choose…</option>
            {names.map((n) => <option key={n} value={n}>{n}</option>)}
          </select>
        </Row>
      )}
      {!existing && (
        <Row label="Name" help={`Becomes ${coll.scope === "project" ? `projects/${project}/` : ""}${coll.id}/…`} error={!id.trim() && problem ? "Still needed." : ""}>
          <Input value={id} onChange={(e) => setId(e.target.value)} autoFocus placeholder={coll.singular + "-1"} />
        </Row>
      )}
      {basic.map((f) => (
        <FieldRow key={f.key} f={f} coll={coll} value={values[f.key]} onChange={(v) => set(f.key, v)}
          error={errors[f.key]} locked={!!existing && f.atCreation} />
      ))}
      <Row label="Labels" help="`key=value`, separated by commas. Every board filters on them, and placement rules can require one.">
        <Input value={labels} onChange={(e) => setLabels(e.target.value)} placeholder="env=prod, tier=web" className="font-mono text-xs" />
      </Row>
      {advanced.length > 0 && (
        <div>
          <button type="button" className="text-xs underline-offset-2 hover:underline"
            style={{ color: "var(--text-muted)" }} onClick={() => setShowAdvanced((s) => !s)}>
            {showAdvanced ? "Hide" : "Show"} {advanced.length} advanced {advanced.length === 1 ? "setting" : "settings"}
          </button>
          {showAdvanced && (
            <div className="mt-4 grid gap-5">
              {advanced.map((f) => (
                <FieldRow key={f.key} f={f} coll={coll} value={values[f.key]} onChange={(v) => set(f.key, v)}
                  error={errors[f.key]} locked={!!existing && f.atCreation} />
              ))}
            </div>
          )}
        </div>
      )}
      {problem && <p className="text-sm" role="alert" style={{ color: "var(--failing)" }}>{problem}</p>}
      <div className="flex justify-end gap-2 pt-2">
        <Button type="button" variant="secondary" onClick={onCancel}>Cancel</Button>
        <Pressed type="submit" onPress={submit}>{existing ? "Save" : "Create"}</Pressed>
      </div>
    </form>
    </FormProject.Provider>
  );
}

/** The project the form writes to, for the pickers inside it: their lists
 *  come from that project, not from whatever the rail's picker says. */
const FormProject = createContext<string | null>(null);

/// `key=value` pairs, or `null` when that is not what was typed.
///
/// Empty is an empty map and not a refusal: clearing the box is how a label is
/// taken off, and a form that could only add them would be half a control.
function labelsFrom(raw: string): Record<string, string> | null {
  const out: Record<string, string> = {};
  for (const pair of raw.split(",").map((p) => p.trim()).filter(Boolean)) {
    const at = pair.indexOf("=");
    if (at <= 0) return null;
    out[pair.slice(0, at).trim()] = pair.slice(at + 1).trim();
  }
  return out;
}

function Row({ label, help, error, children }: {
  label: string; help?: string; error?: string; children: React.ReactNode;
}) {
  return (
    <div className="grid gap-1.5">
      <Label className="text-[13px]" style={{ color: "var(--text-strong)" }}>{label}</Label>
      {children}
      {error ? <p className="text-xs" style={{ color: "var(--failing)" }}>{error}</p>
        : help ? <p className="text-xs" style={{ color: "var(--text-faint)" }}>{help.replace(/\*\*(.+?)\*\*/g, "$1").replace(/`(.+?)`/g, "$1")}</p> : null}
    </div>
  );
}

function FieldRow({ f, coll, value, onChange, error, locked }: {
  f: Field; coll: Collection; value: any; onChange: (v: any) => void; error?: string; locked: boolean;
}) {
  const label = f.label + (f.required ? " *" : "");
  const help = locked ? "Answered once, when the object was made." : f.help;
  const common = { disabled: locked, "aria-invalid": !!error || undefined };

  // A field the registry knows better than the schema does.
  const editor = entry(coll.id).fieldEditors?.[f.key];
  if (editor) return <Row label={label} help={help} error={error}>{editor({ value, onChange, disabled: locked })}</Row>;

  switch (f.kind) {
    case "switch":
      return (
        <Row label={label} help={help} error={error}>
          <div className="flex items-center gap-3">
            <Switch checked={!!value} onCheckedChange={onChange} disabled={locked} />
            <span className="text-xs" style={{ color: "var(--text-muted)" }}>{value ? "on" : "off"}</span>
          </div>
        </Row>
      );
    case "choice":
      return (
        <Row label={label} help={help} error={error}>
          <Select value={value || ""} onValueChange={onChange} disabled={locked}>
            <SelectTrigger><SelectValue placeholder={f.whenEmpty || "Choose…"} /></SelectTrigger>
            <SelectContent>
              {(f.options ?? []).map((o) => <SelectItem key={o.value} value={o.value}>{o.label}</SelectItem>)}
            </SelectContent>
          </Select>
        </Row>
      );
    case "number":
      return (
        <Row label={label} help={help} error={error}>
          <Input {...common} inputMode="numeric" value={value ?? ""} onChange={(e) => onChange(e.target.value)} placeholder={f.whenEmpty} />
        </Row>
      );
    case "moment":
      return (
        <Row label={label} help={help} error={error}>
          <Input {...common} type="datetime-local"
            value={value ? new Date(Number(value) || value).toISOString().slice(0, 16) : ""}
            onChange={(e) => onChange(e.target.value ? new Date(e.target.value).getTime() : "")} />
        </Row>
      );
    case "lines":
      return (
        <Row label={label} help={help} error={error}>
          <Textarea {...common} rows={5} value={value ?? ""} onChange={(e) => onChange(e.target.value)} className="font-mono text-xs" />
        </Row>
      );
    case "textList":
      return (
        <Row label={label} help={help ?? "One per line."} error={error}>
          <Textarea {...common} rows={3} value={(value ?? []).join("\n")}
            onChange={(e) => onChange(e.target.value.split("\n").map((s) => s.trim()).filter(Boolean))} className="font-mono text-xs" />
        </Row>
      );
    case "ref":
      return (
        <Row label={label} help={help} error={error}>
          <RefPicker f={f} value={value ?? ""} onChange={onChange} disabled={locked} />
        </Row>
      );
    case "refList":
      return (
        <Row label={label} help={help} error={error}>
          <RefPicker f={f} value={value ?? []} onChange={onChange} disabled={locked} multiple />
        </Row>
      );
    default:
      if (/List$/.test(f.kind)) {
        return (
          <Row label={label} help={help} error={error}>
            <ListEditor kind={f.kind} value={Array.isArray(value) ? value : []} onChange={onChange} disabled={locked} />
          </Row>
        );
      }
      return (
        <Row label={label} help={help} error={error}>
          <Input {...common} value={value ?? ""} onChange={(e) => onChange(e.target.value)} placeholder={f.whenEmpty} />
        </Row>
      );
  }
}

/** What already exists is offered rather than typed. */
function RefPicker({ f, value, onChange, disabled, multiple }: {
  f: Field; value: any; onChange: (v: any) => void; disabled: boolean; multiple?: boolean;
}) {
  const storeProject = useStore((s) => s.project);
  const formProject = useContext(FormProject);
  const project = formProject || storeProject;
  const target = f.collection ? collection(f.collection) : undefined;
  const [options, setOptions] = useState<{ id: string; name: string }[]>([]);
  useEffect(() => {
    if (!target || (target.scope === "project" && (!project || project === ALL))) { setOptions([]); return; }
    call(`list:${target.id}`, "GET", basePath(target, project), { pageSize: 200 })
      .then((a) => setOptions((a.items ?? []).map((r: Resource) => ({ id: idOf(r), name: r.meta.name }))))
      .catch(() => setOptions([]));
  }, [target, project]);
  const spell = (o: { id: string; name: string }) => (f.spelling === "name" ? o.name : o.id);

  if (multiple) {
    const chosen: string[] = value ?? [];
    return (
      <div className="flex flex-wrap gap-2">
        {options.map((o) => {
          const v = spell(o); const on = chosen.includes(v);
          return (
            <button key={o.id} type="button" disabled={disabled} aria-pressed={on}
              onClick={() => onChange(on ? chosen.filter((x) => x !== v) : [...chosen, v])}
              className="rounded-[3px] border px-2 py-0.5 text-xs"
              style={{ borderColor: on ? "var(--focus-ring)" : "var(--border)", background: on ? "var(--surface-hover)" : "var(--surface-sunken)" }}>
              {o.id}
            </button>
          );
        })}
        {!options.length && <span className="text-xs" style={{ color: "var(--text-faint)" }}>Nothing to pick from yet.</span>}
      </div>
    );
  }
  return (
    <Select value={value || ""} onValueChange={onChange} disabled={disabled}>
      <SelectTrigger><SelectValue placeholder={f.whenEmpty || `Pick a ${target?.singular ?? "value"}…`} /></SelectTrigger>
      <SelectContent>
        {options.map((o) => <SelectItem key={o.id} value={spell(o)}>{o.id}</SelectItem>)}
      </SelectContent>
    </Select>
  );
}

/** Structured lists — disks, grants, listeners, pools, rules — as rows of keys. */
function ListEditor({ kind, value, onChange, disabled }: {
  kind: string; value: Record<string, any>[]; onChange: (v: any) => void; disabled: boolean;
}) {
  const keys = useMemo(() => {
    const seen = new Set<string>();
    for (const row of value) for (const k of Object.keys(row)) seen.add(k);
    if (!seen.size) for (const k of DEFAULT_KEYS[kind] ?? []) seen.add(k);
    return [...seen];
  }, [value, kind]);
  const update = (i: number, k: string, v: string) =>
    onChange(value.map((row, j) => (j === i ? { ...row, [k]: coerce(v) } : row)));
  if (!keys.length) {
    return (
      <p className="text-xs" style={{ color: "var(--drifting)" }}>
        This list has no editor in the console yet, and guessing its field names would write fields the API silently drops. Set it through the API, or ask for an editor.
      </p>
    );
  }
  return (
    <div className="grid gap-2">
      {value.map((row, i) => (
        <div key={i} className="grid gap-2" style={{ gridTemplateColumns: `repeat(${keys.length}, minmax(0,1fr)) auto` }}>
          {keys.map((k) => (
            <Input key={k} disabled={disabled} placeholder={k} value={row[k] ?? ""} className="font-mono text-xs"
              onChange={(e) => update(i, k, e.target.value)} />
          ))}
          <Button type="button" variant="ghost" size="sm" disabled={disabled}
            onClick={() => onChange(value.filter((_, j) => j !== i))}>Remove</Button>
        </div>
      ))}
      <div>
        <Button type="button" variant="secondary" size="sm" disabled={disabled}
          onClick={() => onChange([...value, Object.fromEntries(keys.map((k) => [k, ""]))])}>
          Add {kind.replace(/List$/, "")}
        </Button>
      </div>
    </div>
  );
}

// Only kinds whose wire shape is flat and whose key names are known belong
// here. Everything else has a real editor in the registry: guessing a key
// name writes a field the API does not have, and serde drops it without a
// word — a listener configured 443 -> 8080 arrived as 443 -> 443.
const DEFAULT_KEYS: Record<string, string[]> = {
  diskList: ["node", "device"],
};

const coerce = (v: string) => (/^-?\d+$/.test(v) ? Number(v) : v === "true" ? true : v === "false" ? false : v);
