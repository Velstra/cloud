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
  await loadProjects();
  const wanted = location.hash.replace(/^#/, "");
  await show(collection(wanted) ? wanted : "instances");
  sweep();
}

async function signIn(token) {
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
  try {
    await signIn($("token").value.trim());
    $("token").value = "";
    await enter();
  } catch (err) {
    session.token = "";
    box.textContent = err.status === 401 || err.status === 403
      ? "That token was refused."
      : err.message;
    box.classList.remove("hidden");
  } finally {
    btn.removeAttribute("disabled");
  }
});

$("signout").addEventListener("click", () => signedOut(""));

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
