// Asking for something, with controls that can only produce a legal answer.
//
// Wherever a value is constrained the form chooses: an image is picked from the
// images that exist, a size is a stepper carrying its unit, a boolean is a
// switch. Free text is what is left over, and every one of those is checked as
// it is typed rather than on submit — a form that accepts six fields and then
// rejects the second is a form that wasted somebody's time on purpose.

// ---- the checks ------------------------------------------------------------

const v4 = (s) => /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/.test(s) &&
  s.split(".").every((o) => Number(o) <= 255);
const v6 = (s) => /^[0-9a-fA-F:]+$/.test(s) && s.includes(":") && !/:::/.test(s);

const CHECKS = {
  none: () => "",
  id: (s) => /^[a-z0-9][a-z0-9.-]*$/.test(s) ? ""
    : "lowercase letters, digits, '-' and '.' only — an id that has to be quoted is one something downstream will mis-split",
  address: (s) => v4(s) || v6(s) ? "" : "not an address",
  cidr: (s) => {
    const [addr, len, ...rest] = s.split("/");
    if (rest.length || len === undefined || len === "") return "expected address/prefix, like 10.0.0.0/24";
    const max = v4(addr) ? 32 : v6(addr) ? 128 : -1;
    if (max < 0) return "not an address";
    const n = Number(len);
    return Number.isInteger(n) && n >= 0 && n <= max ? "" : "the prefix must be 0–" + max;
  },
  mac: (s) => /^([0-9a-fA-F]{2}:){5}[0-9a-fA-F]{2}$/.test(s) ? "" : "six hex pairs, like 02:1a:4b:00:11:22",
  digest: (s) => /^sha256:[0-9a-f]{64}$/.test(s) ? "" : "sha256: followed by 64 hex characters",
  url: (s) => /^[a-z][a-z0-9+.-]*:\/\/.+/i.test(s) ? "" : "expected a URL with a scheme",
  name: (s) => s.split("/").length % 2 === 0 && s.split("/").every(Boolean)
    ? "" : "a resource name is collection/id pairs, like projects/p1/images/x",
};

const check = (kind, value) => (value === "" ? "" : (CHECKS[kind] || CHECKS.none)(String(value)));

/// Checks that need two fields at once. Kept apart from the per-field ones
/// because they can only run when both have been answered, and complaining
/// about a gateway before the range exists is nagging, not validating.
function crossCheck(coll, values) {
  const bad = {};
  if (coll.id === "subnets" && values.cidr && values.gateway) {
    const inside = v4Inside(values.gateway, values.cidr);
    if (inside === false) bad.gateway = "outside " + values.cidr;
  }
  return bad;
}

function v4Inside(addr, cidr) {
  const [net, len] = String(cidr).split("/");
  if (!v4(addr) || !v4(net)) return null;         // v6 is not checked rather than checked wrongly
  const n = (s) => s.split(".").reduce((a, o) => a * 256 + Number(o), 0);
  const bits = Number(len);
  if (!Number.isInteger(bits) || bits < 0 || bits > 32) return null;
  const mask = bits === 0 ? 0 : (0xffffffff << (32 - bits)) >>> 0;
  return (n(addr) & mask) >>> 0 === (n(net) & mask) >>> 0;
}

/// A locally administered address, so the console can offer one rather than ask
/// somebody to invent it. The second nibble is 2: unicast, locally assigned.
function generateMac() {
  const b = crypto.getRandomValues(new Uint8Array(6));
  b[0] = (b[0] & 0xfe) | 0x02;
  return [...b].map((x) => x.toString(16).padStart(2, "0")).join(":");
}

// ---- one field's control ---------------------------------------------------

/// Choosing one thing decides another. There is exactly one of these, and it is
/// a real fact about the model rather than a convenience: the node that must
/// open a volume is the node the guest is on, and asking for it twice is asking
/// for two answers that can disagree.
const DERIVE = {
  attachments: {
    instance: {
      values: (obj) => ({ node: at(status(obj), "node") || at(spec(obj), "node") || "" }),
      // An instance the scheduler has not placed has no node to take, and an
      // attachment without one cannot exist — the node is what has to open the
      // volume. Saying so where the choice was made beats a required field the
      // operator is left staring at with nothing legal to put in it.
      missing: (obj) => shortName(nameOf(obj)) +
        " has not been placed on a node yet, so there is nothing to attach it to.",
    },
  },
};

