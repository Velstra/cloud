// Moving a guest, and watching it happen.
//
// There is no `MIGRATING` state on this page because there is none in the
// platform, and inventing one here would be the same bug at one remove: a
// screen that says "migrating" about a guest whose migration died is a screen
// somebody has to go behind. What exists is a migration object and an instance
// whose node is a fact, so that is what is shown — and when the source has let
// go and the destination has not claimed yet, the page says exactly that
// instead of covering the gap with a spinner.
//
// The destination is chosen where the guest is. `:explainMigration` answers,
// before anything is created, which nodes could receive this particular guest;
// what cannot receive it is shown with the sentence saying why rather than
// quietly left out, and cannot be clicked.

/// What each mode costs, in the only two moments an operator needs it: before
/// starting one, and before abandoning one. The three sentences per mode are the
/// whole reason `mode` is not a detail behind "More settings".
const MIGRATION_MODES = {
  Live: {
    label: "Live",
    grave: false,
    risk: (f) =>
      "Pre-copy: memory is copied while the guest keeps running on " + f.from +
      ", and it pauses only for the last dirty pages. A failure is free — the guest stays on " +
      f.from + ", and nothing is lost but the time spent copying.",
    abandon: (f) =>
      "Abandon this migration? The guest keeps running on " + f.from +
      ". The receiver on " + f.to + " is torn down and the memory copied so far is thrown away.",
  },
  PostCopy: {
    label: "Post-copy",
    grave: true,
    risk: (f) =>
      "Post-copy: " + f.to + " resumes the guest first and faults its memory in from " + f.from +
      " on demand. Lower total downtime, and a failure mid-flight loses the guest — once " + f.to +
      " has resumed it, neither node has all of its memory.",
    abandon: (f) =>
      "Abandon this post-copy migration? This is not safe. If " + f.to +
      " has already resumed the guest, its memory is split across both nodes and stopping the transfer loses it. " +
      "Only abandon a post-copy migration that has not started sending.",
  },
  Reboot: {
    label: "Reboot",
    grave: false,
    risk: (f) =>
      "Stop, move, start: the guest is off from the moment " + f.from + " stops it until " + f.to +
      " has started it again. Nothing crosses a wire, so this is the move that works between " +
      "machines that are not alike — a different processor, or a guest holding hardware. The " +
      "outage is the price, and it is the length of a boot.",
    // No receiver is opened for a cold move, so there is none to tear down.
    // Saying otherwise sent somebody looking for one.
    abandon: (f) =>
      "Abandon this migration? The guest stays on " + f.from + ". Nothing was opened on " + f.to +
      " to tear down. If it was already stopped for the move, it is the instance's own desired " +
      "state that brings it back up on " + f.from + ".",
  },
};

/// Everything the screens below read off a migration, in one place, so the
/// dialog and the sheet cannot describe the same object differently.
function migrationFacts(r) {
  const sp = spec(r), st = status(r);
  const node = (key, fallback) => {
    const v = pick(sp, key);
    return v === null || v === undefined || v === "" ? fallback : String(v);
  };
  return {
    instance: String(pick(sp, "instance") || ""),
    from: node("fromNode", "the node it is on"),
    to: node("toNode", "the destination"),
    mode: String(pick(sp, "mode") || ""),
    ready: pick(st, "receiverReady") === true,
    url: pick(st, "receiverUrl") || "",
    copied: Number(pick(st, "transferredMib") || 0),
  };
}

const migrationArrived = (r) => {
  const c = condition(r, "Moved");
  return !!c && (c.status === "True" || c.reason === "Arrived");
};

