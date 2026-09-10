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