function fieldControl(form, f) {
  const box = el("div.field" + (f.kind === "lines" || f.kind === "refList" || f.kind === "textList" || f.kind === "ruleList" ? ".wide" : ""));
  const id = "f-" + f.key.replace(/\./g, "-");
  form.boxes[f.key] = box;
  box.appendChild(el("label", { for: id }, f.label, f.required ? el("span.req", " ·") : null));
  const err = el("div.err.hidden");
  const setErr = (m) => { err.textContent = m || ""; err.classList.toggle("hidden", !m); };
  form.errs[f.key] = setErr;

  const commit = (value, control) => {
    form.values[f.key] = value;
    const m = check(f.check || "none", value);
    setErr(m);
    if (control) control.classList.toggle("bad", !!m);
    form.revalidate(f.key);
  };

  const current = form.values[f.key];

  switch (f.kind) {
    case "lines": {
      const t = el("textarea", { id, placeholder: f.placeholder || "" });
      t.value = current ?? "";
      t.addEventListener("input", () => commit(t.value, t));
      box.appendChild(t);
      break;
    }
    case "number": {
      const input = el("input", { id, type: "number", min: String(f.min), max: String(f.max), step: String(f.step) });
      input.value = current ?? "";
      const also = el("div.also");
      const reread = () => {
        const n = Number(input.value);
        also.textContent = !input.value ? "" :
          f.scale === "mib" ? mibAlso(n) : f.scale === "bytes" ? bytes(n) : "";
      };
      const nudge = (by) => {
        const n = Number(input.value || f.min || 0) + by;
        input.value = String(Math.min(f.max, Math.max(f.min, n)));
        commit(Number(input.value), input); reread();
      };
      input.addEventListener("input", () => {
        const n = Number(input.value);
        commit(input.value === "" ? "" : n, input);
        setErr(input.value !== "" && (n < f.min || n > f.max)
          ? "between " + f.min.toLocaleString() + " and " + f.max.toLocaleString() : "");
        reread();
      });
      box.appendChild(el("div.stepper",
        el("button", { type: "button", tabindex: "-1", "aria-label": "less", onclick: () => nudge(-f.step) }, "−"),
        input,
        el("button", { type: "button", tabindex: "-1", "aria-label": "more", onclick: () => nudge(f.step) }, "+"),
        f.unit ? el("span.unit", f.unit) : null));
      box.appendChild(also);
      reread();
      break;
    }
    case "switch": {
      const b = el("button.switch", { id, type: "button", role: "switch",
        "aria-checked": current ? "true" : "false" });
      b.addEventListener("click", () => {
        const now = b.getAttribute("aria-checked") !== "true";
        b.setAttribute("aria-checked", now ? "true" : "false");
        commit(now, b);
      });
      box.appendChild(b);
      break;
    }
    case "choice": {
      const opts = f.options || [];
      if (opts.length <= 3) {
        const seg = el("div.segmented", { id });
        for (const o of opts) {
          const b = el("button", { type: "button", "data-value": o.value,
            "aria-pressed": String(current === o.value) }, o.label);
          b.addEventListener("click", () => {
            for (const other of seg.children) other.setAttribute("aria-pressed", "false");
            b.setAttribute("aria-pressed", "true");
            commit(o.value, seg);
          });
          seg.appendChild(b);
        }
        box.appendChild(seg);
      } else {
        const s = el("select", { id });
        for (const o of opts) s.appendChild(el("option", { value: o.value, selected: current === o.value ? "" : null }, o.label));
        s.addEventListener("change", () => commit(s.value, s));
        box.appendChild(s);
      }
      break;
    }
    case "ref": {
      const s = el("select", { id });
      s.appendChild(el("option", { value: "" }, f.required ? "Choose…" : "— none —"));
      s.addEventListener("change", () => {
        commit(s.value, s);
        const derive = (DERIVE[form.coll.id] || {})[f.key];
        const chosen = derive && (form.refs[f.collection] || []).find((o) => nameOf(o) === s.value);
        if (chosen) {
          for (const [key, value] of Object.entries(derive.values(chosen))) {
            form.setValue(key, value);
            if (!form.errs[key]) continue;
            form.errs[key](value ? "" : derive.missing(chosen));
          }
        }
        // A picker filtered by this one now offers a different set, and a
        // choice made before the filter changed may no longer be in it.
        form.refilter(f.key);
      });
      box.appendChild(s);
      form.pickers.push({ field: f, select: s });
      break;
    }
    case "refList": {
      const host = el("div.rowlist", { id });
      form.pickers.push({ field: f, list: host, render: () => renderRefList(form, f, host) });
      box.appendChild(host);
      break;
    }
    case "textList": {
      const host = el("div.rowlist", { id });
      renderTextList(form, f, host, setErr);
      box.appendChild(host);
      break;
    }
    case "ruleList": {
      const host = el("div.rowlist", { id });
      // Goes through the picker machinery so the groups a remote can name are
      // fetched the same way every other reference is, rather than this control
      // inventing its own loader.
      form.pickers.push({
        field: { key: f.key, collection: f.remoteCollection, spelling: "name" },
        list: host,
        render: () => renderRuleList(form, f, host, setErr),
      });
      renderRuleList(form, f, host, setErr);
      box.appendChild(host);
      break;
    }
    default: {
      const input = el("input", { id, type: "text", placeholder: f.placeholder || "", spellcheck: "false" });
      input.value = current ?? "";
      input.addEventListener("input", () => commit(input.value, input));
      if (f.check === "mac") {
        box.appendChild(el("div", { style: "display:flex;gap:var(--space-2)" }, input,
          el("button.btn", { type: "button", onclick: () => { input.value = generateMac(); commit(input.value, input); } },
            "Generate")));
      } else {
        box.appendChild(input);
      }
      form.inputs[f.key] = input;
    }
  }

  if (f.help) box.appendChild(el("div.hint", f.help));
  box.appendChild(err);
  // Derived: computed by the platform, shown but never asked. Locked: decided
  // before this dialog opened — the guest a migration moves is the object it was
  // started from, and letting it be changed here would leave every answer the
  // platform already gave about the destination attached to a different guest.
  if (f.derived || (form.locked || []).includes(f.key)) {
    for (const c of box.querySelectorAll("input, select, textarea, button.switch")) c.setAttribute("disabled", "");
  }
  return box;
}