/// The `Moved` condition, said in words.
///
/// The order is the model's own: arrived beats everything, a refusal beats a
/// report about progress, and a receiver that is not listening beats a reason
/// written before it stopped — because "the destination is not listening" is a
/// fact about now and a condition is a summary of a moment that has passed.
function movedWords(r, f) {
  const c = condition(r, "Moved");
  const reason = c ? c.reason : "";
  if (migrationArrived(r)) {
    return {
      kind: "settled", word: "Arrived",
      why: "The guest is running on " + f.to +
        ". Nothing is left to do; deleting this migration only removes the record.",
    };
  }
  if (c && c.status === "False") {
    // Verbatim, and especially here. The sentence behind `Timeout` names the
    // one thing an operator needs at that moment — which node has the guest
    // now, or that the handover was interrupted and none does. Nothing this
    // console could compose would be more use than the platform's own words,
    // and a paraphrase could be wrong about where the guest is.
    return {
      kind: "failing",
      word: reason === "NoSuchInstance" ? "No such instance"
        : reason === "Timeout" ? "Gave up" : "Refused",
      why: c.message || (reason === "Timeout"
        ? "It ran out of time, and says nothing about where the guest is."
        : "The Moved condition is false and says nothing more."),
    };
  }
  if (!f.ready) {
    return {
      kind: "unreported", word: "Not listening yet",
      why: f.to + " has not started receiving. Nothing has been sent, and the guest is still running on " +
        f.from + ".",
    };
  }
  if (reason === "HandingOver") {
    return {
      kind: "drifting", word: "Handing over",
      why: "The source has let go and " + f.to +
        " has not claimed the guest yet. It is running on neither node at this moment.",
    };
  }
  if (reason === "Transferring" || f.copied > 0) {
    return {
      kind: "drifting", word: "Copying memory",
      why: f.copied.toLocaleString() + " MiB copied to " + f.to +
        " so far. That is what the source last reported, not a fraction of a total: a guest that keeps " +
        "dirtying pages is copied more than once, and nothing here promises when it ends.",
    };
  }
  return {
    kind: "unreported", word: "Listening",
    why: f.to + " is receiving, and nothing has been reported about the transfer itself yet.",
  };
}

/// The migration sheet's own block: what is happening, from what the two agents
/// have reported, and what this mode costs if it goes wrong.
function movementBlock(r) {
  const f = migrationFacts(r);
  const w = movedWords(r, f);
  const mode = MIGRATION_MODES[f.mode];
  const box = el("div.verdict." + w.kind,
    el("div.head", mark(w.kind), w.word),
    el("div.why.muted", w.why));

  // One age, for one reason, because exactly one of them has an anchor.
  //
  // `Moved` is computed when the object is read, so in general its
  // `lastTransition` is the moment of *the read* — an age from it would say
  // "just now" over a transfer that died an hour ago, which is the worst thing
  // this screen could say. `Timeout` is the exception and not by accident: it
  // happened at `createdAt + timeoutS`, which is arithmetic rather than a
  // guess, so the API stamps that moment and two reads agree on it. Hence the
  // test on the reason rather than trust in the field.
  //
  // Nothing timestamps `transferredMib`, so there is no age beside the copied
  // number. That it may stall is said in words instead.
  const moved = condition(r, "Moved");
  const gaveUp = moved && moved.reason === "Timeout" ? pick(moved, "lastTransition") : null;

  box.appendChild(el("div.gens",
    el("div", el("span.k", "Guest"), shortName(f.instance)),
    el("div", el("span.k", "From"), f.from),
    el("div", el("span.k", "To"), f.to),
    // A count, never a percentage: the platform promises no total and no
    // deadline, and a bar that fills to 90% and stays there for an hour is a
    // promise the page had no right to make.
    el("div", el("span.k", "Copied"), f.copied.toLocaleString() + " MiB"),
    el("div", el("span.k", "Receiver"), f.ready ? "listening" : "not listening"),
    gaveUp
      ? el("div", el("span.k", "Gave up"),
        el("span", { title: stamp(gaveUp) }, ago(gaveUp)))
      : null));

  if (f.url) {
    box.appendChild(el("div.why", el("span.faint", "Receiving on "), el("span.mono", String(f.url))));
  }
  box.appendChild(mode
    ? el("div.why" + (mode.grave ? ".err" : ".muted"), mode.risk(f))
    : el("div.why.muted", f.mode
      ? "This migration names a mode this console does not know (" + f.mode +
        "), so what a failure would cost cannot be said here."
      : "This migration names no mode, so what a failure would cost cannot be said here."));
  return box;
}

