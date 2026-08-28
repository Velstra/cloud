// Reading a resource, and the one question the console exists to answer.

/// The wire spells keys as the contract does. Read the other spelling too, so a
/// console that is right about the model is not wrong about a serialiser
/// setting — and so a field that is genuinely absent still reads as absent
/// rather than as a rendering bug somebody has to go and diagnose.
function pick(obj, key) {
  if (obj === null || obj === undefined) return undefined;
  if (key in obj) return obj[key];
  const snake = key.replace(/[A-Z]/g, (c) => "_" + c.toLowerCase());
  return obj[snake];
}

/// `at(r, "status.addresses.0")`.
function at(obj, path) {
  let here = obj;
  for (const seg of String(path).split(".")) {
    if (here === null || here === undefined) return undefined;
    here = Array.isArray(here) ? here[Number(seg)] : pick(here, seg);
  }
  return here;
}

const meta = (r) => r.meta || {};
const spec = (r) => r.spec || {};
const status = (r) => r.status || {};
const generation = (r) => Number(pick(meta(r), "generation") || 0);
const observed = (r) => Number(pick(status(r), "observedGeneration") || 0);
const revision = (r) => {
  const v = pick(meta(r), "revision");
  return v === undefined || v === null ? null : String(v);
};
const nameOf = (r) => {
  const n = pick(meta(r), "name");
  // A name that arrives parsed rather than flat still has to render.
  if (n && typeof n === "object" && Array.isArray(n.segments)) return n.segments.join("/");
  return String(n || "");
};
const idOf = (r) => nameOf(r).split("/").pop();
const deletedAt = (r) => pick(meta(r), "deletedAt");

function condition(r, kind) {
  const cs = pick(status(r), "conditions") || [];
  return cs.find((c) => c.kind === kind) || null;
}

/// Is it converged, and if not, why.
///
/// Everything the console shows about state comes from here, so there is one
/// answer rather than a list view and a detail view that can disagree. The
/// order of the tests is the meaning: a deleting object is deleting whatever
/// else is true of it, and an object whose spec has moved is drifting even if
/// the last condition it carries says Ready — that condition was written about
/// an older ask.
///
/// `kind` is the condition the collection is judged by, from the schema. Almost
/// everything answers `Ready`; a migration answers `Moved`, because "ready" is
/// not a thing a migration ever is. It is one function reading one condition
/// either way — the vocabulary of the verdict does not grow with the platform.
/// The sentence a person needs, which is not always on the condition being
/// judged.
///
/// A guest whose node is still fetching its image reports `Ready=False` with
/// "wanted Running, the node reports Stopped" — a restatement of the two
/// fields, not a reason — while the *reason* sits on a second condition the
/// agent writes: "copying images/sha256-… from /var/lib/velstra/src…".
///
/// Watching somebody create their first guest, that is the whole difference
/// between a wall and a progress report. So the work in flight is appended
/// where there is any.
function because(r, sentence) {
  const work = condition(r, "HostActions");
  const doing = work && pick(work, "message");
  return doing ? sentence + " — " + doing : sentence;
}