function renderTextList(form, f, host, setErr) {
  const values = Array.isArray(form.values[f.key]) ? form.values[f.key] : [];
  clear(host);
  const commit = () => {
    form.values[f.key] = values.filter((x) => x !== "");
    const bad = values.map((x) => check(f.check || "none", x)).find(Boolean);
    setErr(bad || "");
  };
  values.forEach((value, i) => {
    const input = el("input", { type: "text", value, placeholder: f.placeholder || "", spellcheck: "false" });
    input.addEventListener("input", () => { values[i] = input.value; commit(); input.classList.toggle("bad", !!check(f.check || "none", input.value)); });
    host.appendChild(el("div.row", input,
      el("button.btn", { type: "button", "aria-label": "remove", onclick: () => { values.splice(i, 1); renderTextList(form, f, host, setErr); commit(); } }, "−")));
  });
  host.appendChild(el("button.btn", { type: "button", onclick: () => { values.push(""); renderTextList(form, f, host, setErr); } },
    "Add" + (values.length ? " another" : "")));
}

// One rule is direction, protocol, an optional port range and a remote. They
// are rendered together because they only mean anything together: a port range
// on a protocol with no ports is refused by the API, and a remote naming a
// group that is spelled by hand is a rule that silently allows nothing.
const RULE_DIRECTIONS = ["ingress", "egress"];
const RULE_PROTOCOLS = ["any", "tcp", "udp", "icmp"];

function blankRule() {
  return { direction: "ingress", protocol: "tcp", ports: { from: 443, to: 443 }, remote: { cidr: "0.0.0.0/0" } };
}

function ruleRemoteKind(rule) {
  return rule && rule.remote && Object.prototype.hasOwnProperty.call(rule.remote, "group") ? "group" : "cidr";
}

