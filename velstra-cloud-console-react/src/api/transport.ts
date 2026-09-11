// The one place a request is made. Every generated function comes through
// here, which is what lets the coverage test count calls by operation id.

const TOKEN_KEY = "velstra-react-token";
export const token = () => localStorage.getItem(TOKEN_KEY) ?? "";
export const setToken = (t: string) => localStorage.setItem(TOKEN_KEY, t);
export const clearToken = () => localStorage.removeItem(TOKEN_KEY);

/** Called when the API says the session is over. The shell registers what to
 *  do; the transport must not import the store (the store imports this). */
let onSessionEnd: (() => void) | null = null;
export const whenSessionEnds = (fn: () => void) => { onSessionEnd = fn; };
const sessionEnded = () => { onSessionEnd?.(); };

export class ApiError extends Error {
  status: number;
  code: string;
  field?: string;
  constructor(status: number, code: string, message: string, field?: string) {
    super(message);
    this.status = status;
    this.code = code;
    this.field = field;
  }
}

export const called = new Set<string>();

export async function call(
  id: string, method: string, path: string,
  query?: Record<string, string | number | boolean | undefined>, body?: unknown,
  headers?: Record<string, string>,
) {
  called.add(id);
  const qs = query
    ? "?" + Object.entries(query).filter(([, v]) => v !== undefined && v !== "")
        .map(([k, v]) => `${encodeURIComponent(k)}=${encodeURIComponent(String(v))}`).join("&")
    : "";
  const r = await fetch(path + (qs === "?" ? "" : qs), {
    method,
    headers: {
      ...(token() ? { authorization: "Bearer " + token() } : {}),
      ...(body !== undefined ? { "content-type": "application/json" } : {}),
      ...(headers ?? {}),
    },
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });
  const text = await r.text();
  let parsed: any = null;
  try { parsed = text ? JSON.parse(text) : null; } catch { parsed = { message: text }; }
  if (!r.ok) {
    // An expired or revoked session: drop the token AND the identity, so the
    // shell falls back to the sign-in form instead of leaving a signed-in
    // frame whose every request now fails.
    if (r.status === 401) { clearToken(); sessionEnded(); }
    // The API's refusal is `{ error: { code, message, field } }`; some paths
    // say it flat. Either way the sentence is what the person is shown.
    const e = parsed?.error && typeof parsed.error === "object" ? parsed.error : parsed ?? {};
    const message = e.message ?? (typeof parsed?.error === "string" ? parsed.error : null) ?? r.statusText;
    throw new ApiError(r.status, e.code ?? "", message, e.field);
  }
  return parsed;
}

// ---- watching --------------------------------------------------------------

/** What a stream is doing, for the one line on screen that says so. */
export type WatchState = "connecting" | "live" | "dropped" | "unsupported";

export type WatchEvent =
  | { type: "PUT"; resource: any }
  | { type: "DELETE"; name: string; revision?: string };

/**
 * Server-sent events, read off `fetch` rather than through `EventSource`.
 *
 * `EventSource` cannot carry an `Authorization` header, and the alternatives
 * are putting the token in the query string — where it lands in every access
 * log between here and the API — or a cookie this API does not have. So the
 * stream is read by hand; it is thirty lines and it keeps the token in a
 * header where it belongs.
 *
 * The same shape the other console has used all along. This one polled
 * instead, and only while some row's verdict said it was busy — which never
 * fires for a collection whose `condition` is empty, so the subnets and
 * security-groups boards were frozen from the moment they loaded.
 */
export function watch(
  path: string,
  fromRevision: string | undefined,
  onEvent: (e: WatchEvent) => void,
  onState: (s: WatchState) => void,
): { stop: () => void } {
  let stopped = false;
  let controller: AbortController | null = null;
  let attempt = 0;

  async function once() {
    controller = new AbortController();
    const url = path + "?watch=true" + (fromRevision ? "&fromRevision=" + encodeURIComponent(fromRevision) : "");
    const res = await fetch(url, {
      headers: {
        accept: "text/event-stream",
        ...(token() ? { authorization: "Bearer " + token() } : {}),
      },
      signal: controller.signal,
    });
    if (!res.ok || !res.body) throw new ApiError(res.status, "", res.statusText);

    onState("live");
    attempt = 0;
    const reader = res.body.getReader();
    const decoder = new TextDecoder();
    let buffer = "";
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      let cut: number;
      while ((cut = buffer.indexOf("\n\n")) >= 0) {
        const frame = buffer.slice(0, cut);
        buffer = buffer.slice(cut + 2);
        const data = frame.split("\n").filter((l) => l.startsWith("data:"))
          .map((l) => l.slice(5).trim()).join("");
        if (!data) continue;
        let event: WatchEvent;
        try { event = JSON.parse(data); } catch { continue; }
        // Remember where the stream got to, so a reconnect resumes rather than
        // replaying or skipping.
        const rev = (event as any).revision ?? (event as any).resource?.meta?.revision;
        if (rev) fromRevision = String(rev);
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
        // A watch the API does not serve must say so once, not blink
        // "reconnecting" for ever at somebody waiting for an update that is
        // never coming.
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

  return { stop() { stopped = true; try { controller?.abort(); } catch { /* already gone */ } } };
}
