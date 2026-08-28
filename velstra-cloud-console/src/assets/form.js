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
/// Consequences of what is filled in — true, not wrong.
///
/// Distinct from `crossCheck`, which returns *errors* and stops the form. These
/// are things the platform will accept and somebody would rather know before
/// pressing Create than discover afterwards.
///
/// The first one is the one that sent me looking: an instance with no port is
/// legal, is what a new person creates by default because the field sits one
/// disclosure deeper, and produces a guest that cannot be reached, cannot be
/// logged into, and cannot fetch its own metadata. Nothing said so anywhere —
/// the machine simply ran and answered nobody.
function consequences(coll, values) {
  const out = [];
  if (coll.id === "instances") {
    const ports = values.ports;
    if (!Array.isArray(ports) || !ports.length) {
      out.push("This guest will have no network. It cannot be reached, and its "
        + "cloud-init cannot fetch the SSH keys and hostname this platform would "
        + "give it. Attach a port under More settings, or add one later.");
    }
    if (!values.sshKeys && !values.userData) {
      out.push("No SSH key and no cloud-init. A stock cloud image has no password, "
        + "so the only way in will be the console.");
    }
  }
  return out;
}

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

/// An epoch millisecond as `datetime-local` wants it: the operator's own
/// timezone, and no seconds. Built by hand rather than with `toISOString`,
/// which would hand back UTC and quietly move every window by the offset.
function localMoment(ms) {
  const d = new Date(ms);
  const pad = (n) => String(n).padStart(2, "0");
  return d.getFullYear() + "-" + pad(d.getMonth() + 1) + "-" + pad(d.getDate()) +
    "T" + pad(d.getHours()) + ":" + pad(d.getMinutes());
}

