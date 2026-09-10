// Components. `dom.js` builds elements; this builds the few things the console
// is pressed on, so that pressing one behaves the same everywhere.
//
// It exists because of a defect rather than a preference. The stylesheet has
// had `.btn.busy` — a spinner, and a slower one under `prefers-reduced-motion`
// — for as long as it has had buttons, and of the four presses that wait on the
// API, three did it differently and one of them forgot the spinner entirely.
// A press that waits is not something to remember at each call site; it is what
// a button does.

/// The button.
///
///   btn("Refresh", { onclick: () => show(coll.id) })
///   btn("Create", { primary: true, onclick: submit })
///   btn("Delete", { quiet: true, destroys: true, onclick: go })
///
/// If the handler returns a promise, the button serves the wait itself: it
/// disables, shows that it is working, and comes back — whether the call
/// answered or threw. A handler that returns nothing is a press that finishes
/// when it returns, and the button does not flicker on the way.
///
/// What it deliberately does not do is celebrate. A tick that flashes green for
/// a second and a half says what the object beside it already says, and this
/// console's rule is that the object carries the truth; a refusal still goes to
/// `toast`, which is where a sentence belongs.
function btn(label, opts) {
  const o = opts || {};
  const classes = ["btn"];
  if (o.primary) classes.push("primary");
  if (o.quiet) classes.push("quiet");
  if (o.destroys || o.danger) classes.push("danger");

  // `type="button"` always: a button inside a form with no type submits it,
  // which is how a "Remove this grant" turns into a save nobody asked for.
  const OPTIONS = ["primary", "quiet", "danger", "destroys", "onclick", "busy", "disabled"];
  const attrs = { type: "button" };
  for (const [k, v] of Object.entries(o)) {
    if (OPTIONS.includes(k) || v === null || v === undefined || v === false) continue;
    attrs[k] = v;
  }
  if (o.disabled) attrs.disabled = "";

  const node = el("button." + classes.join("."), attrs, label);
  node.dataset.label = label;

  if (o.onclick) {
    node.addEventListener("click", (e) => {
      const answer = o.onclick(e);
      if (!answer || typeof answer.then !== "function") return answer;
      working(node, o.busy);
      // Both arms, not `finally`: a press that fails has still finished, and a
      // button left spinning after a 403 is a console that looks hung. Written
      // this way rather than `.finally()` because that derives a second promise
      // which rejects again with nobody holding it — one refusal, two unhandled
      // rejections in the log. Every call site here catches its own and says so
      // with `toast`; what this owes them is only the button coming back.
      answer.then(() => settled(node), () => settled(node));
      return answer;
    });
  }
  return node;
}

/// Which press this button is serving. A slow first press must not un-busy the
/// second one that replaced it — the same reason a stale answer must not draw
/// itself over a newer one.
let pressRun = 0;

/// "Create" while it is being created reads as a button that did nothing.
///
/// The verb in the present tense, derived rather than written out at each call
/// site — lifted from the create form, which was the one place that did it and
/// the reason every other press looked dead for the second it took. A label
/// this does not recognise falls back to "Working…", which is true of anything.
const PRESSED_VERB = /^(Create|Save|Add|Move|Migrate|Attach|Apply|Refresh|Delete|Remove|Issue|Start|Stop|Detach|Resize|Reboot)\b/;
function presentTense(label) {
  const verb = (PRESSED_VERB.exec(String(label || "")) || ["Working"])[0];
  return verb.replace(/e?$/, "") + "ing…";
}

/// Say that this button is working.
///
/// It does not try to hold the button's width. Pinning `min-width` to what was
/// measured before the swap was tried and measured again: it stops a button
/// shrinking and does nothing about it growing, which is the direction that
/// moves anything — "Refreshing…" is wider than "Refresh". Reserving the wider
/// label up front would mean a layout pass per button at render time, and in
/// this console the presses that wait sit last in their rows, so the growth
/// pushes nothing. Left out on purpose rather than left in and untrue.
function working(node, busyLabel) {
  if (!node) return;
  node.dataset.press = String(++pressRun);
  node.disabled = true;
  node.setAttribute("aria-busy", "true");
  node.classList.add("busy");
  if (!node.dataset.label) node.dataset.label = node.textContent;
  node.textContent = busyLabel || presentTense(node.dataset.label);
}

/// …and that it has finished, whichever way it finished.
function settled(node) {
  // The bar a button sits in is often rebuilt by the very call it was serving,
  // so by now this node is usually gone. Restoring it anyway is for the one
  // path where it is not: a refusal that never got as far as a re-render.
  if (!node || !node.isConnected) return;
  node.disabled = false;
  node.removeAttribute("aria-busy");
  node.classList.remove("busy");
  if (node.dataset.label) node.textContent = node.dataset.label;
}
