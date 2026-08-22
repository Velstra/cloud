// Getting in, and staying oriented once in.

/// End the session, and leave nothing of it behind.
///
/// This used to stop the stream and drop the token, which is what makes the
/// *next* request fail — and left everything the session had already read
/// sitting in memory and on screen: the rail's counts, the board's rows, the
/// picker cache. On a shared machine that is the previous tenant's inventory,
/// shown by name to whoever signs in next. Neither cache had anything to clear
/// it, because both were only ever invalidated by a write or a watch event, and
/// a signed-out console has neither.
function signedOut(why) {
  session.token = "";
  sessionStorage.removeItem(TOKEN_KEY);
  if (view.watcher) { view.watcher.stop(); view.watcher = null; }
  stopRecheck();
  closeSheet(); closeDialog();
  forgetSession();
  forgetBoard();
  session.who = null;
  const chip = $("whoami");
  chip.textContent = "";
  chip.classList.add("hidden");
  $("app").classList.add("hidden");
  $("signin").classList.remove("hidden");
  const box = $("loginerr");
  box.textContent = why || "";
  box.classList.toggle("hidden", !why);
}

/// Which projects this token can see. It doubles as the check that the token is
/// good at all: a console that lets somebody in and then fails every panel has
/// told them nothing about what went wrong.
async function loadProjects() {
  const coll = collection("projects");
  const r = await list(coll);
  const select = $("project");
  clear(select);
  const ids = r.items.map(idOf).sort();
  for (const id of ids) select.appendChild(el("option", { value: id }, id));
  if (!ids.length) {
    select.appendChild(el("option", { value: "" }, "no projects"));
    session.project = "";
  } else if (!ids.includes(session.project)) {
    session.project = ids[0];
  }
  select.value = session.project;
  sessionStorage.setItem(PROJECT_KEY, session.project);
  return ids;
}

