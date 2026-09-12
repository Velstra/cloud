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

const isRung = (role) => ROLES.some((r) => r.id === role);

/// The bindings of a project, as rows somebody can work with.
///
/// Normalised on the way in: the API carries a list of `{role, members}`, and
/// two entries with the same role are legal there and confusing here. They are
/// folded, so what is shown is one row per person and per role.
///
/// A role this panel has no rung for — `roles/db-operator`, written by hand or
/// by something that is not this console — is kept and marked. Mapped onto the
/// four it read as "Viewer", because that is the option a `<select>` shows when
/// none of them match, and the row then saved back as whatever it was showing.
/// A grant somebody made deliberately is not this panel's to reinterpret.
function grantsOf(project) {
  const out = [];
  for (const b of (spec(project).bindings || [])) {
    const fixed = !isRung(b.role);
    for (const m of (b.members || [])) out.push({ role: b.role, member: m, fixed });
  }
  out.sort((a, b) => a.member.localeCompare(b.member) || a.role.localeCompare(b.role));
  return out;
}

/// Rows back into the shape the API takes: one entry per role, members folded
/// into it, and roles nobody holds left out entirely.
///
/// The four rungs first and in their own order, then every other role that came
/// off the wire. Filtering to the four was not a display decision: a binding on
/// a role this console has no name for never reached the result at all, so the
/// first save from this panel quietly took it away.
function bindingsFrom(grants) {
  const by = new Map();
  for (const g of grants) {
    if (!g.member) continue;
    if (!by.has(g.role)) by.set(g.role, []);
    const members = by.get(g.role);
    if (!members.includes(g.member)) members.push(g.member);
  }
  const rungs = ROLES.map((r) => r.id).filter((id) => by.has(id));
  const rest = [...by.keys()].filter((role) => !isRung(role));
  return rungs.concat(rest).map((role) => ({ role, members: by.get(role) }));
}

/// What this account is in `project`, in the API's own word, or "" where the
/// answer is not known here.
function rungIn(project) {
  const who = session.who || {};
  if (!who.projects) return "";
  return who.projects[idOf(project)] || "";
}

/// May this account change these grants, or only read them?
///
/// Asked of whoami rather than of the API by trying: a PATCH from anybody below
/// project admin comes back "no permission on this resource, or it does not
/// exist", and until this was here an operator or a viewer got the selects, the
/// "Add someone" and the "Save grants" — and learned all of that from a refusal,
/// after typing somebody's account id into a box that was never going to keep
/// it.
///
/// The unknown cases stay permissive, the same way `roleHere` does: a token with
/// no `projects` map, or a role the console has no rung for, is offered the
/// controls and left to the API's answer, so nothing that used to work goes
/// missing. What is not permissive is a rung this console understands and knows
/// to be too low.
function mayGrant(project) {
  const who = session.who || {};
  if (who.cellAdmin) return true;
  if (!who.projects) return true;
  const rung = rungIn(project);
  if (!rung) return false;
  return rung === "admin" || !isRung(rung);
}