function renderRuleList(form, f, host, setErr) {
  // The array lives in the form, not in this closure: the control redraws
  // itself after every change, and a rule pushed into a local copy would be
  // gone by the time the redraw read the values back.
  if (!Array.isArray(form.values[f.key])) form.values[f.key] = [];
  const rules = form.values[f.key];
  // The picker fills this in a moment later; the control is drawn once before
  // that, so it must not assume the fetch has happened.
  const groups = (form.refs || {})[f.remoteCollection] || [];
  clear(host);

  const commit = () => {
    form.values[f.key] = rules;
    // Only what the API would refuse. A rule that is merely broad is the
    // operator's business.
    let bad = "";
    for (const r of rules) {
      const kind = ruleRemoteKind(r);
      if (kind === "cidr" && check("cidr", r.remote.cidr || "")) bad = "a remote prefix is address/length";
      if (kind === "group" && !r.remote.group) bad = "choose a group for the remote";
      if (r.ports && r.ports.from > r.ports.to) bad = "a port range runs from the lower number to the higher one";
    }
    setErr(bad);
  };

  const redraw = () => { renderRuleList(form, f, host, setErr); commit(); };

  rules.forEach((rule, i) => {
    const direction = el("select");
    for (const d of RULE_DIRECTIONS) direction.appendChild(el("option", { value: d, selected: rule.direction === d ? "" : null }, d));
    direction.addEventListener("change", () => { rule.direction = direction.value; commit(); });

    const protocol = el("select");
    for (const p of RULE_PROTOCOLS) protocol.appendChild(el("option", { value: p, selected: rule.protocol === p ? "" : null }, p));
    protocol.addEventListener("change", () => {
      rule.protocol = protocol.value;
      // Dropped rather than hidden: a range kept out of sight would be sent
      // with the rule and refused, and the operator would be looking at a form
      // that shows no range at all.
      if (rule.protocol !== "tcp" && rule.protocol !== "udp") delete rule.ports;
      else if (!rule.ports) rule.ports = { from: 1, to: 65535 };
      redraw();
    });

    const row = el("div.row", direction, protocol);

    if (rule.protocol === "tcp" || rule.protocol === "udp") {
      const from = el("input", { type: "number", min: "1", max: "65535", value: String(rule.ports.from), "aria-label": "from port" });
      const to = el("input", { type: "number", min: "1", max: "65535", value: String(rule.ports.to), "aria-label": "to port" });
      from.addEventListener("input", () => { rule.ports.from = Number(from.value); commit(); });
      to.addEventListener("input", () => { rule.ports.to = Number(to.value); commit(); });
      row.appendChild(el("span.idx", "ports"));
      row.appendChild(from);
      row.appendChild(el("span.idx", "to"));
      row.appendChild(to);
    }

    const kind = el("select");
    for (const k of ["cidr", "group"]) kind.appendChild(el("option", { value: k, selected: ruleRemoteKind(rule) === k ? "" : null }, k === "cidr" ? "from prefix" : "from group"));
    kind.addEventListener("change", () => {
      rule.remote = kind.value === "cidr" ? { cidr: "0.0.0.0/0" } : { group: "" };
      redraw();
    });
    row.appendChild(kind);

    if (ruleRemoteKind(rule) === "cidr") {
      const cidr = el("input", { type: "text", value: rule.remote.cidr || "", placeholder: "0.0.0.0/0", spellcheck: "false", "aria-label": "remote prefix" });
      cidr.addEventListener("input", () => { rule.remote.cidr = cidr.value; cidr.classList.toggle("bad", !!check("cidr", cidr.value)); commit(); });
      row.appendChild(cidr);
    } else {
      const pick = el("select", { "aria-label": "remote group" });
      pick.appendChild(el("option", { value: "" }, groups.length ? "Choose…" : "none exist yet"));
      for (const g of groups) {
        const wire = nameOf(g);
        pick.appendChild(el("option", { value: wire, selected: wire === rule.remote.group ? "" : null }, shortName(wire)));
      }
      pick.addEventListener("change", () => { rule.remote.group = pick.value; commit(); });
      row.appendChild(pick);
    }

    row.appendChild(el("button.btn", { type: "button", "aria-label": "remove", onclick: () => { rules.splice(i, 1); redraw(); } }, "\u2212"));
    host.appendChild(row);
  });

  host.appendChild(el("button.btn", { type: "button", id: "addrule", onclick: () => { rules.push(blankRule()); redraw(); } },
    "Add" + (rules.length ? " another" : " a rule")));
}