async function enter() {
  $("signin").classList.add("hidden");
  $("app").classList.remove("hidden");
  await loadIdentity();
  await loadProjects();
  const wanted = location.hash.replace(/^#/, "");
  await show(collection(wanted) ? wanted : "instances");
  sweep();
}

/// Who is signed in, and what the API says they may do at cell scope.
///
/// Asked of the API rather than decided here. A console that worked out its own
/// answer would draw buttons the API then refuses, and the operator would learn
/// what they may do by trying things.
async function loadIdentity() {
  try {
    // `request` answers with the envelope — `{ body, revision }` — and not the
    // document. Taking the whole thing here left every field undefined and the
    // header blank, which reads exactly like "not signed in".
    session.who = (await request("GET", "/api/v1/sessions/current")).body;
  } catch {
    // A static token has no session record behind it. That is a legitimate way
    // to be signed in, so it is not an error — it just means there is nothing
    // to name and nothing to sign out of.
    session.who = { subject: "", cellAdmin: false, session: false };
  }
  const chip = $("whoami");
  const name = session.who.displayName || session.who.subject;
  chip.textContent = name ? (session.who.cellAdmin ? name + " · operator" : name) : "";
  chip.classList.toggle("hidden", !name);
}

/// Exchange a username and password for a session token.
async function signInWithPassword(username, password) {
  // Deliberately not through `request`: this is the one call that must go out
  // *without* an Authorization header, and it is the call that produces one.
  const res = await fetch("/api/v1/sessions", {
    method: "POST",
    headers: { "Content-Type": "application/json", Accept: "application/json" },
    body: JSON.stringify({ username, password }),
  });
  const body = await res.json().catch(() => ({}));
  if (!res.ok) throw new ApiError(res.status, body);
  session.token = body.token;
  sessionStorage.setItem(TOKEN_KEY, body.token);
  return body;
}

/// Sign in with a static token — a service account, or an automation.
async function signInWithToken(token) {
  session.token = token;
  const r = await request("GET", "/api/v1/projects");   // throws on a bad token
  sessionStorage.setItem(TOKEN_KEY, token);
  return r;
}

$("tokenform").addEventListener("submit", async (e) => {
  e.preventDefault();
  const btn = $("signinbtn");
  const box = $("loginerr");
  box.classList.add("hidden");
  btn.setAttribute("disabled", "");
  const token = $("token").value.trim();
  const username = $("username").value.trim();
  try {
    if (token) {
      await signInWithToken(token);
    } else {
      await signInWithPassword(username, $("password").value);
    }
    // Cleared whatever happened next: the password must not sit in a DOM node
    // for the life of the tab, where a screen-share or a stray extension can
    // read it back.
    $("password").value = "";
    $("token").value = "";
    await enter();
  } catch (err) {
    session.token = "";
    // The API refuses every bad sign-in in the same words on purpose, so the
    // console repeats them rather than guessing at a friendlier cause — a
    // console that said "no such user" would undo that.
    box.textContent = err.status === 401 || err.status === 403
      ? (token ? "That token was refused." : err.message || "Those credentials were not accepted.")
      : err.message;
    box.classList.remove("hidden");
  } finally {
    btn.removeAttribute("disabled");
  }
});

$("signout").addEventListener("click", () => {
  // The screen clears first, and the cell is told after.
  //
  // The other order is tempting — end the session, *then* forget it — and it is
  // wrong for the moment this button exists for: somebody standing up from a
  // shared machine. Waiting on a round trip there means the console is still
  // showing a tenant's inventory while a slow network decides, which is the one
  // thing sign-out must never do.
  //
  // Nothing is lost by the order. The token is captured before it is dropped and
  // the request goes out with it, so the session really does end; and if that
  // request never lands, what is left is a token nobody holds, which expires on
  // its own. Refusing to sign out because a server did not answer would be the
  // worst of both.
  const token = session.token;
  const hadSession = !!(session.who && session.who.session);
  signedOut("");
  if (!hadSession || !token) return;
  // Not through `request`: it reads `session.token`, which is deliberately
  // already gone by this point.
  fetch("/api/v1/sessions/current", {
    method: "DELETE",
    headers: { Authorization: "Bearer " + token },
  }).catch(() => {});
});

$("project").addEventListener("change", async () => {
  session.project = $("project").value;
  sessionStorage.setItem(PROJECT_KEY, session.project);
  forgetOptions();
  closeSheet();
  await show(view.coll ? view.coll.id : "instances");
  sweep();
});

// Dark is the system's identity and the default; an operator working in
// daylight beside other documents can say otherwise, and the choice sticks.
const THEME_KEY = "velstra-cloud-theme";
function applyTheme(theme) {
  if (theme) document.documentElement.setAttribute("data-theme", theme);
  else document.documentElement.removeAttribute("data-theme");
  $("theme").textContent = theme === "light" ? "Dark" : "Light";
}
$("theme").addEventListener("click", () => {
  const now = document.documentElement.getAttribute("data-theme") === "light" ? "dark" : "light";
  localStorage.setItem(THEME_KEY, now);
  applyTheme(now);
});
applyTheme(localStorage.getItem(THEME_KEY));

// Never trap anybody: Escape leaves whatever is on top, innermost first.
document.addEventListener("keydown", (e) => {
  if (e.key !== "Escape") return;
  if ($("dialog")) closeDialog();
  else if (sheet.open) closeSheet();
});

window.addEventListener("hashchange", () => {
  const wanted = location.hash.replace(/^#/, "");
  if (collection(wanted) && (!view.coll || view.coll.id !== wanted)) show(wanted);
});

// A token still in this tab's storage means the operator is already signed in;
// a token that has since been revoked means they are not, and finding that out
// at the sign-in screen is better than finding it out one empty panel at a time.
if (session.token) {
  signIn(session.token).then(enter).catch(() => signedOut(""));
}