/// Does this edit take the signed-in account's own admin away?
///
/// A project admin removing their own binding locks themselves out of this
/// panel, and the only way back is a cell operator or another admin here. It is
/// a legitimate thing to do — handing a project over ends with exactly this —
/// so it is asked, not refused.
function losesOwnAdmin(stored, grants) {
  const who = session.who || {};
  // A cell operator holds this panel whatever a project's bindings say, so
  // there is nothing to ask them — and the sentence would be untrue of them.
  if (who.cellAdmin) return false;
  const me = who.subject;
  if (!me) return false;
  const held = grantsOf(stored).some((g) => g.member === me && g.role === "admin");
  if (!held) return false;
  return !grants.some((g) => g.member === me && g.role === "admin");
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
  // Whether the "this removes your own access" question is on screen, waiting
  // to be answered.
  let asking = false;

  // The accounts this cell has, where this session may read them. Filled in
  // place rather than by redrawing, because the answer arrives while somebody
  // may already be typing into the row it belongs to.
  const known = el("datalist", { id: "grantaccounts" });
  let accountIds = null;
  // One per row on screen, so the rows already drawn can be judged again when
  // the list turns up — the grant most worth flagging is the address somebody
  // typed last week, and it is on screen before anything is fetched.
  const checks = [];

  const draw = () => {
    const editable = mayGrant(stored);
    const rows = el("div.grants");
    checks.length = 0;
    for (const [i, g] of grants.entries()) {
      // A role with no rung, and a panel that may only read: both are rows that
      // say what is stored and change nothing. Drawn the same way, because they
      // mean the same thing to whoever is looking at them.
      if (!editable || g.fixed) {
        rows.appendChild(el("div.grantrow",
          el("span.mono", g.member),
          el("span", (ROLES.find((r) => r.id === g.role) || {}).label || g.role),
          el("span.muted", g.fixed ? "left as it is" : "")));
        continue;
      }

      const pick = el("select.input");
      for (const r of ROLES) {
        const opt = el("option", { value: r.id }, r.label);
        if (r.id === g.role) opt.selected = true;
        pick.appendChild(opt);
      }
      pick.onchange = () => { grants[i].role = pick.value; };
      pick.title = (ROLES.find((r) => r.id === g.role) || {}).help || "";

      // An account **id**, which is what a binding is stored against. The box
      // asked for `ada@example.com` for a long time, and an address typed into
      // it was accepted, stored and honoured by nothing — the administrator
      // believed they had given somebody access, and nobody had it.
      const who = el("input.input", { type: "text", value: g.member,
        placeholder: "account id", list: "grantaccounts",
        autocomplete: "off", spellcheck: "false", autocapitalize: "none",
        "aria-label": "Account id" });
      const flag = el("div.warn.hidden");
      const check = () => {
        // Only where the console genuinely holds the list. Silence about an id
        // nobody fetched would be a promise this cannot keep, so a project
        // admin — who may not read `users` — gets no flag rather than a wrong
        // one.
        const id = grants[i].member;
        const unknown = accountIds && accountIds.length && id && !accountIds.includes(id);
        flag.textContent = unknown
          ? "No account called " + id + " in this cell. The grant is stored against "
            + "whatever is typed, so this one would give nobody anything."
          : "";
        flag.classList.toggle("hidden", !unknown);
      };
      who.oninput = () => { grants[i].member = who.value.trim(); check(); };

      const drop = btn("Remove", { quiet: true, title: "Remove this grant" });
      drop.onclick = () => { grants.splice(i, 1); draw(); };

      rows.appendChild(el("div.grantrow", who, pick, drop));
      rows.appendChild(flag);
      checks.push(check);
      check();
    }

    const empty = grants.length
      ? null
      : el("p.muted", "Nobody but a cell operator. That is what a new project is, "
        + "deliberately: whoever created it grants themselves rather than being "
        + "granted by a default nobody chose.");

    if (!editable) {
      const rung = rungIn(stored);
      fill(host, rows, empty,
        el("p.muted", "Only a project admin may change these, and this account is "
          + (rung ? rung + " in " : "not a member of ") + idOf(stored)
          + " — ask an admin here, or a cell operator."));
      return;
    }

    const help = el("p.muted",
      "An account id — dba, qa-user — not an email address. A grant is stored against "
      + "whatever is written here, and honoured for nobody if that is not an account.");

    const add = btn("Add someone", { quiet: true });
    add.onclick = () => { grants.push({ role: "viewer", member: "" }); draw(); };

    const save = btn("Save grants");
    save.onclick = () => {
      if (losesOwnAdmin(stored, grants)) { asking = true; draw(); return; }
      commit(save);
    };

    // Asked again of the rows as they stand. The selects behind the question
    // stay live, so an admin row put back while it is on screen has answered
    // it — and a warning left standing over an edit it is no longer true of is
    // one nobody reads the next time.
    if (asking && !losesOwnAdmin(stored, grants)) asking = false;

    if (!asking) {
      fill(host, rows, empty, help, known,
        el("div.grantactions", add, save, note || el("span")));
      return;
    }

    // Asked once, in place, the way a delete is — and for the same reason: it
    // is a legitimate thing to do and it cannot be undone from here.
    const yes = btn("Save anyway", { quiet: true, id: "confirmgrants" });
    yes.onclick = () => { asking = false; commit(yes); };
    const no = btn("Keep my access");
    no.onclick = () => { asking = false; draw(); };
    fill(host, rows, empty, help, known,
      el("p.warn", "This takes your own admin away in " + idOf(stored)
        + ". Once it is saved you cannot change these grants again — a cell operator, "
        + "or whoever is admin here afterwards, would have to give it back."),
      el("div.grantactions", yes, no));
  };

  const commit = async (pressed) => {
    working(pressed);
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
    settled(pressed);
    draw();
  };

  draw();

  // The cell's accounts, for the box above. Only a cell operator may read
  // `users` — it is the cell's collection, not the project's — so a project
  // admin is not made to ask and be refused; their box stays the box it was.
  if (session.who && session.who.cellAdmin && mayGrant(stored)) {
    options("users").then((items) => {
      accountIds = items.map(idOf);
      fill(known, accountIds.map((id) => el("option", { value: id })));
      for (const again of checks) again();
    }).catch(() => {});
  }
}
