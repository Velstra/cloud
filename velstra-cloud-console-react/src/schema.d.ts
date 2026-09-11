// Generated from schema.json. Do not edit:
// VELSTRA_WRITE_SCHEMA=1 cargo test -p velstra-cloud-console --test react_schema
//
// Derived from the document itself rather than from the Rust types, so a
// key the schema carries is a key TypeScript knows about — and a key it
// does not carry for this kind of field is a compile error rather than a
// value that is quietly `undefined`.

export type Field =
  | { kind: "ref"; advanced: boolean; atCreation: boolean; collection: string; derived: boolean; filterBy: null | string; help: string; key: string; label: string; required: boolean; spelling: string; whenEmpty: string }
  | { kind: "number"; advanced: boolean; atCreation: boolean; derived: boolean; help: string; key: string; label: string; max: number; min: number; required: boolean; scale: string; step: number; unit: string; whenEmpty: string }
  | { kind: "choice"; advanced: boolean; atCreation: boolean; derived: boolean; help: string; key: string; label: string; options: { label: string; value: string }[]; required: boolean; whenEmpty: string }
  | { kind: "refList"; advanced: boolean; also?: string; atCreation: boolean; collection: string; derived: boolean; help: string; key: string; label: string; required: boolean; spelling: string; whenEmpty: string }
  | { kind: "textList"; advanced: boolean; atCreation: boolean; check: string; derived: boolean; help: string; key: string; label: string; placeholder: string; required: boolean; whenEmpty: string }
  | { kind: "lines"; advanced: boolean; atCreation: boolean; derived: boolean; help: string; key: string; label: string; placeholder: string; required: boolean; whenEmpty: string }
  | { kind: "text"; advanced: boolean; atCreation: boolean; check: string; derived: boolean; help: string; key: string; label: string; placeholder: string; required: boolean; whenEmpty: string }
  | { kind: "switch"; advanced: boolean; atCreation: boolean; derived: boolean; help: string; key: string; label: string; required: boolean; whenEmpty: string }
  | { kind: "ruleList"; advanced: boolean; atCreation: boolean; derived: boolean; help: string; key: string; label: string; remoteCollection: string; required: boolean; whenEmpty: string }
  | { kind: "moment"; advanced: boolean; atCreation: boolean; defaultInMinutes: number; derived: boolean; help: string; key: string; label: string; required: boolean; whenEmpty: string }
  | { kind: "listenerList"; advanced: boolean; atCreation: boolean; derived: boolean; help: string; key: string; label: string; required: boolean; whenEmpty: string }
  | { kind: "diskList"; advanced: boolean; atCreation: boolean; collection: string; derived: boolean; help: string; key: string; label: string; minGib: number; refusals: { kind: string; text: string }[]; required: boolean; tooSmall: string; unknown: string; warning: string; whenEmpty: string }
  | { kind: "poolList"; advanced: boolean; atCreation: boolean; defaultMinSize: number; defaultSize: number; derived: boolean; help: string; key: string; label: string; required: boolean; whenEmpty: string }
  | { kind: "grantList"; advanced: boolean; atCreation: boolean; derived: boolean; help: string; key: string; label: string; required: boolean; whenEmpty: string }
;

export type Column =
  | { cell: "text"; label: string; path: string; width: number }
  | { cell: "mono"; label: string; path: string; width: number }
  | { cell: "number"; label: string; path: string; unit: string; width: number }
  | { cell: "bytes"; label: string; path: string; width: number }
  | { cell: "yes"; label: string; no: string; path: string; width: number; yes: string }
  | { cell: "count"; label: string; path: string; width: number }
  | { cell: "ago"; label: string; path: string; width: number }
;