function renderRefList(form, f, host) {
  const values = Array.isArray(form.values[f.key]) ? form.values[f.key] : [];
  const offered = form.refs[f.collection] || [];
  clear(host);
  const commit = () => { form.values[f.key] = values.filter(Boolean); };
  values.forEach((value, i) => {
    const s = el("select");
    s.appendChild(el("option", { value: "" }, "— remove —"));
    for (const o of offered) {
      const wire = f.spelling === "id" ? idOf(o) : nameOf(o);
      s.appendChild(el("option", { value: wire, selected: wire === value ? "" : null }, shortName(nameOf(o))));
    }
    s.addEventListener("change", () => {
      if (!s.value) values.splice(i, 1); else values[i] = s.value;
      renderRefList(form, f, host); commit();
    });
    // The order is the order they are attached in, so it has to be changeable.
    host.appendChild(el("div.row", el("span.idx", String(i + 1)), s,
      el("button.btn", { type: "button", "aria-label": "up", disabled: i === 0 ? "" : null,
        onclick: () => { [values[i - 1], values[i]] = [values[i], values[i - 1]]; renderRefList(form, f, host); commit(); } }, "↑"),
      el("button.btn", { type: "button", "aria-label": "down", disabled: i === values.length - 1 ? "" : null,
        onclick: () => { [values[i + 1], values[i]] = [values[i], values[i + 1]]; renderRefList(form, f, host); commit(); } }, "↓")));
  });
  host.appendChild(el("button.btn", { type: "button",
    onclick: () => {
      const first = offered.length ? (f.spelling === "id" ? idOf(offered[0]) : nameOf(offered[0])) : "";
      values.push(first); form.values[f.key] = values; renderRefList(form, f, host);
    } },
    offered.length ? "Add" + (values.length ? " another" : "") : "Nothing to attach yet"));
}

// ---- the dialog ------------------------------------------------------------

function closeDialog() {
  const d = $("dialog"), s = $("dialogscrim");
  if (d) d.remove();
  if (s) s.remove();
}

function nest(flat) {
  const out = {};
  for (const [path, value] of Object.entries(flat)) {
    if (value === "" || value === undefined || value === null) continue;
    if (Array.isArray(value) && !value.length) continue;
    const segs = path.split(".");
    let here = out;
    for (const seg of segs.slice(0, -1)) here = here[seg] || (here[seg] = {});
    here[segs[segs.length - 1]] = value;
  }
  return out;
}

function flatten(obj, fields) {
  const out = {};
  for (const f of fields) {
    const v = at(obj, f.key);
    if (v !== undefined && v !== null) out[f.key] = v;
  }
  return out;
}

/// The body a create takes. The contract says "id in the body" and no more, so
/// the exact shape is decided in one place — if the API wants another, this is
/// the function that changes and nothing else does.
const createBody = (id, specValues) => ({ id, spec: specValues });

