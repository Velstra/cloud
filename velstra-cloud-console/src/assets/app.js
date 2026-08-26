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
  // The overview, unless a link asked for something else. Landing on a board
  // was landing on one collection's answer to a question nobody had asked yet.
  if (collection(wanted)) await show(wanted);
  else await showOverview();
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

// The jump palette. The rail groups the collections behind their group names;
// this is the flat way in for an operator who knows a collection's name and not
// which group it sits under. It reuses the rail's own registry (`groups`) and
// its own activation (`show`), so the palette is a second door onto one path.
const palette = { open: false, sel: 0, items: [] };

function paletteEntries() {
  const out = [];
  for (const g of groups()) {
    for (const c of g.items) out.push({ title: c.title, group: g.name, id: c.id });
  }
  return out;
}

/// How many objects the palette offers before it stops offering them.
///
/// Twelve: enough that a name typed nearly in full is in the list, few enough
/// that the collections above it are still on screen. A palette that answered
/// "db" with four hundred rows would be a list nobody reads to the end of.
const PALETTE_OBJECTS = 12;

/// Objects, by name, out of what the sweep already read.
///
/// "Where is db-1" is the question an operator actually has, and until now the
/// only answer was to guess which board it was on and look. Nothing is fetched
/// here: the sweep lists every collection on the way in and keeps the names, so
/// this is a substring over strings already in hand rather than a search
/// endpoint that would have to exist, be paged, and be kept in step.
///
/// The cost of that choice is stated rather than hidden: these names are as old
/// as the last sweep, so an object created a moment ago in another tab is not
/// here yet. Opening one asks the API for it, and an object that has gone
/// answers as gone.
function paletteObjects(q) {
  if (!q) return [];
  const out = [];
  for (const c of collections()) {
    for (const name of (census[c.id] || {}).names || []) {
      if (!name.toLowerCase().includes(q)) continue;
      out.push({ title: shortName(name), group: c.title, id: c.id, name });
      if (out.length >= PALETTE_OBJECTS) return out;
    }
  }
  return out;
}

function renderPaletteList() {
  const list = $("palettelist");
  if (!list) return;
  clear(list);
  if (!palette.items.length) {
    list.appendChild(el("div.palempty.muted", "Nothing matches"));
    return;
  }
  palette.items.forEach((e, i) => {
    list.appendChild(el("button.palitem" + (i === palette.sel ? ".on" : ""),
      { type: "button", role: "option", "aria-selected": String(i === palette.sel),
        onclick: () => runPalette(i) },
      el("span.palt", e.title), el("span.palg", e.group)));
  });
}

function filterPalette() {
  // Substring over the collection's name and its group — enough to find a
  // collection you can name, with no ranking to learn.
  const q = $("paletteq").value.trim().toLowerCase();
  const all = paletteEntries();
  // Collections first, then the objects in them. That order is the point: a
  // typed word is far more often a place to go than a thing to open, and a
  // list that put forty guests above "Instances" would make the common case
  // scroll past the rare one.
  palette.items = q
    ? all.filter((e) => (e.title + " " + e.group).toLowerCase().includes(q))
        .concat(paletteObjects(q))
    : all;
  if (palette.sel >= palette.items.length) palette.sel = Math.max(0, palette.items.length - 1);
  renderPaletteList();
}

function movePalette(step) {
  if (!palette.items.length) return;
  palette.sel = Math.max(0, Math.min(palette.sel + step, palette.items.length - 1));
  renderPaletteList();
  const on = $("palettelist").querySelector(".palitem.on");
  if (on) on.scrollIntoView({ block: "nearest" });
}

async function runPalette(i) {
  const e = palette.items[i];
  if (!e) return;
  closePalette();
  await show(e.id);
  // An object was asked for by name, so it is opened — on its own board, not
  // as a panel over whatever happened to be on screen.
  if (!e.name) return;
  const there = view.items.find((r) => nameOf(r) === e.name);
  if (there) openSheet(view.coll, there);
}

function openPalette() {
  if (palette.open || $("app").classList.contains("hidden")) return;
  palette.open = true;
  palette.sel = 0;
  const scrim = el("div", { id: "palettescrim", onclick: closePalette });
  const box = el("div",
    { id: "palette", role: "dialog", "aria-label": "Jump to a collection or find an object" },
    el("input", { id: "paletteq", placeholder: "Jump to a collection…", autocomplete: "off",
      role: "combobox", "aria-controls": "palettelist", "aria-expanded": "true",
      oninput: filterPalette,
      onkeydown: (ev) => {
        if (ev.key === "ArrowDown") { ev.preventDefault(); movePalette(1); }
        else if (ev.key === "ArrowUp") { ev.preventDefault(); movePalette(-1); }
        else if (ev.key === "Enter") { ev.preventDefault(); runPalette(palette.sel); }
      } }),
    el("div", { id: "palettelist", role: "listbox", "aria-label": "Collections" }));
  document.body.appendChild(scrim);
  document.body.appendChild(box);
  filterPalette();
  $("paletteq").focus();
}

function closePalette() {
  palette.open = false;
  for (const id of ["palettescrim", "palette"]) { const n = $(id); if (n) n.remove(); }
}

$("jump").addEventListener("click", openPalette);

// Never trap anybody: Escape leaves whatever is on top, innermost first. And
// Cmd/Ctrl-K is the palette, from anywhere the board is up.
document.addEventListener("keydown", (e) => {
  if ((e.metaKey || e.ctrlKey) && (e.key === "k" || e.key === "K")) {
    e.preventDefault();
    palette.open ? closePalette() : openPalette();
    return;
  }
  if (e.key !== "Escape") return;
  if (palette.open) closePalette();
  else if ($("dialog")) closeDialog();
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
