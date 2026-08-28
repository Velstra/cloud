// The API, and only the API. Nothing on this page fetches anything else.

const TOKEN_KEY = "velstra-cloud-token";
const PROJECT_KEY = "velstra-cloud-project";

// The token lives in sessionStorage: it is gone when the tab closes, because a
// cloud token has no business outliving the session on a shared machine.
const session = {
  token: sessionStorage.getItem(TOKEN_KEY) || "",
  project: sessionStorage.getItem(PROJECT_KEY) || "",
  // Who the API says is signed in, filled in on entry. Never persisted: it is
  // the server's answer about the current token, and a stale copy from a
  // previous session would be a claim about the wrong person.
  who: null,
  // The label filter in force, as typed: `env=prod,tier=web`.
  //
  // Not persisted, deliberately. A filter that outlived the session would greet
  // somebody with a short list and no visible reason — and "where did my guests
  // go" is a bad first question. It lives for as long as the board it was typed
  // on is open.
  labels: "",
};

const collections = () => SCHEMA;
const collection = (id) => SCHEMA.find((c) => c.id === id) || null;

/// A collection's scope is declared in the schema, but the contract does not
/// spell out where a node is addressed from, and a console that guesses wrong
/// shows an empty fleet rather than an error. So the first 404 on a listing
/// tries the other scope once and remembers the answer for the session.
const scopeFound = {};

/// Where a **write** goes: the scope the schema declares, always.
///
/// Never the probed one. The probe below exists to find where an unfamiliar
/// collection can be *read* from; letting it decide where a create goes was a
/// bug with a very quiet failure — see `list`. A create belongs where the
/// contract says it belongs.
/// Where a write goes: the contract's scope, never the probed one.
///
/// `scope` overrides it for the collections that genuinely have two —
/// `BOTH_SCOPES` — where a cell operator may put an object under a project or
/// under the cell itself. An image is the case: a catalogue everybody may boot
/// is a cell-wide object, and the console could only ever make project ones, so
/// the one thing an administrator most wants to publish was the one thing this
/// interface could not.
function writePath(coll, scope) {
  return basePath(coll, scope || coll.scope);
}

/// Whether this collection can be written at both levels, and this session may.
///
/// Both halves matter: an ordinary tenant sees no choice at all, because for
/// them there is none — and offering one that is refused on submit is worse than
/// not offering it.
function offersBothScopes(coll) {
  return BOTH_SCOPES.includes(coll.id) && !!(session.who && session.who.cellAdmin);
}

function basePath(coll, scope) {
  const s = scope || scopeFound[coll.id] || coll.scope;
  return s === "project"
    ? "/api/v1/projects/" + encodeURIComponent(session.project) + "/" + coll.id
    : "/api/v1/" + coll.id;
}

class ApiError extends Error {
  constructor(status, body) {
    const e = (body && body.error) || {};
    super(e.message || body?.message || ("the API answered " + status));
    this.status = status;
    this.code = e.code || "";
    this.field = e.field || "";
    this.body = body;
  }
}

async function request(method, path, opts = {}) {
  const headers = { Accept: "application/json" };
  if (session.token) headers.Authorization = "Bearer " + session.token;
  if (opts.body !== undefined) headers["Content-Type"] = "application/json";
  // Without If-Match a write is last-writer-wins, and the client said so by
  // omission. The console sends the revision it read, so an edit made from a
  // stale sheet is refused rather than quietly overwriting somebody.
  if (opts.ifMatch) headers["If-Match"] = opts.ifMatch;

  const res = await fetch(path, {
    method,
    headers,
    body: opts.body === undefined ? undefined : JSON.stringify(opts.body),
    signal: opts.signal,
  });

  let body = null;
  const text = await res.text();
  if (text) { try { body = JSON.parse(text); } catch (e) { body = { message: text.slice(0, 400) }; } }

  if (res.status === 401) { signedOut("The token was refused."); throw new ApiError(401, body); }
  if (!res.ok) throw new ApiError(res.status, body);
  return { body, revision: res.headers.get("X-Velstra-Revision") };
}

/// The contract fixes the resource body but not the envelope a listing arrives
/// in. Take the array however it is wrapped rather than insisting on one
/// spelling — this is the one place the console is deliberately permissive, and
/// it is written down in the report rather than hidden.
function itemsOf(body, coll) {
  if (Array.isArray(body)) return body;
  if (!body || typeof body !== "object") return [];
  for (const key of [coll.id, "items", "resources", "results"]) {
    if (Array.isArray(body[key])) return body[key];
  }
  const found = Object.values(body).find((v) => Array.isArray(v));
  return found || [];
}

