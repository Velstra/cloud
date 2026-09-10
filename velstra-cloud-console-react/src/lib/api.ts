// The same REST contract the other console reads, against the same server.
const TOKEN_KEY = "velstra-react-token";

export const token = () => localStorage.getItem(TOKEN_KEY) ?? "";

export async function signIn(username: string, password: string) {
  const r = await fetch("/api/v1/sessions", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ username, password }),
  });
  if (!r.ok) throw new Error("That was not accepted.");
  const body = await r.json();
  localStorage.setItem(TOKEN_KEY, body.token);
  return body as { subject: string; displayName: string; cellAdmin: boolean };
}

async function call(method: string, path: string, body?: unknown) {
  const r = await fetch("/api/v1" + path, {
    method,
    headers: {
      authorization: "Bearer " + token(),
      ...(body ? { "content-type": "application/json" } : {}),
    },
    body: body ? JSON.stringify(body) : undefined,
  });
  const text = await r.text();
  const parsed = text ? JSON.parse(text) : null;
  if (!r.ok) throw new Error(parsed?.message || parsed?.error || r.statusText);
  return parsed;
}

export const list = (coll: string) => call("GET", `/projects/p1/${coll}`);
export const create = (coll: string, body: unknown) => call("POST", `/projects/p1/${coll}`, body);
