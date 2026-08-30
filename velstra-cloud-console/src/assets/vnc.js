// The guest's screen, as its own page.
//
// Not a panel on the sheet. A framebuffer at sheet width is a postage stamp —
// found by putting it there — and unlike the serial console, whose last lines
// are useful at any size, a screen someone cannot read is not a smaller
// version of the feature. So the sheet carries a button, and the button
// navigates: same tab, full content area. A new *window* was considered and
// rejected for a plain reason: the session token lives in `sessionStorage`,
// which is per-tab, so a new window would open onto the sign-in page.
//
// What speaks RFB here is this file, by hand, and that is a decision rather
// than a shortcut missed. The console is one hand-written script with no
// dependencies — the whole page is auditable — and vendoring a client library
// would end that for the one screen that carries keystrokes into machines.
// The subset spoken is deliberately small: Raw and CopyRect encodings, the
// DesktopSize resize, no compression. On the LAN between a browser and a cell
// that is entirely adequate, and every byte of it is readable below.

/// RFB keysyms for the keys a browser names by word rather than by character.
/// Printable characters are their own codepoint (RFB inherited X11's rule that
/// Latin-1 maps straight through, and `keysym = 0x01000000 + codepoint` covers
/// the rest).
const VNC_KEYS = {
  Backspace: 0xff08, Tab: 0xff09, Enter: 0xff0d, Escape: 0xff1b,
  Insert: 0xff63, Delete: 0xffff, Home: 0xff50, End: 0xff57,
  PageUp: 0xff55, PageDown: 0xff56,
  ArrowLeft: 0xff51, ArrowUp: 0xff52, ArrowRight: 0xff53, ArrowDown: 0xff54,
  Shift: 0xffe1, Control: 0xffe3, Alt: 0xffe9, Meta: 0xffe7,
  CapsLock: 0xffe5, ContextMenu: 0xff67,
  F1: 0xffbe, F2: 0xffbf, F3: 0xffc0, F4: 0xffc1, F5: 0xffc2, F6: 0xffc3,
  F7: 0xffc4, F8: 0xffc5, F9: 0xffc6, F10: 0xffc7, F11: 0xffc8, F12: 0xffc9,
};

function vncKeysym(e) {
  if (e.key.length === 1) {
    const cp = e.key.codePointAt(0);
    return cp < 0x100 ? cp : 0x01000000 + cp;
  }
  return VNC_KEYS[e.key] || null;
}

/// A byte stream reassembled from websocket frames.
///
/// RFB is a stream protocol and a websocket delivers it as messages cut
/// wherever the relay's buffer happened to end, so nothing here may assume a
/// message is a whole anything. The parser asks for N bytes and either gets
/// them or waits.
function byteQueue() {
  const chunks = [];
  let size = 0;
  return {
    push(buf) { chunks.push(new Uint8Array(buf)); size += buf.byteLength; },
    size: () => size,
    take(n) {
      if (size < n) return null;
      const out = new Uint8Array(n);
      let at = 0;
      while (at < n) {
        const head = chunks[0];
        const want = n - at;
        if (head.length <= want) { out.set(head, at); at += head.length; chunks.shift(); }
        else { out.set(head.subarray(0, want), at); chunks[0] = head.subarray(want); at = n; }
      }
      size -= n;
      return out;
    },
  };
}