function openForm({ coll, title, blurb, values, submitLabel, onSubmit, candidates, locked }) {
  closeDialog();
  const form = {
    coll, values: { ...values },
    errs: {}, inputs: {}, pickers: [], refs: {}, boxes: {},
    // A picker the platform answers for, and the fields this dialog was opened
    // about rather than opened to ask.
    candidates: candidates || null, locked: locked || [],
    setValue(key, value) {
      form.values[key] = value;
      const input = form.inputs[key];
      if (input) input.value = value ?? "";
      // A picker is set through the same path that fills it, so a value derived
      // from another object is matched exactly as one read off the wire is.
      // Assigning straight to the select silently clears anything it has no
      // option for — which is every bare id.
      const picker = form.pickers.find((p) => p.field.key === key);
      if (picker) fillPicker(form, picker);
    },
    revalidate() { /* replaced below, once the whole form exists */ },
    refilter() {},
  };

  // A modal task dims what it covers. It does not dismiss on a click outside:
  // that is the easiest gesture to make by accident and it would throw away
  // everything typed. Cancel and Escape both say so deliberately.
  const scrim = el("div", { id: "dialogscrim" });
  const dialog = el("div", { id: "dialog", role: "dialog", "aria-label": title });
  const common = el("div.fields"), advanced = el("div.fields.hidden");
  const problems = el("p.err.hidden");

  form.revalidate = () => {
    const bad = crossCheck(coll, form.values);
    for (const [key, setErr] of Object.entries(form.errs)) {
      const f = coll.fields.find((x) => x.key === key);
      if (!f) continue;
      const own = check(f.check || "none", form.values[key] ?? "");
      setErr(own || bad[key] || "");
    }
  };
  form.refilter = (changed) => {
    for (const p of form.pickers) {
      if (p.field.filterBy === changed) fillPicker(form, p);
    }
  };

  for (const f of coll.fields) {
    // Derived fields are rendered, never asked: a value the platform computes
    // is worth showing before the request — which node will open the volume,
    // which VNI was assigned — and the control is disabled below.
    (f.advanced ? advanced : common).appendChild(fieldControl(form, f));
  }

  dialog.appendChild(el("h2", title));
  dialog.appendChild(el("p.prose", blurb || coll.blurb));
  dialog.appendChild(el("div", { style: "height:var(--space-6)" }));
  dialog.appendChild(common);
  if (advanced.childElementCount) {
    // The common path first, the rest one level deeper. Not hidden — one click
    // away, and the click says how many are behind it.
    const n = advanced.childElementCount;
    const toggle = el("button.disclose", { type: "button", id: "moresettings",
      "aria-expanded": "false", "aria-controls": "moresettingsfields" },
      "More settings (" + n + ")");
    // The deeper level is a level: what it opens sits inside its own inset
    // surface, so the rest of the form reads as *behind* the common path rather
    // than as more of it. Without the container the disclosure is a button
    // floating between two identical field grids, which is the same crowding
    // one click later.
    advanced.id = "moresettingsfields";
    advanced.classList.add("deeper");
    toggle.addEventListener("click", () => {
      const shown = !advanced.classList.toggle("hidden");
      toggle.setAttribute("aria-expanded", String(shown));
      toggle.classList.toggle("open", shown);
      toggle.textContent = (shown ? "Fewer settings" : "More settings (" + n + ")");
    });
    dialog.appendChild(toggle);
    dialog.appendChild(advanced);
  }
  dialog.appendChild(problems);

  const submit = el("button.btn.primary", { type: "button", id: "submitform" }, submitLabel);
  submit.addEventListener("click", async () => {
    form.revalidate();
    const missing = coll.fields.filter((f) => f.required && !f.derived &&
      (form.values[f.key] === undefined || form.values[f.key] === ""));
    const bad = Object.entries(form.errs).find(([key]) => {
      const f = coll.fields.find((x) => x.key === key);
      return f && (check(f.check || "none", form.values[key] ?? "") || crossCheck(coll, form.values)[key]);
    });
    if (missing.length || bad) {
      fill(problems, missing.length
        ? "Still needed: " + missing.map((f) => f.label).join(", ")
        : "Fix " + (coll.fields.find((f) => f.key === bad[0]) || {}).label + " first.");
      problems.classList.remove("hidden");
      return;
    }
    problems.classList.add("hidden");
    submit.setAttribute("disabled", "");
    try {
      await onSubmit(form);
      closeDialog();
    } catch (e) {
      submit.removeAttribute("disabled");
      // The API points at the offending path when there is one, so the message
      // lands on the control rather than in a banner nobody can act on. When it
      // does not, one refusal is still placeable without guessing: the only
      // thing that can already exist is the name.
      const named = e.field || (e.code === "ALREADY_EXISTS" ? "id" : "");
      const key = named.replace(/^spec\./, "");
      if (key && form.errs[key]) {
        form.errs[key](e.message);
        fill(problems, "The API refused " + key + ".");
      } else {
        fill(problems, e.message);
      }
      problems.classList.remove("hidden");
    }
  });

  dialog.appendChild(el("div.dialogacts",
    el("span.grow"),
    el("button.btn", { type: "button", id: "cancelform", onclick: closeDialog }, "Cancel"),
    submit));

  document.body.appendChild(scrim);
  document.body.appendChild(dialog);

  // The pickers are filled after the form is on screen: a select that waits for
  // a round trip before anything renders is a dialog that appears late.
  for (const p of form.pickers) fillPicker(form, p);
  return form;
}

