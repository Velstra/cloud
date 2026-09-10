// Every operation the API documents is reachable from this UI, or is named
// here as deliberately not. A claim a script can refuse, not a sentence.
//
//   node scripts/coverage.mjs
//
// The generic layers reach an operation by pattern rather than by name:
// `list:<coll>`, `get:<coll>`, `create:<coll>`, `patch:<coll>`, `delete:<coll>`
// are what the board, the detail and the form call for every collection the
// schema names. Custom actions (`…:explainPlacement`) are found by the
// registry from the OpenAPI document and offered as buttons. So coverage is:
// for each documented operation, is there a generic path that calls it, a
// registry path, or a direct call by id somewhere in src/?

import { readFileSync, readdirSync, statSync } from "node:fs";
import { join } from "node:path";

const ops = JSON.parse(readFileSync(new URL("../src/api/operations.json", import.meta.url)));
const schema = JSON.parse(readFileSync(new URL("../src/schema.json", import.meta.url)));
const notSurfaced = JSON.parse(readFileSync(new URL("../src/api/not-surfaced.json", import.meta.url)));

const walk = (d) => readdirSync(d).flatMap((f) => {
  const p = join(d, f);
  return statSync(p).isDirectory() ? walk(p) : /\.(tsx?|mjs)$/.test(f) ? [readFileSync(p, "utf8")] : [];
});
const src = walk(new URL("../src", import.meta.url).pathname).join("\n");

const collections = new Set(schema.map((c) => c.id));
const collOf = (path) => (/\/api\/v1\/(?:projects\/\{project\}\/)?([a-z-]+)/.exec(path) ?? [])[1];
const isItem = (path) => /\/\{(name|id)\}$/.test(path);
const isCustom = (path) => /:[a-zA-Z]+$/.test(path);

const reached = (o) => {
  const c = collOf(o.path);
  if (src.includes(`"${o.id}"`)) return "by id";
  if (isCustom(o.path) && c && collections.has(c)) return "registry action";
  if (c && collections.has(c) && !isCustom(o.path)) {
    if (o.method === "GET" && !isItem(o.path)) return "board list";
    if (o.method === "GET" && isItem(o.path)) return "detail get";
    if (o.method === "POST" && !isItem(o.path)) return schema.find((x) => x.id === c).creatable ? "form create" : null;
    if (o.method === "PATCH" && isItem(o.path)) return schema.find((x) => x.id === c).editable ? "form patch" : null;
    if (o.method === "DELETE" && isItem(o.path)) return schema.find((x) => x.id === c).deletable ? "detail delete" : null;
  }
  return null;
};

let covered = 0; const missing = [];
for (const o of ops) {
  const how = reached(o);
  if (how) covered++;
  else if (notSurfaced[o.id]) covered++;
  else missing.push(o);
}
console.log(`${covered}/${ops.length} operations reachable or accounted for`);
for (const o of missing) console.log(`  MISSING ${o.method} ${o.path}  (${o.id})`);
process.exit(missing.length ? 1 : 0);