/// Speak RFB over `ws`, drawing into `canvas`, reporting via `say(text)`.
///
/// Returns a handle with `close()` and the input senders the page wires to DOM
/// events. The pixel format is *told to the server* rather than accepted from
/// it: one SetPixelFormat for little-endian 32bpp BGRX and every rect decodes
/// the same way, instead of this client growing a decoder per server mood.
function rfbClient(ws, canvas, say) {
  const q = byteQueue();
  const ctx = canvas.getContext("2d");
  let phase = "version";
  let closed = false;

  const send = (bytes) => { if (ws.readyState === 1) ws.send(new Uint8Array(bytes)); };
  const u16 = (n) => [(n >> 8) & 0xff, n & 0xff];
  const u32 = (n) => [(n >>> 24) & 0xff, (n >>> 16) & 0xff, (n >>> 8) & 0xff, n & 0xff];

  const requestUpdate = (incremental) => {
    send([3, incremental ? 1 : 0, ...u16(0), ...u16(0), ...u16(canvas.width), ...u16(canvas.height)]);
  };

  // What the parser is in the middle of. Anything read before its body has
  // arrived lives here rather than on the stack, so the next call resumes
  // instead of re-reading — see the note in the `security` phase.
  let rects = 0;
  let rect = null;
  let pending = null;

  const step = () => {
    for (;;) {
      if (closed) return;
      switch (phase) {
        case "version": {
          const b = q.take(12);
          if (!b) return;
          // Whatever it offers, 3.8 is what this client speaks; a server old
          // enough to refuse that is a server this page cannot help with.
          send([...new TextEncoder().encode("RFB 003.008\n")]);
          phase = "security";
          break;
        }
        case "security": {
          // The count is *held* once read, never taken twice: a websocket cuts
          // the stream wherever the relay's buffer ended, and a parser that
          // consumed the count, found the types not yet arrived and returned
          // would read the first type as the next count — one byte of drift
          // that turns every message after it into noise. The same rule holds
          // everywhere below a length precedes its body.
          if (pending === null) {
            if (q.size() < 1) return;
            pending = q.take(1)[0];
            if (pending === 0) { say("the server refused the handshake"); return; }
          }
          const types = q.take(pending);
          if (!types) return;
          pending = null;
          if (![...types].includes(1)) {
            // QEMU on a unix socket offers None — the socket itself is behind
            // the ticket check. A server wanting VNC auth is not this cell's.
            say("the server wants an authentication this client does not speak");
            return;
          }
          send([1]);
          phase = "secresult";
          break;
        }
        case "secresult": {
          const b = q.take(4);
          if (!b) return;
          if (b[3] !== 0 || b[0] !== 0) { say("the server refused the connection"); return; }
          send([1]); // ClientInit: shared
          phase = "serverinit";
          break;
        }
        case "serverinit": {
          if (pending === null) {
            if (q.size() < 24) return;
            pending = q.take(24);
          }
          const head = pending;
          const w = (head[0] << 8) | head[1];
          const h = (head[2] << 8) | head[3];
          const nameLen = (head[20] << 24) | (head[21] << 16) | (head[22] << 8) | head[23];
          if (!q.take(nameLen)) return;
          pending = null;
          canvas.width = w;
          canvas.height = h;
          // One format for everything after this: 32bpp, true colour,
          // little-endian, red at 16 / green at 8 / blue at 0 — BGRX on the
          // wire, swizzled to RGBA below.
          send([0, 0, 0, 0, 32, 24, 0, 1, ...u16(255), ...u16(255), ...u16(255), 16, 8, 0, 0, 0, 0]);
          // Raw (0), CopyRect (1), DesktopSize (-223).
          send([2, 0, ...u16(3), ...u32(1), ...u32(0), ...u32(0xffffff21 >>> 0)]);
          say("");
          requestUpdate(false);
          phase = "messages";
          break;
        }
        case "messages": {
          if (q.size() < 1) return;
          const type = q.take(1)[0];
          if (type === 0) { phase = "update-head"; break; }
          if (type === 1) { phase = "palette"; break; }
          if (type === 2) { break; } // Bell: nothing to ring.
          if (type === 3) { phase = "cut-head"; break; }
          say("the server said something this client does not know (" + type + ")");
          return;
        }
        case "update-head": {
          const b = q.take(3);
          if (!b) return;
          rects = (b[1] << 8) | b[2];
          phase = rects ? "rect-head" : "messages";
          if (!rects) requestUpdate(true);
          break;
        }
        case "rect-head": {
          const b = q.take(12);
          if (!b) return;
          const enc = (b[8] << 24) | (b[9] << 16) | (b[10] << 8) | b[11];
          rect = {
            x: (b[0] << 8) | b[1], y: (b[2] << 8) | b[3],
            w: (b[4] << 8) | b[5], h: (b[6] << 8) | b[7],
            enc,
          };
          if (enc === -223) {
            // DesktopSize: the guest changed its resolution. Setting a
            // canvas's size clears it, and the server believes everything it
            // sent is on screen — an *incremental* request after this is a
            // black screen until the guest next changes a pixel. So the next
            // request is a full one.
            canvas.width = rect.w;
            canvas.height = rect.h;
            rect = null;
            if (--rects === 0) { phase = "messages"; requestUpdate(false); }
            break;
          }
          if (enc === 1) { phase = "copyrect"; break; }
          if (enc !== 0) { say("the server sent an encoding this client did not ask for (" + enc + ")"); return; }
          phase = "raw";
          break;
        }
        case "copyrect": {
          const b = q.take(4);
          if (!b) return;
          const sx = (b[0] << 8) | b[1], sy = (b[2] << 8) | b[3];
          ctx.drawImage(canvas, sx, sy, rect.w, rect.h, rect.x, rect.y, rect.w, rect.h);
          rect = null;
          if (--rects === 0) { phase = "messages"; requestUpdate(true); } else phase = "rect-head";
          break;
        }
        case "raw": {
          const need = rect.w * rect.h * 4;
          const b = q.take(need);
          if (!b) return;
          const img = ctx.createImageData(rect.w, rect.h);
          for (let i = 0; i < need; i += 4) {
            img.data[i] = b[i + 2];      // R (wire is BGRX)
            img.data[i + 1] = b[i + 1];  // G
            img.data[i + 2] = b[i];      // B
            img.data[i + 3] = 255;
          }
          ctx.putImageData(img, rect.x, rect.y);
          rect = null;
          if (--rects === 0) { phase = "messages"; requestUpdate(true); } else phase = "rect-head";
          break;
        }
        case "palette": {
          // SetColourMapEntries: header then 6 bytes per colour. Never asked
          // for (true colour was set), but a server that sends one anyway must
          // not desynchronise the stream.
          if (pending === null) {
            const b = q.take(5);
            if (!b) return;
            pending = ((b[3] << 8) | b[4]) * 6;
          }
          if (!q.take(pending)) return;
          pending = null;
          phase = "messages";
          break;
        }
        case "cut-head": {
          if (pending === null) {
            const b = q.take(7);
            if (!b) return;
            pending = ((b[3] << 24) | (b[4] << 16) | (b[5] << 8) | b[6]) >>> 0;
          }
          if (!q.take(pending)) return;
          pending = null;
          phase = "messages";
          break;
        }
        default:
          return;
      }
    }
  };

  ws.binaryType = "arraybuffer";
  ws.onmessage = (e) => { q.push(e.data); step(); };

  let buttons = 0;
  const pointer = (x, y) => send([5, buttons, ...u16(Math.max(0, x | 0)), ...u16(Math.max(0, y | 0))]);
  return {
    key(down, keysym) { send([4, down ? 1 : 0, 0, 0, ...u32(keysym)]); },
    move(x, y) { pointer(x, y); },
    button(mask, x, y) { buttons = mask; pointer(x, y); },
    close() { closed = true; try { ws.close(); } catch (e) { /* already gone */ } },
  };
}