async function fillPicker(form, p) {
  const f = p.field;
  // Some choices are not "everything that exists": they are what the platform
  // has already said it would accept for this particular object. Where there is
  // such an answer it is the picker, because offering the rest and letting the
  // API refuse them is making somebody find out the hard way.
  const ask = (form.candidates || {})[f.key];
  if (ask) return fillFromAnswer(form, p, await ask(form));
  let offered = await options(f.collection);
  if (f.filterBy) {
    const want = form.values[f.filterBy];
    // Only what belongs: a subnet picker on a chosen network offers that
    // network's subnets, and nothing at all before one is chosen.
    offered = want ? offered.filter((o) => at(spec(o), f.filterBy) === want) : [];
  }
  form.refs[f.collection] = offered;
  if (p.render) { p.render(); return; }

  const s = p.select;
  const keep = form.values[f.key] ?? "";
  clear(s);
  s.appendChild(el("option", { value: "" },
    f.filterBy && !form.values[f.filterBy] ? "Choose a " + f.filterBy + " first" :
      offered.length ? (f.required ? "Choose…" : "— none —") : "none exist yet"));
  // How the platform spells this reference. A node is a bare id — that is what
  // the scheduler writes and what ownership is decided by — and everything else
  // is a full resource name. The API refuses the wrong one at the door, so the
  // picker has to produce the right one rather than a plausible one.
  const wire = (o) => (f.spelling === "id" ? idOf(o) : nameOf(o));
  for (const o of offered) {
    s.appendChild(el("option", { value: wire(o) },
      shortName(nameOf(o)) + optionNote(f.collection, o)));
  }
  if (offered.some((o) => wire(o) === keep)) { s.value = keep; return; }
  if (!keep) return;
  // Something already on the object is spelled the other way. Keep the spelling
  // that arrived rather than rewriting a field this edit never touched — a form
  // that silently normalises somebody's value changes what the object says
  // without anybody asking. If the API refuses it, it says so, loudly.
  const same = offered.find((o) => nameOf(o) === keep || idOf(o) === keep);
  const option = same && [...s.options].find((o) => o.value === wire(same));
  if (option) { option.value = keep; s.value = keep; }
  else { form.values[f.key] = ""; s.value = ""; }
}

/// An answer of the shape `{ candidates: [{ id, ok, why, detail }], trouble }`,
/// rendered as a picker that can only produce something the platform has said
/// yes to.
///
/// What cannot be chosen is still *shown*, disabled and with the sentence
/// beside it. Leaving it out would answer "why can I not send it there" with
/// silence, and silence is what sends somebody to a log file.
function fillFromAnswer(form, p, answer) {
  const f = p.field, s = p.select;
  const list = (answer && answer.candidates) || [];
  const keep = form.values[f.key] ?? "";
  const usable = list.filter((c) => c.ok);

  clear(s);
  s.appendChild(el("option", { value: "" },
    !list.length ? "nothing to choose from"
      : usable.length ? (f.required ? "Choose…" : "— none —")
        : "none of them can take it"));
  for (const c of list) {
    s.appendChild(el("option", { value: c.id, disabled: c.ok ? null : "" },
      c.ok ? c.id + (c.detail ? "  " + c.detail : "") : c.id + " — cannot take it"));
  }
  if (usable.some((c) => c.id === keep)) s.value = keep;
  else { form.values[f.key] = ""; s.value = ""; }

  // The reasons, under the control, where the choice is being made.
  const host = p.host || (p.host = el("div.candidates"));
  if (!host.parentNode) s.parentNode.insertBefore(host, s.nextSibling);
  const refused = list.filter((c) => !c.ok);
  // A field carrying an answer needs the row: squeezed into a column of the
  // fields grid, the sentence explaining a refusal wraps one word per line,
  // which is a sentence nobody reads.
  const box = form.boxes[f.key];
  if (box && refused.length) box.classList.add("wide");
  fill(host,
    answer && answer.trouble ? el("p.err", answer.trouble) : null,
    refused.length
      ? el("table.reject",
          el("thead", el("tr",
            el("th", { style: "width:120px" }, "Cannot take it"),
            el("th", { style: "width:150px" }, "Because"),
            el("th", ""))),
          el("tbody", refused.map((c) => el("tr",
            el("td.mono", String(c.id)),
            el("td.mono", String(c.why || "—")),
            el("td.muted", String(c.detail || ""))))))
      : null);
}

/// What tells two options apart, beside the name. A list of digests is not a
/// choice anybody can make.
function optionNote(collectionId, o) {
  const st = status(o), sp = spec(o);
  if (collectionId === "images") {
    return "  " + (pick(sp, "format") || "") + " " + bytes(pick(sp, "sizeBytes"));
  }
  if (collectionId === "nodes") {
    const free = Number(pick(pick(st, "capacity") || {}, "vcpus") || 0) -
      Number(pick(pick(st, "allocated") || {}, "vcpus") || 0);
    return "  " + (pick(sp, "schedulable") === false ? "draining" : free + " vCPU free");
  }
  if (collectionId === "volumes") return "  " + (pick(sp, "sizeGib") || 0) + " GiB";
  if (collectionId === "subnets") return "  " + (pick(sp, "cidr") || "");
  if (collectionId === "instances") return "  " + (pick(st, "state") || "Unknown");
  return "";
}