// ---- listing, a page at a time ---------------------------------------------
//
// **The console holds whole collections and it must go on holding them.** The
// board sorts client-side, the rail counts, and the watch folds each event into
// the list it already has — none of those can be done over one page of an
// unknown many. So paging here is a transport detail: the walk happens inside
// `list`, and everything above it still gets a collection.
//
// The alternative — a visible "next page" control — was rejected rather than
// missed. It would mean a count that is the size of one page, a sort that is
// only within a page, and a watch event for an object on another page with
// nowhere to land: three ways to be quietly wrong, to save round trips on
// collections that in this platform are hundreds of objects, not millions.
//
// Two things about the walk are the contract and not choices:
//
//  * **The revision is the *first* page's.** A client lists, then watches from
//    what the list reported. Take the last page's revision and every change that
//    landed while the walk was running is skipped for ever — silently, because
//    the board looks complete and the watch looks live. The API pins it in the
//    page token and repeats it on every page; this takes the first one it saw
//    and keeps it.
//  * **The walk ends when there is no token, not when a page is short.** A full
//    last page with no token is a finished walk, and a short page with a token
//    is not — stopping on a short page is the natural mistake and it stops the
//    walk in the middle.

/// What the console asks for per page.
///
/// The API's own maximum, so a cell of a thousand objects is still one round
/// trip, and a larger one is bounded work per response instead of one answer
/// that has to build and serialise the whole cell. The number is not load-
/// bearing: the walk below is correct at any page size, including a server
/// whose ceiling is lower than this.
const PAGE_SIZE = 1000;

/// A walk that cannot end is a tab that never stops fetching. Reaching this is a
/// server that keeps handing out tokens, so it is reported rather than absorbed
/// — see `complete` below.
const MOST_PAGES = 200;

function pagePath(coll, scope, token, narrow) {
  const q = "pageSize=" + PAGE_SIZE + (token ? "&pageToken=" + encodeURIComponent(token) : "");
  // Narrowed by the API rather than here. Filtering client-side would mean
  // fetching the whole cell to show six rows of it, which is exactly the cost
  // a filter exists to avoid on the boards where anybody needs one.
  //
  // Passed in rather than read off the session, because only one caller wants
  // it. A filter is about the board somebody typed it on; a picker inside a
  // form is not that board, and one that quietly offered two of the eleven
  // volumes in the project would be a wrong answer with no visible cause.
  const labels = narrow ? "&labels=" + encodeURIComponent(narrow) : "";
  return basePath(coll, scope) + "?" + q + labels;
}

/// Follow `nextPageToken` from a first page already in hand.
async function walk(coll, scope, first, narrow) {
  const items = first.items.slice();
  let token = first.next;
  let pages = 1;
  while (token) {
    if (pages >= MOST_PAGES) {
      // Loud, not silent. Everything read so far is still handed back — an
      // operator staring at a truncated fleet is better served by the rows plus
      // a sentence than by an error page with nothing in it — but `complete` is
      // false and the board says so.
      return { items, revision: first.revision, complete: false };
    }
    const r = await request("GET", pagePath(coll, scope, token, narrow));
    const page = itemsOf(r.body, coll);
    const next = r.body && r.body.nextPageToken;
    // A server that answers a token with the same token would spin here for
    // ever; a page that adds nothing and still offers a token is the same
    // failure one step removed.
    if (next === token || (!page.length && next)) {
      return { items, revision: first.revision, complete: false };
    }
    items.push(...page);
    token = next;
    pages++;
  }
  return { items, revision: first.revision, complete: true };
}

/// One page, and what the contract said about it.
async function firstPage(coll, scope, narrow) {
  const r = await request("GET", pagePath(coll, scope, null, narrow));
  const items = itemsOf(r.body, coll);
  return {
    items,
    // Header, then the body's own field, then — only if neither is there — the
    // newest revision on *this page*. Never over the whole walk: a revision
    // taken from a later page is one the watch would start after, and the
    // fallback must err towards replaying rather than skipping.
    revision: r.revision || (r.body && r.body.revision) || newestRevision(items),
    next: (r.body && r.body.nextPageToken) || null,
  };
}