/// Whatever screen view is open, so every navigation can put it down. A socket
/// left open keeps a session attached against a guest nobody is watching.
let screenOpen = null;
function closeScreen() {
  if (screenOpen) { screenOpen(); screenOpen = null; }
  $("screenbox").classList.add("hidden");
}

/// Send the three-finger salute, held together the way a keyboard holds it.
function saluteWith(client) {
  const seq = [0xffe3, 0xffe9, 0xffff];
  for (const k of seq) client.key(true, k);
  for (const k of [...seq].reverse()) client.key(false, k);
}

/// The screen page for one guest.
async function showScreen(name) {
  const instances = collection("instances");
  if (!instances) return;
  if (view.watcher) { view.watcher.stop(); view.watcher = null; }
  stopRecheck();
  closeScreen();
  view.coll = null;
  view.home = false;
  view.map = false;
  view.items = [];
  view.picked.clear();
  $("picked").classList.add("hidden");
  location.hash = "#screen/" + name;

  const id = shortName(name).split("/").pop();
  $("listtitle").textContent = id;
  fill($("listblurb"), "The guest's display. Click the screen to type into it; " +
    "what you see is what the guest is showing, serial console or not.");
  clear($("listacts"));
  $("listfilter").classList.add("hidden");
  $("listerr").classList.add("hidden");
  $("cpuadvisory").classList.add("hidden");
  $("listempty").classList.add("hidden");
  $("overviewbox").classList.add("hidden");
  $("topologybox").classList.add("hidden");
  document.querySelector(".boardwrap").classList.add("hidden");

  const box = $("screenbox");
  box.classList.remove("hidden");
  clear(box);
  renderRail();

  const status = el("p.muted", "Asking for the screen…");
  const canvas = el("canvas", { class: "vncscreen", tabindex: "0",
    "aria-label": "the guest's display; focus and type" });
  const back = el("button.btn", { type: "button",
    onclick: () => { closeScreen(); show("instances"); } }, "Back to instances");
  const salute = el("button.btn", { type: "button", id: "ctrlaltdel", disabled: "" },
    "Ctrl+Alt+Del");
  box.appendChild(el("div.screenacts", back, salute, status));
  box.appendChild(canvas);

  let grant;
  try {
    grant = await openConsole(instances, id, "Vnc");
  } catch (e) {
    // The API's own sentence — a viewer without operate is told here why, and
    // what works instead.
    status.textContent = String((e && e.message) || e);
    return;
  }

  const scheme = location.protocol === "https:" ? "wss:" : "ws:";
  const url = scheme + "//" + location.host + consolePath(instances, id)
    + ":consoleStream?session=" + encodeURIComponent(grant.session)
    + "&ticket=" + encodeURIComponent(grant.ticket);
  const ws = new WebSocket(url);
  const client = rfbClient(ws, canvas, (t) => {
    status.textContent = t || "connected — click the screen and type";
  });
  salute.removeAttribute("disabled");
  salute.onclick = () => { saluteWith(client); canvas.focus(); };
  ws.onclose = () => { if (screenOpen) status.textContent = "the screen closed"; };
  ws.onerror = () => { status.textContent = "the screen could not be reached"; };

  // Input. Scaled coordinates: CSS may shrink the canvas to fit, and the guest
  // needs framebuffer positions, not page pixels.
  const posOf = (e) => {
    const r = canvas.getBoundingClientRect();
    return [ (e.clientX - r.left) * (canvas.width / r.width),
             (e.clientY - r.top) * (canvas.height / r.height) ];
  };
  const maskOf = (e) => (e.buttons & 1 ? 1 : 0) | (e.buttons & 4 ? 2 : 0) | (e.buttons & 2 ? 4 : 0);
  canvas.onmousemove = (e) => { const [x, y] = posOf(e); client.button(maskOf(e), x, y); };
  canvas.onmousedown = (e) => { canvas.focus(); const [x, y] = posOf(e); client.button(maskOf(e), x, y); e.preventDefault(); };
  canvas.onmouseup = (e) => { const [x, y] = posOf(e); client.button(maskOf(e), x, y); e.preventDefault(); };
  canvas.oncontextmenu = (e) => e.preventDefault();
  canvas.onwheel = (e) => {
    const [x, y] = posOf(e);
    const b = e.deltaY < 0 ? 8 : 16; // wheel up / down as buttons 4 / 5
    client.button(maskOf(e) | b, x, y);
    client.button(maskOf(e), x, y);
    e.preventDefault();
  };
  canvas.onkeydown = (e) => {
    // The browser's own chords stay the browser's: a screen that swallowed
    // copy, paste and devtools would be a trap, same rule as the terminal.
    if ((e.ctrlKey || e.metaKey) && ["c", "v", "l", "t", "w"].includes(e.key.toLowerCase()) && e.shiftKey === false) {
      // Ctrl+C et al are also things a guest needs. Send them; only leave
      // F5/F12-style browser keys alone below.
      // fallthrough
    }
    const sym = vncKeysym(e);
    if (sym === null) return;
    client.key(true, sym);
    e.preventDefault();
  };
  canvas.onkeyup = (e) => {
    const sym = vncKeysym(e);
    if (sym === null) return;
    client.key(false, sym);
    e.preventDefault();
  };

  screenOpen = () => client.close();
}
