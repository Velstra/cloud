// Who may do what inside a project.
//
// The one surface where a form would have been actively harmful, and the reason
// there was no box here for a long time: a project's bindings are a *set*, and
// a text field holding the whole set replaces all of it on every save. Somebody
// adding one person while a colleague adds another loses the colleague's change
// and never learns it happened.
//
// So: rows. Each grant is a row, each row is added or removed on its own, and
// the save is a compare-and-swap against the revision the sheet was drawn from
// — a change made underneath is refused with the API's own words rather than
// overwritten.

/// The rungs, in the order they climb, with what each one is for.
///
/// Read from the model rather than remembered here would be better and is not
/// possible: the console has the schema, and the schema describes *fields*.
/// These four are a contract of their own — `docs/rest-contract.md` lists them
/// — and a rung that appeared in one place and not the other would be a role
/// somebody grants and nothing honours. The test `every_role_the_model_has_is
/// _offered` in the API's suite is what keeps the two in step.
const ROLES = [
  { id: "viewer", label: "Viewer", help: "Look at everything in the project and change nothing." },
  {
    id: "operator",
    label: "Operator",
    help: "Run what is there — start, stop, resize, attach, open a console with a keyboard. "
      + "Cannot create anything or take anything away.",
  },
  { id: "editor", label: "Editor", help: "That, and create and delete. Cannot change who may." },
  { id: "admin", label: "Admin", help: "Everything, including these grants." },
];

/// The bindings of a project, as rows somebody can work with.
///
/// Normalised on the way in: the API carries a list of `{role, members}`, and
/// two entries with the same role are legal there and confusing here. They are
/// folded, so what is shown is one row per person and per role.
function grantsOf(project) {
  const out = [];
  for (const b of (spec(project).bindings || [])) {
    for (const m of (b.members || [])) out.push({ role: b.role, member: m });
  }
  out.sort((a, b) => a.member.localeCompare(b.member) || a.role.localeCompare(b.role));
  return out;
}

/// Rows back into the shape the API takes: one entry per role, members folded
/// into it, and roles nobody holds left out entirely.
function bindingsFrom(grants) {
  const by = new Map();
  for (const g of grants) {
    if (!g.member) continue;
    if (!by.has(g.role)) by.set(g.role, []);
    const members = by.get(g.role);
    if (!members.includes(g.member)) members.push(g.member);
  }
  return ROLES.filter((r) => by.has(r.id)).map((r) => ({ role: r.id, members: by.get(r.id) }));
}

/// Render the grants of `project` into `host`.
///
/// `onSaved` is handed the API's answer so the sheet can redraw from what was
/// actually stored rather than from what was typed.
function grantsInto(host, coll, project, onSaved) {
  // What is *stored*, and what is being edited, kept apart. The screen has to
  // be able to fall back to the first when a save does not land — a person who
  // removed somebody, saw them go, and finds them still there tomorrow was
  // shown something that was never true.
  let stored = project;
  let grants = grantsOf(stored);
  let note = null;

  const draw = () => {
    const rows = el("div.grants");
    for (const [i, g] of grants.entries()) {
      const pick = el("select.input");
      for (const r of ROLES) {
        const opt = el("option", { value: r.id }, r.label);
        if (r.id === g.role) opt.selected = true;
        pick.appendChild(opt);
      }
      pick.onchange = () => { grants[i].role = pick.value; };
      pick.title = (ROLES.find((r) => r.id === g.role) || {}).help || "";

      const who = el("input.input", { type: "text", value: g.member, placeholder: "ada@example.com" });
      who.oninput = () => { grants[i].member = who.value.trim(); };

      const drop = el("button.btn.quiet", { type: "button", title: "Remove this grant" }, "Remove");
      drop.onclick = () => { grants.splice(i, 1); draw(); };

      rows.appendChild(el("div.grantrow", who, pick, drop));
    }

    const add = el("button.btn.quiet", { type: "button" }, "Add someone");
    add.onclick = () => { grants.push({ role: "viewer", member: "" }); draw(); };

    const save = el("button.btn", { type: "button" }, "Save grants");
    save.onclick = async () => {
      save.disabled = true;
      try {
        // The revision of what this panel last read. A colleague's change made
        // in between is refused here rather than replaced — which is the whole
        // reason this is not one text box.
        const answer = await patch(
          coll,
          idOf(stored),
          { spec: { bindings: bindingsFrom(grants) } },
          revision(stored),
        );
        // The answer is now what is stored, and the next save has to carry
        // *its* revision. Without this the second save from one open sheet was
        // always refused as stale — the panel kept the revision it was drawn
        // with, which the first save had already moved on from.
        stored = answer;
        grants = grantsOf(stored);
        note = el("span.muted", "Saved.");
        if (onSaved) onSaved(answer);
      } catch (e) {
        // Refused. The rows go back to what is **stored**, because leaving the
        // edit on screen shows a change that did not happen — and the sentence
        // beside them is the API's own.
        try {
          stored = await get(coll, idOf(stored));
        } catch (again) {
          // Could not re-read either: keep the last known object rather than
          // inventing one, and say what went wrong with the save.
        }
        grants = grantsOf(stored);
        note = el("span.bad", String(e.message || e));
      }
      save.disabled = false;
      draw();
    };

    const empty = grants.length
      ? null
      : el("p.muted", "Nobody but a cell operator. That is what a new project is, "
        + "deliberately: whoever created it grants themselves rather than being "
        + "granted by a default nobody chose.");

    fill(host, rows, empty, el("div.grantactions", add, save, note || el("span")));
  };

  draw();
}