/// Collections a person expects to see whole, wherever the objects live.
///
/// Images are the one so far, and the reason is the whole point of a catalogue:
/// a project's own images are under the project, and the ones the cell
/// published are under no project at all. A console that showed one of the two
/// answered "which images are there" with half of it — and the half that is
/// missing is the half a new tenant has, because their project is empty on the
/// day they sign up.
///
/// Merged rather than switched, and each row keeps its own name, so where an
/// image came from is readable from the row: `images/debian-13` is the cell's,
/// `projects/p1/images/…` is this project's.
const BOTH_SCOPES = ["images"];

async function listBoth(coll, narrow) {
  const seen = new Set();
  const items = [];
  let complete = true;
  let revision = null;
  for (const scope of ["global", "project"]) {
    if (scope === "project" && !session.project) continue;
    try {
      const first = await firstPage(coll, scope, narrow);
      const all = await walk(coll, scope, first, narrow);
      if (all.complete === false) complete = false;
      if (all.revision) revision = all.revision;
      for (const r of all.items) {
        const key = nameOf(r);
        if (seen.has(key)) continue;
        // The cell-wide half is asked without a parent, and the API answers it
        // with everything the caller may read — which for a tenant is the
        // catalogue plus their own, and for a **cell operator** is every
        // project's. An operator picking an image in one project was being
        // offered another tenant's, which is a choice nobody meant to make.
        //
        // So the global half keeps only what is genuinely the cell's: a name
        // with no project in it.
        if (scope === "global" && key.startsWith("projects/")) continue;
        seen.add(key);
        items.push(r);
      }
    } catch (e) {
      // One half missing is not the whole answer failing: a tenant who may not
      // read the cell's catalogue still has their own images, and an operator
      // with no project selected still has the catalogue.
      if (!(e instanceof ApiError) || (e.status !== 404 && e.status !== 403)) throw e;
    }
  }
  return { items, revision, complete };
}

async function list(coll, narrow) {
  if (BOTH_SCOPES.includes(coll.id)) return listBoth(coll, narrow);
  // The fallback runs in one direction only: for a collection declared
  // **global**, whose objects may turn out to be addressed under a project.
  //
  // The other direction is deliberately not probed. An empty answer for a
  // project collection is the ordinary state of a project on its first day —
  // not evidence of a wrong address — and probing past it asks the cell-wide
  // path, which a cell operator may read in full. The rows that come back are
  // other tenants', and believing them pointed the whole session at the wrong
  // place: a new customer's first guest was created outside their project
  // entirely. Writes never used the probed answer at all (see `writePath`);
  // this stops the reads wandering too.
  const settled = coll.scope === "project" && session.project;
  const tries = settled
    ? [coll.scope]
    : scopeFound[coll.id]
      ? [scopeFound[coll.id]]
      : [coll.scope, coll.scope === "project" ? "global" : "project"];
  let last = null, empty = null;
  for (const scope of tries) {
    if (scope === "project" && !session.project) continue;
    try {
      // Probed one page at a time: whether this is the right address is
      // answered by the first page, and walking the wrong one to the end before
      // finding out would be the whole cell fetched under a parent nobody meant.
      const first = await firstPage(coll, scope, narrow);
      // Safe to remember: the probe only runs for a collection declared
      // global, so the only thing it can discover is that the objects live
      // under a project — which is narrower than where it was looking, never
      // wider.
      if (first.items.length) { scopeFound[coll.id] = scope; return walk(coll, scope, first, narrow); }
      // An empty answer is not proof of the right address for a collection
      // whose address is genuinely in doubt — a node might be addressed under a
      // project or not. It **is** the ordinary answer for a project collection
      // in a project that has nothing in it yet, which is every project on its
      // first day. Probing past that and believing what comes back is how a new
      // customer's first guest was created outside their project entirely.
      if (!empty) empty = { scope, first };
    } catch (e) {
      last = e;
      if (!(e instanceof ApiError) || e.status !== 404) throw e;
    }
  }
  if (empty) { scopeFound[coll.id] = empty.scope; return walk(coll, empty.scope, empty.first, narrow); }
  throw last || new ApiError(404, null);
}

/// A client lists first, notes the newest revision, then watches from it —
/// so nothing between the list and the watch is lost. The header is the
/// authority; this is the fallback the contract names for when it is absent.
function newestRevision(items) {
  let best = null;
  for (const r of items) {
    const rev = revision(r);
    if (rev === null) continue;
    if (best === null || Number(rev) > Number(best)) best = rev;
  }
  return best;
}

