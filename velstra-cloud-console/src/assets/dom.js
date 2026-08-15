// Building blocks. Nothing here knows what a resource is.
//
// Elements are built, never assembled as a string of HTML. Every value on this
// page came off the wire — a condition message written by an agent, a label an
// operator typed — and `innerHTML` over any of it is markup injection with
// extra steps. `el()` sets `textContent`, so there is no path from the API to
// the parser.

const $ = (id) => document.getElementById(id);

/// `el("td.name", "i1")`, `el("button.btn.primary", {onclick}, "Create")`.
function el(spec, ...rest) {
  const [tag, ...classes] = spec.split(".");
  const node = document.createElement(tag || "div");
  if (classes.length) node.className = classes.join(" ");
  for (const item of rest) {
    if (item === null || item === undefined || item === false) continue;
    if (item instanceof Node) { node.appendChild(item); continue; }
    if (Array.isArray(item)) { for (const c of item) if (c) node.appendChild(c); continue; }
    if (typeof item === "object") {
      for (const [k, v] of Object.entries(item)) {
        if (v === null || v === undefined) continue;
        if (k.startsWith("on")) node.addEventListener(k.slice(2), v);
        else if (k === "text") node.textContent = String(v);
        else if (k === "html") throw new Error("no");   // see the note above
        else node.setAttribute(k, String(v));
      }
      continue;
    }
    node.appendChild(document.createTextNode(String(item)));
  }
  return node;
}

function clear(node) { while (node.firstChild) node.removeChild(node.firstChild); }

function fill(node, ...children) {
  clear(node);
  for (const c of children.flat()) {
    if (c === null || c === undefined || c === false) continue;
    node.appendChild(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return node;
}

/// A label in the margin column and its body beside it — the page's one gutter.
function spread(label, body, sub) {
  return el("div.spread",
    el("div.margin", label, sub ? el("span.sub", sub) : null),
    body);
}

/// Say something once, briefly. Not a notification centre: the object itself is
/// where the truth lives, and a toast that has to be read twice is a dialog.
let toastTimer = null;
function toast(message, tone) {
  let box = $("toast");
  if (!box) { box = el("div", { id: "toast" }); document.body.appendChild(box); }
  fill(box, el("span" + (tone === "bad" ? ".err" : ""), message));
  box.classList.remove("hidden");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => box.classList.add("hidden"), 6000);
}