/// What abandoning this particular migration means. Genuinely different per
/// mode, which is why it is a sentence and not a "are you sure?".
function abandonAsk(r) {
  const f = migrationFacts(r);
  const mode = MIGRATION_MODES[f.mode];
  if (migrationArrived(r)) {
    return {
      verb: "Remove",
      warning: "The guest has arrived on " + f.to +
        ". Removing this migration only removes the record — nothing about the guest changes.",
      done: "Removed. The guest stays where it arrived.",
    };
  }
  const said = mode
    ? mode.abandon(f)
    : "Abandon this migration? It names a mode this console does not know" +
      (f.mode ? " (" + f.mode + ")" : "") +
      ", so what abandoning costs cannot be said here.";
  // A migration that gave up already says where the guest is, in the
  // platform's words. That sentence goes first, because it is what decides
  // whether abandoning this one is safe.
  const c = condition(r, "Moved");
  const timedOut = c && c.reason === "Timeout" && c.message;
  return {
    verb: "Abandon",
    grave: !!(mode && mode.grave),
    warning: timedOut ? c.message + " " + said : said,
    done: "Abandoned. The receiver on " + f.to +
      " is torn down; nothing else about the guest was asked to change.",
  };
}

// ---- on the instance -------------------------------------------------------

/// Where this guest is, and where it is going — read from the migrations that
/// exist, never from a field on the instance, because the instance has no such
/// field and must not grow one.
async function migrationInto(host, r) {
  const where = whereItIs(r);
  const coll = collection("migrations");
  fill(host, where, coll ? el("p.faint", "Looking for migrations…") : null);
  if (!coll) return;

  let mine = [];
  try {
    const all = await list(coll);
    mine = all.items.filter((m) => String(pick(spec(m), "instance") || "") === nameOf(r));
  } catch (e) {
    fill(host, where, el("p.faint", "Migrations could not be read: " + e.message));
    return;
  }

  const live = mine.filter((m) => !deletedAt(m) && !migrationArrived(m));
  const done = mine.filter((m) => !deletedAt(m) && migrationArrived(m));
  const on = at(status(r), "node");

  fill(host, where,
    live.map((m) => migrationLine(m)),
    done.map((m) => migrationLine(m)),
    live.length
      // "Open", not "in flight": one that gave up is finished and still here,
      // and the way on from it is to abandon it. Starting another is starting
      // another — there is no retry, because a new migration is a new
      // migration.
      ? el("p.faint", "A migration for this guest is still open. Abandon it before starting another.")
      : on
        // Not the primary: blue is the one action a screen is *for*, and this
        // screen is for reading the guest. Moving it is deliberate, not the
        // default thing to do here.
        ? el("div", { style: "margin-top:var(--space-4)" },
          el("button.btn", { type: "button", id: "migratebtn", onclick: () => openMigrate(r) },
            "Migrate…"))
        : el("p.faint", "No node has this guest, so there is nothing to move."));
}

/// The one fact, said plainly — including the honest version of the gap in the
/// middle of a migration, where nobody has it.
function whereItIs(r) {
  const on = at(status(r), "node");
  const assigned = at(spec(r), "node");
  if (on) {
    return el("p", el("span.mono", String(on)), el("span.muted", " has this guest."));
  }
  if (assigned) {
    return el("p",
      el("span.muted", "No node has this guest right now. "),
      el("span.mono", String(assigned)),
      el("span.muted", " is assigned it and has not claimed it yet."));
  }
  return el("p.muted", "No node has this guest, and none is assigned to it.");
}

function migrationLine(m) {
  const f = migrationFacts(m);
  const w = movedWords(m, f);
  return el("div.movement",
    el("div",
      el("span.state." + w.kind, mark(w.kind), w.word),
      el("span.muted", " · to "), el("span.mono", f.to),
      el("span.muted", " · " + (MIGRATION_MODES[f.mode] ? MIGRATION_MODES[f.mode].label : f.mode) + " · "),
      el("button.link.mono", { type: "button", title: nameOf(m), onclick: () => goTo(nameOf(m)) }, idOf(m))),
    el("div.why.muted", w.why));
}

// ---- starting one ----------------------------------------------------------