/// Every record *about* one object: the changes that were accepted, and — for
/// a cell operator, who is the only one allowed to read them — the ones that
/// were refused.
///
/// Narrowed by the API. Fetching every operation in the cell to show six lines
/// about one guest is exactly the cost these filters exist to avoid.
async function historyOf(name) {
  const at = name.lastIndexOf("/", name.lastIndexOf("/") - 1);
  const parent = at > 0 ? name.slice(0, at) : "";
  const under = (kind) =>
    "/api/v1/" + (parent ? parent + "/" : "") + kind + "?target=" + encodeURIComponent(name) +
    "&pageSize=" + PAGE_SIZE;
  const operations = request("GET", under("operations"))
    .then((r) => r.body.items || [])
    .catch(() => []);
  // Asked for by everybody, not only by operators. A record is readable by
  // whoever may read what it is about — so the person whose click did nothing
  // is handed the sentence explaining it, which is the whole point of the
  // panel. A caller who may read none of them gets an empty list rather than a
  // refusal, so there is nothing to branch on here.
  const refusals = request(
    "GET", "/api/v1/audit?target=" + encodeURIComponent(name) + "&pageSize=" + PAGE_SIZE,
  )
    .then((r) => r.body.items || [])
    .catch(() => []);
  const [done, refused] = await Promise.all([operations, refusals]);
  return { operations: done, refusals: refused };
}

/// What a project has left, and what it could actually start with it.
const explainQuota = (project) =>
  request("GET", "/api/v1/projects/" + encodeURIComponent(project) + ":explainQuota")
    .then((r) => r.body);

/// What maintenance is planned for one node, and what it will cost.
const explainMaintenance = (id) =>
  request("GET", basePath(collection("nodes")) + "/" + encodeURIComponent(id) + ":explainMaintenance")
    .then((r) => r.body);

const get = (coll, id) => request("GET", basePath(coll) + "/" + encodeURIComponent(id)).then((r) => r.body);

const create = (coll, body, scope) =>
  request("POST", writePath(coll, scope), { body }).then((r) => r.body);

const patch = (coll, id, body, ifMatch) =>
  request("PATCH", writePath(coll) + "/" + encodeURIComponent(id), { body, ifMatch }).then((r) => r.body);

const remove = (coll, id, ifMatch) =>
  request("DELETE", writePath(coll) + "/" + encodeURIComponent(id), { ifMatch }).then((r) => r.body);

/// Ask for a way into a guest.
///
/// A POST, because it **makes** something: a session, spent once, that expires
/// in a minute. The ticket comes back in this answer and exists nowhere else —
/// what is stored is its hash — so it is held in memory and put in the stream's
/// query, never in a link and never in the address bar.
const openConsole = (coll, id) =>
  request("POST", writePath(coll) + "/" + encodeURIComponent(id) + ":console", { body: {} })
    .then((r) => r.body);

/// The **request path** of an object in the current scope — `/api/v1/…`,
/// already prefixed. Named for what it is: a caller that read this as a bare
/// resource name prepended the prefix a second time and built a URL that
/// matched no route.
const consolePath = (coll, id) =>
  basePath(coll) + "/" + encodeURIComponent(id);

const explainPlacement = (coll, id) =>
  request("GET", basePath(coll) + "/" + encodeURIComponent(id) + ":explainPlacement").then((r) => r.body);

/// Which nodes could receive this guest, and why the others could not.
///
/// Asked of the instance, because the answer is about a particular guest, and
/// asked *before* anything is created: every refusal is knowable in advance, and
/// finding one out half way through a transfer is how an operator loses a guest
/// to a preventable mismatch.
///
/// A GET, like its sibling `:explainPlacement`. It reads and creates nothing,
/// and two spellings for the same kind of verb is how a surface starts needing a
/// table of exceptions to use it.
const explainMigration = (instances, id) =>
  request("GET", basePath(instances) + "/" + encodeURIComponent(id) + ":explainMigration")
    .then((r) => r.body);

/// What the cell's processors look like, and what to do about them.
///
/// A verb on the collection rather than on a node, because which machines can
/// exchange guests is a property of the fleet: hung off one node it would read
/// as that node's answer while being the same for every one of them.
const explainCpu = () =>
  request("GET", "/api/v1/nodes:explainCpu").then((r) => r.body);