function verdict(r, kind) {
  const gen = generation(r), obs = observed(r);

  // Nothing reports on some things, and saying "not reported" about them is
  // how an attention list fills with objects nobody can act on. An audit
  // record, a usage reading, a user account are *records*: no agent will ever
  // write a condition on one, so `observedGeneration` stays at zero for ever.
  //
  // The schema says which, with an empty `condition`. Measured on a real cell
  // before it did: a hundred and nine objects on the attention list, three of
  // them actually wrong, and the three unfindable.
  if (kind === "") {
    return {
      kind: "settled",
      word: "Recorded",
      why: "A record of something that happened. Nothing reports on it, and nothing will.",
      since: null,
    };
  }
  // An operation says whether it is finished, and that is the answer — not the
  // condition beside it. `status.done` is computed from the target on every
  // read, so an operation that is done is done however it ended: the change
  // landed, or the thing it was about is gone. Either way there is nothing
  // left to wait for and nothing anybody can do to the operation itself.
  //
  // Judging one by `Ready` instead put eighty-three finished operations on the
  // attention list of a real cell, every one of them saying "the object I was
  // about no longer exists" — which is true, and is a fact about a delete
  // somebody did on purpose. If the target is genuinely broken, the target is
  // on the list; the receipt for the request is not a second copy of it.
  const finished = pick(status(r), "done");
  if (finished === true) {
    const failed = pick(status(r), "error");
    return {
      kind: "settled",
      word: "Finished",
      why: failed || "The change this was a receipt for has landed.",
      since: null,
    };
  }
  const named = kind || "Ready";
  const ready = condition(r, named);
  const gone = deletedAt(r);
  if (gone) {
    const finalizers = pick(meta(r), "finalizers") || [];
    return {
      kind: "deleting",
      word: "Deleting",
      why: finalizers.length
        ? "Requested. It stays until " + finalizers.join(", ") + " lets go."
        : "Requested, and nothing is holding it. It goes on the next pass.",
      since: gone,
    };
  }
  // A negative answer about *this* ask is authoritative, whoever wrote it and
  // whatever the owning agent has reported.
  //
  // `status.observedGeneration` says whether the owning agent has caught up. A
  // scheduler that cannot place an instance writes `Ready=False` against the
  // current generation and no agent ever sees the object at all — so it sits at
  // `observedGeneration: 0` with the reason already on it. Asking about the
  // agent first calls that "not reported", which reads as "still waiting" about
  // an object nothing further will happen to, and buries the one sentence
  // explaining why. A positive condition is not treated this way: `Ready=True`
  // is a claim about the world matching, and only the agent can report that.
  const decided = ready && ready.status === "False" &&
    Number(pick(ready, "observedGeneration") || 0) === gen;
  if (decided) {
    return {
      kind: "failing", word: "Failing",
      why: because(r, ready.message || "The " + named + " condition is false and says nothing more."),
      since: pick(ready, "lastTransition"), ready,
    };
  }
  // Observed generation zero is not "behind by one". Nobody has looked at this
  // object at all, which is a different thing to say and a different thing to
  // do about it.
  if (obs === 0) {
    return {
      kind: "unreported",
      word: "Not reported",
      why: "Asked for at generation " + gen + ". Nothing has reported on it yet.",
      since: ready ? pick(ready, "lastTransition") : null,
      ready,
    };
  }
  if (obs < gen) {
    return {
      kind: "drifting",
      word: "Drifting",
      why: "The ask moved to generation " + gen + "; the world is reported at " + obs + ".",
      since: ready ? pick(ready, "lastTransition") : null,
      ready,
    };
  }
  if (!ready) {
    return {
      kind: "unreported",
      word: "Not reported",
      why: "Nothing has written a " + named + " condition for generation " + gen + " yet.",
      since: null,
    };
  }
  // The word stays one of five, always. The reason and the sentence the agent
  // wrote are shown beside it — putting a machine token where the verdict goes
  // would give the page a vocabulary that grows every time a controller learns
  // a new way to fail.
  if (ready.status === "False") {
    return {
      kind: "failing", word: "Failing",
      why: because(r, ready.message || "The " + named + " condition is false and says nothing more."),
      since: pick(ready, "lastTransition"), ready,
    };
  }
  if (ready.status === "Unknown") {
    return {
      kind: "unreported", word: "Not reported",
      why: ready.message || "The owning agent has not reported on generation " + gen + ".",
      since: pick(ready, "lastTransition"), ready,
    };
  }
  return {
    kind: "settled", word: "Settled",
    why: "The world matches generation " + gen + ".",
    since: pick(ready, "lastTransition"), ready,
  };
}

/// A condition recorded against an older generation is visibly stale rather
/// than quietly wrong.
function conditionStale(r, c) {
  return Number(pick(c, "observedGeneration") || 0) < generation(r);
}

/// The mark: shape says whether anything was observed, colour says what, and
/// the word is always beside it.
function mark(kind) { return el("span.mark." + kind); }