/// Ask for this guest to run somewhere else.
///
/// Opened from the instance, and the instance is locked: every answer
/// `:explainMigration` gave is about *this* guest, and a dialog that let the
/// guest be swapped underneath them would be offering destinations vouched for
/// against a different one.
function openMigrate(r) {
  const migrations = collection("migrations");
  const instances = collection("instances");
  if (!migrations || !instances) return null;
  const from = at(status(r), "node") || at(spec(r), "node") || "";
  return openCreate(migrations, {
    title: "Migrate " + idOf(r),
    blurb: "Ask that this guest run on another node. Nothing about the instance changes when this is " +
      "created: the destination starts a receiver, the source sends, and the instance moves only once the " +
      "source has reported that it no longer has the guest.",
    // Suggested so two migrations of the same guest do not collide, and
    // editable because it is only a name.
    id: idOf(r) + "-" + Math.random().toString(36).slice(2, 6),
    values: {
      instance: nameOf(r),
      fromNode: from,
      mode: "Live",
      downtimeMs: 300,
      timeoutS: 3600,
      connections: 1,
    },
    locked: ["instance"],
    // The form is handed in: the answer depends on the mode chosen above, and
    // it is asked again whenever that changes.
    candidates: { toNode: (form) => askCandidates(instances, r, from, form) },
    submitLabel: "Migrate",
  });
}

/// What the platform says about each node, for this guest.
async function askCandidates(instances, r, from, form) {
  const nodes = await options("nodes").catch(() => []);
  const mode = (form && form.values && form.values.mode) || "Live";
  try {
    return readCandidates(await explainMigration(instances, idOf(r), mode), nodes);
  } catch (e) {
    // The platform could not answer. Refusing to migrate at all because the
    // explanation is missing would be worse than letting the API refuse the
    // request — which it does, by field, with a sentence that lands on this
    // control. What is not done is pretending the nodes were vouched for.
    return {
      trouble: "The platform could not say which nodes can receive this guest: " + e.message +
        " Every node is offered; one that cannot take it will be refused here, with the reason.",
      candidates: nodes.map((n) => idOf(n)).map((id) => id === from
        ? { id, ok: false, why: "AlreadyThere", detail: "it is already there" }
        : { id, ok: true, detail: nodeNote(id, nodes) }),
    };
  }
}

/// The answer: every node in the cell, each carrying its own verdict.
///
/// `destinations` is the whole set — a node that is not in it does not exist,
/// rather than being undecided — so nothing here infers that a node can take
/// the guest from the absence of a refusal. That inference is what
/// `:explainPlacement` would have invited, and it is exactly backwards: that
/// verb answers with the one node the scheduler picked and throws the rest
/// away, so a candidate set cannot be recovered from it at all. Migration is
/// answered per destination, so it can be read as one.
function readCandidates(answer, nodes) {
  const a = answer || {};
  const said = Array.isArray(a.destinations) ? a.destinations : null;
  if (!said) {
    // An answer this console cannot read is not an answer. It is *not* treated
    // as "everything is fine": nothing here may vouch for a node the platform
    // did not vouch for, so the offer is made with the reason it is unbacked,
    // and the API's own refusal lands on the control.
    return {
      trouble: "The platform answered in a shape this console does not know, so nothing here has " +
        "vouched for any of these nodes. One that cannot take the guest will be refused, with the reason.",
      candidates: (nodes || []).map((n) => ({ id: idOf(n), ok: true, detail: nodeNote(idOf(n), nodes) })),
    };
  }
  const candidates = said.map((d) => {
    const id = String(d.node ?? "");
    const ok = d.allowed === true;
    return ok
      ? { id, ok, detail: nodeNote(id, nodes) }
      : { id, ok, why: d.why || "Refused", detail: d.detail || "" };
  }).filter((c) => c.id);
  // What can be chosen first, and each half in a stable order: a list that
  // reorders itself between two looks is a list nobody trusts.
  candidates.sort((x, y) => (x.ok === y.ok ? String(x.id).localeCompare(String(y.id)) : x.ok ? -1 : 1));
  return { candidates };
}

/// The same second reading the node picker gives everywhere else.
function nodeNote(id, nodes) {
  const found = (nodes || []).find((n) => idOf(n) === id);
  return found ? optionNote("nodes", found).trim() : "";
}