/// What the cell has, what is spoken for, and what would actually still fit.
const explainCapacity = () =>
  request("GET", "/api/v1/nodes:explainCapacity").then((r) => r.body);

// ---- watching --------------------------------------------------------------

/// Server-sent events, read off `fetch` rather than through `EventSource`.
///
/// `EventSource` cannot carry an `Authorization` header, and the alternatives
/// are putting the token in the query string — where it lands in every access
/// log between here and the API — or a cookie this API does not have. So the
/// stream is read by hand; it is thirty lines and it keeps the token in a
/// header where it belongs.
function watch(coll, fromRevision, onEvent, onState) {
  let stopped = false, controller = null, attempt = 0;

  async function once() {
    controller = new AbortController();
    const url = basePath(coll) + "?watch=true" +
      (fromRevision ? "&fromRevision=" + encodeURIComponent(fromRevision) : "");
    const headers = { Accept: "text/event-stream" };
    if (session.token) headers.Authorization = "Bearer " + session.token;
    const res = await fetch(url, { headers, signal: controller.signal });
    if (!res.ok || !res.body) throw new ApiError(res.status, null);

    onState("live");
    attempt = 0;
    const reader = res.body.getReader();
    const decoder = new TextDecoder();
    let buffer = "";
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      let cut;
      while ((cut = buffer.indexOf("\n\n")) >= 0) {
        const frame = buffer.slice(0, cut);
        buffer = buffer.slice(cut + 2);
        const data = frame
          .split("\n")
          .filter((l) => l.startsWith("data:"))
          .map((l) => l.slice(5).trim())
          .join("");
        if (!data) continue;
        let event;
        try { event = JSON.parse(data); } catch (e) { continue; }
        // Remember where the stream got to, so a reconnect resumes rather than
        // replaying or skipping.
        const rev = event.revision || (event.resource ? revision(event.resource) : null);
        if (rev) fromRevision = rev;
        onEvent(event);
      }
    }
  }

  (async function run() {
    while (!stopped) {
      try {
        await once();
        if (stopped) return;
        onState("dropped");                 // the server closed it cleanly
      } catch (e) {
        if (stopped) return;
        // A watch the API does not implement must say so once, not blink
        // "reconnecting" forever at somebody who is waiting for an update that
        // is never coming.
        if (e instanceof ApiError && (e.status === 404 || e.status === 501)) {
          onState("unsupported");
          return;
        }
        onState("dropped");
      }
      attempt++;
      await new Promise((r) => setTimeout(r, Math.min(15000, 500 * 2 ** Math.min(attempt, 5))));
    }
  })();

  return { stop() { stopped = true; try { controller && controller.abort(); } catch (e) {} } };
}

// ---- what the pickers offer ------------------------------------------------

// One fetch per collection, kept until a write or a watch event says it moved.
// A picker that refetches on every keystroke is a picker that empties itself on
// a slow link at the moment somebody is using it.
const optionCache = new Map();

async function options(collectionId) {
  if (optionCache.has(collectionId)) return optionCache.get(collectionId);
  const coll = collection(collectionId);
  if (!coll) return [];
  const p = list(coll).then((r) => r.items).catch(() => []);
  optionCache.set(collectionId, p);
  return p;
}

function forgetOptions(collectionId) {
  if (collectionId) optionCache.delete(collectionId); else optionCache.clear();
}

/// Everything this module remembers about the session that is ending.
///
/// A function rather than three lines at the call site, because the hazard is a
/// fourth cache added here later and not added there. What is forgotten:
///
///  * **the picker cache**, which holds resolved promises and is otherwise only
///    ever invalidated by a write or a watch event — so it outlived not just the
///    sign-out but the whole next session, and offered the previous tenant's
///    ports and volumes, by name, to whoever signed in next on a shared machine;
///  * **where each collection was found**, which is a fact about what the last
///    token could see, not about the API; and
///  * **the project**, which names the tenant this session was looking at. It is
///    a far weaker secret than the token, and it is cleared for the same reason
///    the token is: `signedOut` means nothing behind, and a sign-in screen
///    pre-filled with somebody else's project is not nothing.
function forgetSession() {
  optionCache.clear();
  for (const key of Object.keys(scopeFound)) delete scopeFound[key];
  session.project = "";
  session.labels = "";
  sessionStorage.removeItem(PROJECT_KEY);
}