/// "40 minutes", "3 hours", "2 days" — the granularity somebody is deciding at,
/// not the one the number happens to be in.
function minutesAsWords(m) {
  if (m < 90) return m + (m === 1 ? " minute" : " minutes");
  const h = Math.round(m / 60);
  if (h < 48) return h + (h === 1 ? " hour" : " hours");
  const d = Math.round(h / 24);
  return d + (d === 1 ? " day" : " days");
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
  const WIDE = ["lines", "refList", "textList", "ruleList", "diskList", "poolList", "listenerList"];
  const box = el("div.field" + (WIDE.includes(f.kind) ? ".wide" : ""));
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
    case "moment": {
      // A calendar and a clock, in the operator's own timezone. What is stored
      // is milliseconds since the epoch, and a person asked to type one of
      // those either pastes the wrong number or works one out by hand — both
      // of which end with a machine going out of service at the wrong hour.
      const input = el("input", { id, type: "datetime-local" });
      const also = el("div.also");
      // Empty means "in an hour" rather than "never": that is what most
      // windows want, and it is arithmetic somebody would otherwise do.
      const start = typeof current === "number" && current
        ? current
        : Date.now() + (f.defaultInMinutes || 0) * 60_000;
      input.value = localMoment(start);
      commit(start, input);
      const reread = () => {
        const at = input.value ? new Date(input.value).getTime() : NaN;
        if (Number.isNaN(at)) { also.textContent = ""; return; }
        // Said back in plain words, because a datetime box is read as digits
        // and "in 40 minutes" is what somebody is actually deciding about.
        const away = Math.round((at - Date.now()) / 60_000);
        also.textContent = away >= 0 ? "in " + minutesAsWords(away) : minutesAsWords(-away) + " ago";
      };
      input.addEventListener("input", () => {
        const at = input.value ? new Date(input.value).getTime() : NaN;
        commit(Number.isNaN(at) ? "" : at, input);
        setErr(input.value && Number.isNaN(at) ? "not a moment in time" : "");
        reread();
      });
      box.appendChild(input);
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
    case "diskList": {
      const host = el("div.disks", { id });
      // Through the picker machinery, like the rule list's remote groups: the
      // nodes whose disks are on offer are fetched exactly the way every other
      // reference is, rather than this control growing a loader of its own.
      form.pickers.push({
        field: { key: f.key, collection: f.collection, spelling: "id" },
        list: host,
        render: () => renderDiskList(form, f, host, setErr),
      });
      renderDiskList(form, f, host, setErr);
      box.appendChild(host);
      break;
    }
    case "poolList": {
      const host = el("div.rowlist", { id });
      renderPoolList(form, f, host, setErr);
      box.appendChild(host);
      break;
    }
    case "listenerList": {
      const host = el("div.rowlist", { id });
      renderListenerList(form, f, host, setErr);
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

// One listener is a protocol, the port the address answers on, and the port
// the members answer on. Rendered together, like a rule, because three
// parallel list fields would let somebody assemble a listener out of rows that
// do not line up. TCP and UDP only: the fabric's datapath balances no others,
// and a wider choice is a control that produces an error.
const LISTENER_PROTOCOLS = ["Tcp", "Udp"];

function blankListener() {
  return { protocol: "Tcp", port: 443, memberPort: 0 };
}

function renderListenerList(form, f, host, setErr) {
  if (!Array.isArray(form.values[f.key])) form.values[f.key] = [];
  const listeners = form.values[f.key];
  clear(host);

  const commit = () => {
    form.values[f.key] = listeners;
    // Only what the API would refuse: a port of zero, or one address
    // answering one port twice.
    let bad = "";
    const seen = new Set();
    for (const l of listeners) {
      if (!l.port) bad = "say which port the address answers on";
      const claim = l.protocol + "/" + l.port;
      if (seen.has(claim)) bad = "two listeners claim " + claim;
      seen.add(claim);
    }
    setErr(bad);
    form.revalidate(f.key);
  };

  const redraw = () => { renderListenerList(form, f, host, setErr); commit(); };

  listeners.forEach((listener, i) => {
    const protocol = el("select", { "aria-label": "protocol" });
    for (const p of LISTENER_PROTOCOLS) {
      protocol.appendChild(el("option", { value: p, selected: listener.protocol === p ? "" : null }, p.toUpperCase()));
    }
    protocol.addEventListener("change", () => { listener.protocol = protocol.value; commit(); });

    const port = el("input", { type: "number", min: "1", max: "65535", value: String(listener.port || ""), "aria-label": "port" });
    port.addEventListener("input", () => { listener.port = Number(port.value); commit(); });

    // Zero keeps the client's own destination port, which is what forwarding
    // 443 to 443 wants and what the placeholder says.
    const member = el("input", { type: "number", min: "0", max: "65535", placeholder: "same",
      value: listener.memberPort ? String(listener.memberPort) : "", "aria-label": "member port" });
    member.addEventListener("input", () => { listener.memberPort = Number(member.value || 0); commit(); });

    host.appendChild(el("div.row",
      protocol,
      el("span.idx", "port"),
      port,
      el("span.idx", "members on"),
      member,
      el("button.btn", { type: "button", "aria-label": "remove", onclick: () => { listeners.splice(i, 1); redraw(); } }, "−")));
  });

  host.appendChild(el("button.btn", { type: "button", id: "addlistener", onclick: () => { listeners.push(blankListener()); redraw(); } },
    "Add" + (listeners.length ? " another" : " a listener")));
}

// Picking disks for Ceph, which is the one control on this page that destroys
// something.
//
// It is inverted from every other picker here. The rest offer what exists and
// let the API refuse the rest; this one lists *everything* each node can see,
// offers only what is provably empty, and prints the reason beside every row it
// will not take. Greying a row out answers "why can I not select this disk" with
// silence, and silence is what sends somebody to a terminal to find out
// something the platform already knew.

/// The refusal for one device, in the model's own words, or "" if it may be had.
///
/// None of the wording is written here. It arrives in the schema, word for word
/// from `ceph::may_consume`, pinned to it by a test in the API crate — so the
/// script does substitution and nothing else, and there is no second opinion in
/// JavaScript about what a filesystem on a disk means.
function diskRefusal(f, device) {
  const state = pick(device, "state") || { kind: "Free" };
  const kind = pick(state, "kind") || "Free";
  const say = (text) => String(text).replace(/\{(\w+)\}/g, (_, key) => {
    const v = key === "minGib" ? f.minGib
      : key === "sizeGib" ? pick(device, "sizeGib")
        : pick(state, key);
    return v === undefined || v === null ? "?" : String(v);
  });
  const template = (f.refusals || []).find((r) => r.kind === kind);
  if (template) return say(template.text);
  // Not free, and nothing here has a sentence for it: a node running a newer
  // agent than this page. Refused rather than offered, because the safe
  // direction when the answer is unknown is the conservative one and this is
  // the one control where being wrong erases somebody's data.
  if (kind !== "Free") return say(f.unknown);
  if (Number(pick(device, "sizeGib") || 0) < Number(f.minGib)) return say(f.tooSmall);
  return "";
}

/// What tells two disks apart at a glance. Spinning versus solid state is here
/// because mixing them in one pool is a decision rather than an accident, and
/// the kernel name because that is what an operator is holding an `lsblk`
/// against.
function diskNote(device) {
  return [
    (pick(device, "sizeGib") || 0) + " GiB",
    pick(device, "rotational") ? "spinning" : "solid state",
    pick(device, "model") || null,
    pick(device, "kernelName") || null,
  ].filter(Boolean).join(" · ");
}

function renderDiskList(form, f, host, setErr) {
  // The array lives on the form for the same reason the rule list's does: this
  // control redraws itself after every change, and a disk pushed into a local
  // copy would be gone by the time the redraw read the values back.
  if (!Array.isArray(form.values[f.key])) form.values[f.key] = [];
  const chosen = form.values[f.key];
  // Filled in a moment later by the picker; the control is drawn once before
  // that, so it must not assume the fetch has happened.
  const nodes = (form.refs || {})[f.collection] || [];
  clear(host);

  const where = (node, device) =>
    chosen.findIndex((o) => pick(o, "node") === node && pick(o, "device") === device);
  const commit = () => {
    form.values[f.key] = chosen;
    // Only what the platform could not carry out. Two OSDs cannot be made from
    // one disk, and a spec that asks for it is one the second step fails on
    // with an error about a device that is already in use.
    const seen = new Set();
    let bad = "";
    for (const o of chosen) {
      const key = pick(o, "node") + "\u0000" + pick(o, "device");
      if (seen.has(key)) bad = pick(o, "device") + " on " + pick(o, "node") + " is listed twice, and one disk makes one OSD";
      seen.add(key);
    }
    setErr(bad);
  };
  const redraw = () => { renderDiskList(form, f, host, setErr); commit(); };
  const add = (node, device) => { chosen.push({ node, device }); redraw(); };
  const drop = (node, device) => {
    const i = where(node, device);
    if (i >= 0) chosen.splice(i, 1);
    redraw();
  };

  // Above the list, not under it. A warning below a list of buttons is a
  // warning read after the click.
  host.appendChild(el("p.warn", f.warning));

  for (const n of nodes) {
    const node = idOf(n);
    const devices = at(status(n), "devices") || [];
    const rows = el("div.diskrows");
    for (const d of devices) {
      const device = pick(d, "path") || "";
      const taken = where(node, device) >= 0;
      // A disk already in the spec reads as taken whatever it reports now.
      // Ceph reports its own disks as OSDs, which `may_consume` refuses — and a
      // control that believed the refusal would render every disk of a working
      // cluster as unavailable, with no way left to remove one.
      const why = taken ? "" : diskRefusal(f, d);
      const row = el("div.disk" + (taken ? ".chosen" : why ? ".refused" : ""),
        { "data-node": node, "data-device": device });
      row.appendChild(el("span.mono", device));
      row.appendChild(el("span.note", diskNote(d)));
      row.appendChild(why
        ? el("span.why", "Not offered: " + why)
        : el("button.btn" + (taken ? "" : ".primary"), {
          type: "button", "data-disk": taken ? "remove" : "add",
          onclick: () => (taken ? drop(node, device) : add(node, device)),
        }, taken ? "Remove" : "Add"));
      rows.appendChild(row);
    }
    host.appendChild(el("div.disknode",
      el("div.diskhost", node,
        devices.length ? null : el("span.note", "reports no disks")),
      rows));
  }

  // Chosen, and no node is reporting it. A node that is down looks exactly like
  // this, and dropping these rows from the screen would let an edit that never
  // touched them silently look like it had removed them.
  const stray = chosen.filter((o) => !nodes.some((n) => idOf(n) === pick(o, "node") &&
    (at(status(n), "devices") || []).some((d) => pick(d, "path") === pick(o, "device"))));
  if (stray.length) {
    host.appendChild(el("div.disknode",
      el("div.diskhost", "Not reported",
        el("span.note", "asked for, and no node is reporting the disk — a node that is down looks like this")),
      el("div.diskrows", stray.map((o) => {
        const node = pick(o, "node"), device = pick(o, "device");
        return el("div.disk.chosen", { "data-node": node, "data-device": device },
          el("span.mono", device),
          el("span.note", "on " + node),
          el("button.btn", { type: "button", "data-disk": "remove", onclick: () => drop(node, device) }, "Remove"));
      }))));
  }

  if (!nodes.length) host.appendChild(el("div.note", "No node has reported its disks yet."));
}

/// A pool is a name and the two numbers that decide what it survives, and they
/// are rendered together because the second only means anything against the
/// first: a floor equal to the copies is a pool that stops taking writes the
/// moment any node reboots.
function blankPool(f) {
  return { pool: "", size: Number(f.defaultSize), minSize: Number(f.defaultMinSize) };
}

function renderPoolList(form, f, host, setErr) {
  if (!Array.isArray(form.values[f.key])) form.values[f.key] = [];
  const pools = form.values[f.key];
  clear(host);

  const commit = () => {
    form.values[f.key] = pools;
    let bad = "";
    for (const p of pools) {
      if (!p.pool) bad = "a pool needs a name";
      else if (CHECKS.id(p.pool)) bad = "a pool's name is " + CHECKS.id(p.pool);
      else if (Number(p.size) < 1 || Number(p.minSize) < 1) bad = "a pool keeps at least one copy and writes at least one";
      else if (Number(p.minSize) > Number(p.size)) {
        bad = "the floor is higher than the number of copies, so nothing could ever be written to " + p.pool;
      }
    }
    setErr(bad);
  };
  const redraw = () => { renderPoolList(form, f, host, setErr); commit(); };

  pools.forEach((p, i) => {
    const name = el("input", { type: "text", value: p.pool || "", placeholder: "volumes",
      spellcheck: "false", "aria-label": "pool name" });
    name.addEventListener("input", () => {
      p.pool = name.value;
      name.classList.toggle("bad", !!(name.value && CHECKS.id(name.value)));
      commit();
    });
    const copies = el("input", { type: "number", min: "1", max: "10", value: String(p.size), "aria-label": "copies" });
    copies.addEventListener("input", () => { p.size = Number(copies.value); commit(); });
    const floor = el("input", { type: "number", min: "1", max: "10", value: String(p.minSize), "aria-label": "write floor" });
    floor.addEventListener("input", () => { p.minSize = Number(floor.value); commit(); });
    host.appendChild(el("div.row", name,
      el("span.lab", "copies"), copies,
      el("span.lab", "floor"), floor,
      el("button.btn", { type: "button", "aria-label": "remove",
        onclick: () => { pools.splice(i, 1); redraw(); } }, "−")));
  });

  host.appendChild(el("button.btn", { type: "button", id: "addpool",
    onclick: () => { pools.push(blankPool(f)); redraw(); } },
  "Add" + (pools.length ? " another" : " a pool")));
}

function renderRefList(form, f, host) {
  const values = Array.isArray(form.values[f.key]) ? form.values[f.key] : [];
  const offered = form.refs[f.collection] || [];
  clear(host);
  // The consequences beside the button are computed from the values, so a
  // control that changes them and says nothing leaves a sentence on screen that
  // has stopped being true — "this guest will have no network", still there
  // after somebody attached one.
  const commit = () => {
    form.values[f.key] = values.filter(Boolean);
    form.revalidate();
  };
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
  host.appendChild(el("button.btn", { type: "button", disabled: offered.length ? null : "",
    onclick: () => {
      const first = offered.length ? (f.spelling === "id" ? idOf(offered[0]) : nameOf(offered[0])) : "";
      values.push(first); form.values[f.key] = values; renderRefList(form, f, host);
      form.revalidate();
    } },
    offered.length ? "Add" + (values.length ? " another" : "") : "Nothing to attach yet"));
  // "Nothing to attach yet" is true and useless on its own. A project that has
  // never had a network reaches this on its very first guest, and what it needs
  // is the order to do things in — not a disabled button.
  if (!offered.length && f.whenEmpty) {
    host.appendChild(el("p.muted", f.whenEmpty));
  }
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

  // Said beside the button, not on a field: these are not mistakes, and
  // marking a field bad for one would refuse a thing the platform accepts.
  const notes = el("div.consequences");
  form.revalidate = () => {
    const bad = crossCheck(coll, form.values);
    for (const [key, setErr] of Object.entries(form.errs)) {
      const f = coll.fields.find((x) => x.key === key);
      if (!f) continue;
      const own = check(f.check || "none", form.values[key] ?? "");
      setErr(own || bad[key] || "");
    }
    clear(notes);
    for (const line of consequences(coll, form.values)) {
      notes.appendChild(el("p.note", line));
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
  dialog.appendChild(notes);
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
  // Once, on open. The consequences of what is *already* filled in are the ones
  // that matter most — a form that only spoke after somebody typed said nothing
  // at all to the person who pressed Create straight away, which is exactly the
  // person it was written for.
  form.revalidate();
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
  if (!offered.length && f.whenEmpty && form.boxes[f.key]) {
    form.boxes[f.key].appendChild(el("p.muted", f.whenEmpty));
  }
  // How the platform spells this reference. A node is a bare id — that is what
  // the scheduler writes and what ownership is decided by — and everything else
  // is a full resource name. The API refuses the wrong one at the door, so the
  // picker has to produce the right one rather than a plausible one.
  const wire = (o) => (f.spelling === "id" ? idOf(o) : nameOf(o));
  // A family first, where images declare one: `families/debian-13` asks the
  // platform for the newest and is what most people mean — the concrete builds
  // below it are for pinning. Resolved once at create and written down, so a
  // guest never changes its operating system on a restart.
  if (f.collection === "images") {
    const families = [...new Set(offered.map((o) => String(pick(spec(o), "family") || "").trim()).filter(Boolean))];
    for (const fam of families.sort()) {
      s.appendChild(el("option", { value: "families/" + fam }, fam + " — always the newest"));
    }
  }
  for (const o of offered) {
    // An image leads with what it *is*; everything else leads with its name,
    // which for everything else is already the readable thing.
    const label = f.collection === "images"
      ? imageTitle(o) + optionNote(f.collection, o)
      : shortName(nameOf(o)) + optionNote(f.collection, o);
    s.appendChild(el("option", { value: wire(o) }, label));
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
/// What to call an image, for somebody who has not memorised its digest.
///
/// An image's *name* carries its digest, because that is what makes fetching
/// one verifiable — and it is unreadable. A picker that offered
/// `images/sha256-cbf3e1f5…` and nothing else asked a person to choose an
/// operating system from a hash, and two images built from the same bytes in
/// two projects rendered identically.
///
/// So: the file it came from, or the guest it was captured from, and then the
/// facts that tell two similar ones apart.
/// What to call an image in front of a person.
///
/// Its *name* is its digest, because that is what makes fetching one verifiable
/// — and `images/sha256-cbf3e1f588f02f8d738dbecb…` is not an answer to "which
/// operating system is this". So: the family it belongs to, and which one in the
/// family, both of which somebody chose and typed.
///
/// The two fallbacks are for images published before a family could be declared.
/// A capture says which guest it came from; anything else is guessed out of the
/// source URL's last path segment, which is a guess and is why declaring a
/// family exists.
function imageTitle(o) {
  const sp = spec(o);
  const family = String(pick(sp, "family") || "").trim();
  if (family) {
    const version = String(pick(sp, "version") || "").trim();
    return version ? family + " " + version : family;
  }
  const from = pick(sp, "sourceInstance");
  if (from) return "from " + shortName(from);
  const url = String(pick(sp, "sourceUrl") || "");
  const file = url.split("?")[0].split("/").filter(Boolean).pop() || "";
  const base = file.replace(/\.(qcow2|raw|img|iso)$/i, "");
  return base || "image";
}

function optionNote(collectionId, o) {
  const st = status(o), sp = spec(o);
  if (collectionId === "images") {
    // Where it lives, because two projects may hold the same bytes and a
    // catalogue image is not the same offer as one of your own.
    const name = nameOf(o);
    const where = name.startsWith("projects/")
      ? name.split("/")[1]
      : "catalogue";
    const digest = (idOf(o) || "").replace(/^sha256-/, "").slice(0, 8);
    // A size nobody has measured yet is left out. It is reported by whoever
    // fetches the bytes, so a freshly published catalogue entry has none — and
    // rendering that as "0" reads as an empty image rather than an unknown one.
    const size = Number(pick(sp, "sizeBytes")) || 0;
    return "  " + (pick(sp, "format") || "") + (size ? " " + bytes(size) : "")
      + "  ·  " + where + "  ·  " + digest;
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
    values: {
      ...defaults(coll),
      // Where it goes, for the collections that have two answers. The project
      // leads, because that is what most creates are and what a tenant sees when
      // there is no choice at all.
      ...(offersBothScopes(coll) && session.project ? { __scope: "project" } : {}),
      ...(opts.values || {}),
    },
    submitLabel: opts.submitLabel || "Create",
    candidates: opts.candidates,
    locked: opts.locked,
    async onSubmit(f) {
      const id = f.values.__id;
      // `__scope` is the form's, not the object's: it decides the address the
      // create goes to and must not travel in the body as if it were a field.
      const scope = f.values.__scope;
      const values = { ...f.values };
      delete values.__scope;
      const body = createBody(id, nest(settable(coll, values)));
      const answer = await create(coll, body, scope);
      forgetOptions(coll.id);
      // A registration token comes back exactly once — the platform keeps a
      // hash and cannot show it again — so it gets a panel of its own rather
      // than a toast that scrolls away, with the one command it is for.
      if (answer && answer.nodeToken) {
        await show(coll.id);
        // After the form's own dialog is gone, which the caller closes the
        // moment this returns. `queueMicrotask` is not enough — the close is
        // in the caller's next statement, not in a task — so the panel is
        // opened from a timer that runs after it.
        const token = answer.nodeToken;
        setTimeout(() => showNodeToken(id, token), 0);
        return;
      }
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
  // Where it goes, before what it is: an image published to the cell and one
  // published to a project are different offers, and the second question reads
  // differently once the first is answered.
  const first = [];
  if (offersBothScopes(coll) && session.project) {
    const pick = el("select", { id: "f-scope" },
      el("option", { value: "project" }, "This project — " + session.project),
      el("option", { value: "global" }, "The whole cell — everybody can boot it"));
    pick.value = form.values.__scope || "project";
    pick.addEventListener("change", () => { form.values.__scope = pick.value; });
    first.push(el("div.field",
      el("label", { for: "f-scope" }, "Where"),
      pick,
      el("div.hint",
        "A cell image is the catalogue: every project may boot it, and only a cell "
        + "operator may publish one. A project image belongs to " + session.project
        + " and is invisible to everybody else.")));
  }
  first.push(idField);
  dialog.insertBefore(el("div.fields", ...first), dialog.querySelector(".fields"));
  input.focus();
  return form;
}

/// The one-time registration token, and what to do with it.
///
/// Shown once and never again: the platform keeps a hash of it, so a dialog
/// that could be reopened would be a dialog that lies. That is also why this is
/// a panel somebody has to dismiss rather than a toast — a credential that
/// scrolls away while an operator is looking at the board is a node that has to
/// be deleted and made again.
///
/// The command is here rather than in a document because this is the moment it
/// is needed: the token is on the screen, the node id is known, and the URL is
/// the one this console is talking to.
function showNodeToken(id, token) {
  const line =
    "sudo velstra-cloud-node setup   # node id: " + id + ", control plane: " + location.origin;
  // Its own dialog, opened after the form's has closed — the form closes on a
  // successful submit, so filling that one would put a credential into a panel
  // that is about to be removed. The scrim has no `onclick`: a mis-click
  // outside must not be how a once-only token disappears.
  const scrim = el("div", { id: "dialogscrim" });
  const dialog = el("div", { id: "dialog", role: "dialog", "aria-label": "Registration token" });
  document.body.appendChild(scrim);
  document.body.appendChild(dialog);
  fill(dialog,
    el("h2", "Registration token for " + id),
    el("p.prose",
      "Shown once. The platform keeps a hash of it and cannot show it again — " +
      "if it is lost, delete this node and add it back."),
    el("pre.logblock", { id: "nodetoken" }, token),
    el("p.prose", "On the machine, with the token to hand:"),
    el("pre.logblock", line),
    el("p.prose",
      "The wizard asks which cell and as what — control plane, hypervisor, pool, " +
      "or several. Whether this machine carries external traffic is not its own " +
      "answer: set it here, on the node, once it has registered."),
    el("div.formacts",
      el("button.btn", { type: "button", id: "copytoken",
        onclick: () => navigator.clipboard && navigator.clipboard.writeText(token) },
        "Copy the token"),
      el("button.btn.primary", { type: "button", id: "tokendone",
        onclick: () => closeDialog() }, "I have written it down")));
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
    // Answered once, when the object was made. Offering a control here would
    // be offering an edit whose only outcome is a refusal — or, before the API
    // refused it, something worse: changing a volume's pool was accepted and
    // moved no bytes, and the volume quietly stopped converging because one
    // agent had let go and the other would not take it.
    locked: coll.fields.filter((f) => f.atCreation).map((f) => f.key),
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

// ---- setting a password -----------------------------------------------------

/// Ask for a new password, twice, and put it.
///
/// Deliberately **not** built from the schema form. That form writes `spec`, and
/// a password is not on a spec — it lives in a collection the API never serves,
/// which is the whole reason it cannot leak through a listing. A dialog that
/// went through the same path would be one refactor away from putting it there.
///
/// Two fields rather than one: a password nobody can read back cannot be
/// corrected later, so a typo here is an account its owner cannot sign into and
/// an operator cannot diagnose.
///
/// Changing your **own** password asks for the current one as well, because the
/// API refuses a self-change that does not carry it: a stolen session that could
/// set a new password with no proof of the old one would be a permanent account
/// takeover. An operator resetting *someone else's* is the deliberate exception
/// — the whole point of a reset is that nobody has the old password — so that
/// field is only there when it is your own account.
function openPasswordDialog(user) {
  const own = !!(session.who && session.who.subject === user);
  // No scrim `onclick`: a mis-click outside must not discard a half-typed
  // password. Escape still leaves (handled once, globally, in app.js), and
  // Cancel is right there.
  const scrim = el("div", { id: "dialogscrim" });
  const dialog = el("div", { id: "dialog", role: "dialog", "aria-label": "Set password" });
  const problems = el("p.err.hidden", { role: "alert" });

  const current = el("input", {
    type: "password", id: "currentpassword", autocomplete: "current-password", required: "",
  });
  const first = el("input", {
    type: "password", id: "newpassword", autocomplete: "new-password", required: "",
  });
  const again = el("input", {
    type: "password", id: "newpasswordagain", autocomplete: "new-password", required: "",
  });

  dialog.appendChild(el("h2", "Set password"));
  dialog.appendChild(el("p.prose", own
    ? "Changing your own password ends your other sessions, not this one. Confirm "
      + "the current password to make the change."
    : "For " + user + ". Every session this account holds ends when the password "
      + "changes, which is the point of changing it."));
  dialog.appendChild(el("div", { style: "height:var(--space-6)" }));
  dialog.appendChild(el("div.fields",
    own ? el("div.field", el("label", { for: "currentpassword" }, "Current password"), current) : null,
    el("div.field", el("label", { for: "newpassword" }, "New password"), first),
    el("div.field", el("label", { for: "newpasswordagain" }, "Again"), again)));
  dialog.appendChild(problems);

  const stop = (message) => {
    fill(problems, message);
    problems.classList.remove("hidden");
  };

  const submit = el("button.btn.primary", { type: "button", id: "submitpassword" }, "Set password");
  submit.addEventListener("click", async () => {
    if (own && !current.value) return stop("Enter your current password to confirm the change.");
    if (first.value !== again.value) return stop("The two entries do not match.");
    problems.classList.add("hidden");
    submit.setAttribute("disabled", "");
    try {
      // The current password rides along only for a self-change — that is the
      // one the API demands it for, and the one where the field exists.
      const body = own
        ? { currentPassword: current.value, password: first.value }
        : { password: first.value };
      await request("PUT", "/api/v1/users/" + encodeURIComponent(user) + "/password", { body });
      // Cleared before the dialog closes rather than left for the removal to
      // take with it: the node lives until the next frame either way.
      current.value = ""; first.value = ""; again.value = "";
      closeDialog();
      toast(own ? "Your password was changed." : "Password set for " + user + ".");
    } catch (e) {
      submit.removeAttribute("disabled");
      // The API's own words — a wrong current password, or a length rule that is
      // public anyway. Shown in the dialog rather than swallowed, so the operator
      // learns why and can try again.
      stop(e.message);
    }
  });

  dialog.appendChild(el("div.dialogacts",
    el("span.grow"),
    el("button.btn", { type: "button", id: "cancelpassword", onclick: closeDialog }, "Cancel"),
    submit));

  document.body.appendChild(scrim);
  document.body.appendChild(dialog);
  (own ? current : first).focus();
}