function stateOf(r, kind) {
  const v = verdict(r, kind);
  return el("span.state." + v.kind, mark(v.kind), v.word);
}

// ---- formatting ------------------------------------------------------------

const ACRONYMS = {
  vcpus: "vCPUs", mib: "MiB", gib: "GiB", mtu: "MTU", vni: "VNI", mac: "MAC",
  dns: "DNS", cidr: "CIDR", vmm: "VMM", numa: "NUMA", ssh: "SSH", uid: "UID",
  id: "ID", url: "URL", ip: "IP", pid: "PID", os: "OS", cpu: "CPU",
};

/// Keys whose shape survives the mechanical translation but whose meaning does
/// not. Kept short on purpose: it is a list of exceptions, not a second schema.
const LABELS = {
  // Named so the pair reads as what it is: a tail, and how much there was.
  // "Console output" beside a byte count is how somebody works out that the
  // panic they are looking for scrolled off an hour ago.
  consoleTail: "Console output",
  consoleBytes: "Console written",
  console: "Show console output",
  // `hugepages1gi`, all lowercase, because that is what the wire says: the
  // field is `hugepages_1gi` and a digit cannot be capitalised. Spelled
  // `hugepages1Gi` here, this row silently showed nothing.
  hugepages1gi: "1 GiB hugepages",
  hugepages1gi: "1 GiB hugepages",
  numaFreeMib: "Free per NUMA node",
  vmmPid: "VMM process",
  observedGeneration: "Observed at generation",
  // Acronyms the humaniser cannot know: it title-cases each word, which turns
  // an initialism into a word ("Vip") and reads as a mistake.
  vip: "Address",
  memberPort: "Member port",
  cidr: "CIDR",
  mac: "MAC",
  dns: "DNS",
  fsid: "FSID",
  osd: "OSD",
  rd: "RD",
};

const label = (key) => LABELS[key] || humanise(key);

/// `rootDiskGib` → `Root disk GiB`. Used where the console renders a status
/// object it was never told about — a field an agent starts reporting shows up
/// with a readable name instead of waiting for a console release.
function humanise(key) {
  const words = String(key)
    .replace(/([a-z0-9])([A-Z])/g, "$1 $2")
    .replace(/_/g, " ")
    .toLowerCase()
    .split(" ")
    .filter(Boolean)
    .map((w) => ACRONYMS[w] || w);
  if (!words.length) return key;
  const first = words[0];
  return (ACRONYMS[first.toLowerCase()] ? first : first[0].toUpperCase() + first.slice(1)) +
    (words.length > 1 ? " " + words.slice(1).join(" ") : "");
}

function bytes(n) {
  const v = Number(n);
  if (!isFinite(v) || v <= 0) return "0";
  const units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
  let i = 0, x = v;
  while (x >= 1024 && i < units.length - 1) { x /= 1024; i++; }
  return (x < 10 && i > 0 ? x.toFixed(1) : Math.round(x)) + " " + units[i];
}

function mibAlso(n) {
  const v = Number(n);
  if (!isFinite(v) || v < 1024) return "";
  return (v / 1024).toFixed(v % 1024 ? 1 : 0) + " GiB";
}

function ago(ms) {
  const t = Number(ms);
  if (!isFinite(t) || t <= 0) return "never";
  let s = Math.max(0, Math.round((Date.now() - t) / 1000));
  if (s < 60) return s + "s ago";
  if (s < 3600) return Math.round(s / 60) + "m ago";
  if (s < 86400) return Math.round(s / 3600) + "h ago";
  return Math.round(s / 86400) + "d ago";
}

function stamp(ms) {
  const t = Number(ms);
  if (!isFinite(t) || t <= 0) return "";
  return new Date(t).toISOString().replace("T", " ").replace(/\.\d+Z$/, "Z");
}

/// A resource name is long and its tail is what identifies it. The tail is what
/// is shown; the whole name is on the element for a pointer and a screen
/// reader, so nothing is actually hidden.
function shortName(value) {
  const s = String(value ?? "");
  if (!s.includes("/")) return s;
  return s.split("/").slice(-2).join("/");
}
