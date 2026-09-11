// Is this a legal answer, said while it is being typed.
//
// Ported from the other console's `form.js` rather than written again, because
// the schema names its validators (`check: "cidr"`) instead of carrying
// regexes, and the reason is in the Rust doc: "so the script owns one
// implementation of each and the schema cannot invent a seventh dialect of 'is
// this an address'". Two consoles with two dialects is exactly what that
// prevents, so these are the same functions and the same sentences.
//
// A field is checked as it is typed, never only on submit: a form that accepts
// six fields and then rejects the second is a form that wasted somebody's time
// on purpose.

const v4 = (s: string) =>
  /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/.test(s) &&
  s.split(".").every((o) => Number(o) <= 255);

const v6 = (s: string) => /^[0-9a-fA-F:]+$/.test(s) && s.includes(":") && !/:::/.test(s);

/** Named in the schema's `Check`. A name nobody has is no opinion, not a crash. */
export const CHECKS: Record<string, (s: string) => string> = {
  none: () => "",
  id: (s) =>
    /^[a-z0-9][a-z0-9.-]*$/.test(s)
      ? ""
      : "lowercase letters, digits, '-' and '.' only — an id that has to be quoted is one something downstream will mis-split",
  address: (s) => (v4(s) || v6(s) ? "" : "not an address"),
  cidr: (s) => {
    const [addr, len, ...rest] = s.split("/");
    if (rest.length || len === undefined || len === "") return "expected address/prefix, like 10.0.0.0/24";
    const max = v4(addr) ? 32 : v6(addr) ? 128 : -1;
    if (max < 0) return "not an address";
    const n = Number(len);
    return Number.isInteger(n) && n >= 0 && n <= max ? "" : "the prefix must be 0–" + max;
  },
  mac: (s) =>
    /^([0-9a-fA-F]{2}:){5}[0-9a-fA-F]{2}$/.test(s) ? "" : "six hex pairs, like 02:1a:4b:00:11:22",
  digest: (s) =>
    /^sha256:[0-9a-f]{64}$/.test(s) || /^sha512:[0-9a-f]{128}$/.test(s)
      ? ""
      : "sha256: followed by 64 hex characters, or sha512: followed by 128",
  url: (s) => (/^[a-z][a-z0-9+.-]*:\/\/.+/i.test(s) ? "" : "expected a URL with a scheme"),
  name: (s) =>
    s.split("/").length % 2 === 0 && s.split("/").every(Boolean)
      ? ""
      : "a resource name is collection/id pairs, like projects/p1/images/x",
};

/**
 * A list is checked entry by entry — not as the string it stringifies to.
 *
 * The other console records why: `[]` became "" (which is not the empty string
 * the shortcut below looks for) and failed the id check, and `["a", "b"]`
 * became "a,b" and failed it on the comma. So an edit of any guest with no
 * required node labels — which is every guest — was stopped at "Fix Required
 * node labels first", with no entry on screen to fix.
 */
export function check(kind: string | undefined, value: unknown): string {
  if (Array.isArray(value)) return value.map((v) => check(kind, v)).find(Boolean) || "";
  if (value === "" || value === null || value === undefined) return "";
  return (CHECKS[kind ?? "none"] ?? CHECKS.none)(String(value));
}

/**
 * Checks that need two fields at once.
 *
 * Kept apart from the per-field ones because they can only run once both have
 * been answered, and complaining about a gateway before the range exists is
 * nagging, not validating.
 */
export function crossCheck(id: string, values: Record<string, unknown>): Record<string, string> {
  const bad: Record<string, string> = {};
  if (id === "subnets" && values.cidr && values.gateway) {
    const inside = v4Inside(String(values.gateway), String(values.cidr));
    if (inside === false) bad.gateway = "outside " + values.cidr;
  }
  return bad;
}

/** `null` when it cannot be answered: v6 is not checked rather than checked wrongly. */
export function v4Inside(addr: string, cidr: string): boolean | null {
  const [net, len] = String(cidr).split("/");
  if (!v4(addr) || !v4(net)) return null;
  const n = (s: string) => s.split(".").reduce((a, o) => a * 256 + Number(o), 0);
  const bits = Number(len);
  if (!Number.isInteger(bits) || bits < 0 || bits > 32) return null;
  const mask = bits === 0 ? 0 : (0xffffffff << (32 - bits)) >>> 0;
  return ((n(addr) & mask) >>> 0) === ((n(net) & mask) >>> 0);
}
