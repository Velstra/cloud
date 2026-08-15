// The API, and only the API. Nothing on this page fetches anything else.

const TOKEN_KEY = "velstra-cloud-token";
const PROJECT_KEY = "velstra-cloud-project";

// The token lives in sessionStorage: it is gone when the tab closes, because a
// cloud token has no business outliving the session on a shared machine.
const session = {
  token: sessionStorage.getItem(TOKEN_KEY) || "",
  project: sessionStorage.getItem(PROJECT_KEY) || "",
};

const collections = () => SCHEMA;
const collection = (id) => SCHEMA.find((c) => c.id === id) || null;

/// A collection's scope is declared in the schema, but the contract does not
/// spell out where a node is addressed from, and a console that guesses wrong
/// shows an empty fleet rather than an error. So the first 404 on a listing
/// tries the other scope once and remembers the answer for the session.
const scopeFound = {};

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

async function list(coll) {
  const tries = scopeFound[coll.id]
    ? [scopeFound[coll.id]]
    : [coll.scope, coll.scope === "project" ? "global" : "project"];
  let last = null, empty = null;
  for (const scope of tries) {
    if (scope === "project" && !session.project) continue;
    try {
      const r = await request("GET", basePath(coll, scope));
      const items = itemsOf(r.body, coll);
      const answer = { items, revision: r.revision || newestRevision(items) };
      if (items.length) { scopeFound[coll.id] = scope; return answer; }
      // An empty answer is not proof of the right address: a collection asked
      // for under the wrong parent answers 200 with nothing in it, which reads
      // as "you have no nodes" rather than as a mistake. So the other scope is
      // tried before an empty listing is believed — and if both are empty the
      // declared one is kept, because it is the one that will start answering
      // once something exists.
      if (!empty) empty = { scope, answer };
    } catch (e) {
      last = e;
      if (!(e instanceof ApiError) || e.status !== 404) throw e;
    }
  }
  if (empty) { scopeFound[coll.id] = empty.scope; return empty.answer; }
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

const get = (coll, id) => request("GET", basePath(coll) + "/" + encodeURIComponent(id)).then((r) => r.body);

const create = (coll, body) => request("POST", basePath(coll), { body }).then((r) => r.body);

const patch = (coll, id, body, ifMatch) =>
  request("PATCH", basePath(coll) + "/" + encodeURIComponent(id), { body, ifMatch }).then((r) => r.body);

const remove = (coll, id, ifMatch) =>
  request("DELETE", basePath(coll) + "/" + encodeURIComponent(id), { ifMatch }).then((r) => r.body);

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