function defaults(coll) {
  const out = {};
  for (const f of coll.fields) {
    if (f.kind === "switch") out[f.key] = f.key === "schedulable";
    else if (f.kind === "choice" && f.options.length) out[f.key] = f.options[0].value;
    else if (f.kind === "number" && !f.advanced) out[f.key] = f.min;
  }
  if (coll.id === "networks") out.mtu = 1500;
  if (coll.id === "volumes") out.sizeGib = 10;
  if (coll.id === "instances") { out.vcpus = 2; out.memoryMib = 2048; out.rootDiskGib = 20; }
  return out;
}

/// `opts` is how one create differs from another — a title, values decided
/// before the dialog opened, a picker the platform answers for. Everything else
/// about creating is the same for every collection, which is why there is one
/// of these rather than a screen per type.
function openCreate(coll, opts = {}) {
  const form = openForm({
    coll,
    title: opts.title || "New " + coll.singular,
    blurb: opts.blurb,
    values: { ...defaults(coll), ...(opts.values || {}) },
    submitLabel: opts.submitLabel || "Create",
    candidates: opts.candidates,
    locked: opts.locked,
    async onSubmit(f) {
      const id = f.values.__id;
      const body = createBody(id, nest(settable(coll, f.values)));
      const answer = await create(coll, body);
      forgetOptions(coll.id);
      toast(answer && answer.operation
        ? "Asked for. Operation " + shortName(answer.operation) + " is following it."
        : "Asked for. Watch it converge below.");
      await show(coll.id);
      const made = view.items.find((r) => idOf(r) === id);
      if (made) openSheet(coll, made);
    },
  });

  // The id is asked for first and separately: it is the one thing that cannot
  // be changed afterwards, and it is not part of the spec.
  const dialog = $("dialog");
  const idField = el("div.field",
    el("label", { for: "f-id" }, "Id", el("span.req", " ·")),
    el("input", { id: "f-id", type: "text", spellcheck: "false", placeholder: coll.singular + "-1" }),
    el("div.hint", "Lowercase, and permanent — it is how everything else will refer to this " + coll.singular + "."),
    el("div.err.hidden", { id: "f-id-err" }));
  const input = idField.querySelector("input");
  // Suggested, not imposed: a migration proposed from an instance can name
  // itself after the guest, and an operator who wants another name types one.
  if (opts.id) { input.value = opts.id; form.values.__id = opts.id; }
  input.addEventListener("input", () => {
    form.values.__id = input.value;
    const m = input.value ? CHECKS.id(input.value) : "";
    const box = $("f-id-err");
    box.textContent = m; box.classList.toggle("hidden", !m);
    input.classList.toggle("bad", !!m);
  });
  // Under both names: the form knows it as `__id` because it is not part of the
  // spec, and the API refuses it as `id`. A refusal that cannot find its
  // control ends up in a banner, which is the one place it cannot be acted on.
  form.errs.__id = form.errs.id = (m) => {
    const box = $("f-id-err");
    box.textContent = m || ""; box.classList.toggle("hidden", !m);
    input.classList.toggle("bad", !!m);
  };
  dialog.insertBefore(el("div.fields", idField), dialog.querySelector(".fields"));
  input.focus();
  return form;
}

/// What the form may send: not the id, which is not part of the spec, and not
/// anything the platform derives. A client that echoes a derived value back is
/// a client that can disagree with it — and the disagreement would be about a
/// copy that went stale between the form opening and Create being pressed.
const settable = (coll, values) => {
  const out = { ...values };
  delete out.__id;
  for (const f of coll.fields) if (f.derived) delete out[f.key];
  return out;
};

function openEdit(coll, r) {
  return openForm({
    coll,
    title: "Edit " + idOf(r),
    values: flatten(spec(r), coll.fields),
    submitLabel: "Save",
    async onSubmit(f) {
      // The whole spec, merged over what was read, so a field this console does
      // not know about is not dropped by an edit that never touched it.
      const merged = { ...spec(r), ...nest(settable(coll, f.values)) };
      await patch(coll, idOf(r), { spec: merged }, revision(r));
      forgetOptions(coll.id);
      toast("Asked for. The generation moves; watch the observation catch up.");
      const fresh = await get(coll, idOf(r)).catch(() => null);
      if (fresh) { openSheet(coll, fresh); }
      show(coll.id);
    },
  });
}
