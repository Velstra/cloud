// A way into a guest, for when the network is not one.
//
// What this is and what it is not: a **serial console viewer**, not a terminal
// emulator. It shows what the guest writes and sends what you type, which is
// what somebody watching a machine fail to boot — or logging into one whose
// network never came up — actually needs. It handles the sequences that
// ordinary output uses (carriage return, backspace, erase-line, and colour,
// which it drops) and does not implement cursor addressing, so a full-screen
// program like `top` or `vim` will look wrong in it. That is a known limit and
// not a bug to be surprised by later.
//
// The ticket is spent by opening the stream and is never put in a link: it
// arrives in the query of a websocket the page opens itself, and a session that
// has been attached to is refused a second time.

/// Strip what this viewer does not implement, and act on what it does.
///
/// Kept as a function of a string so it can be reasoned about and tested
/// without a socket. The order matters: escape sequences are removed before the
/// control characters are acted on, or a `\x1b[2K` would be read as an escape
/// and then a stray `[2K`.
function renderable(text) {
  return text
    // CSI: colour, erase, cursor moves. Dropped rather than half-honoured —
    // a viewer that acted on some cursor moves and not others would put output
    // in the wrong place, which is worse than plain text.
    .replace(/\x1b\[[0-9;?]*[a-zA-Z]/g, "")
    // OSC, which is where a guest sets its window title.
    .replace(/\x1b\][^\x07\x1b]*(\x07|\x1b\\)/g, "")
    // Anything else introduced by ESC, one character wide.
    .replace(/\x1b[()#][0-9A-Za-z]/g, "")
    .replace(/\x1b[=>]/g, "");
}

/// Apply one chunk to the text already shown.
///
/// A serial line is not append-only: `\r` returns to the start of the line and
/// what follows overwrites it, which is how every progress indicator and every
/// `[ OK ]` in a boot works. A viewer that appended blindly shows each line
/// several times — which is exactly what makes a boot log unreadable.
function applied(shown, chunk) {
  let out = shown;
  for (const ch of renderable(chunk)) {
    if (ch === "\r") {
      const at = out.lastIndexOf("\n");
      out = out.slice(0, at + 1);
    } else if (ch === "\b") {
      out = out.slice(0, -1);
    } else if (ch === "\x07") {
      // A bell. Nothing to show and nothing to lose.
    } else {
      out += ch;
    }
  }
  return out;
}

/// How much of a guest's output is kept in the page.
///
/// A boot is a few hundred kilobytes and a machine left open for a day is not.
/// The oldest is dropped rather than the newest: what somebody is looking at is
/// the end.
const KEEP = 200000;

/// Open a console onto `name`, into `host`.
///
/// Returns a function that closes it, because a sheet that is closed while a
/// socket is open leaves a session attached against a guest nobody is watching.
function consoleInto(host, coll, id) {
  const screen = el("pre.terminal", { tabindex: "0" });
  const status = el("div.muted", "connecting…");
  host.appendChild(status);
  host.appendChild(screen);

  let socket = null;
  let closed = false;
  let shown = "";

  const show = (chunk) => {
    shown = applied(shown, chunk);
    if (shown.length > KEEP) shown = shown.slice(shown.length - KEEP);
    screen.textContent = shown;
    // Only if the reader is already at the bottom: yanking somebody back to the
    // end while they are reading further up is how a log becomes unusable.
    const atEnd = screen.scrollTop + screen.clientHeight >= screen.scrollHeight - 40;
    if (atEnd) screen.scrollTop = screen.scrollHeight;
  };

  (async () => {
    let grant;
    try {
      grant = await openConsole(coll, id);
    } catch (e) {
      status.textContent = String((e && e.message) || e);
      return;
    }
    if (closed) return;

    const scheme = location.protocol === "https:" ? "wss:" : "ws:";
    // `consolePath` already carries `/api/v1/`; prepending it again produced
    // `/api/v1//api/v1/…`, which matched no route and answered 401.
    const url = scheme + "//" + location.host + consolePath(coll, id)
      + ":consoleStream?session=" + encodeURIComponent(grant.session)
      + "&ticket=" + encodeURIComponent(grant.ticket);
    socket = new WebSocket(url);
    socket.binaryType = "arraybuffer";

    socket.onopen = () => {
      status.textContent = grant.readOnly
        ? "attached — read only, because you may read this guest and not change it"
        : "attached — click the screen and type";
      screen.focus();
    };
    socket.onmessage = (event) => {
      const bytes = typeof event.data === "string"
        ? event.data
        : new TextDecoder().decode(new Uint8Array(event.data));
      show(bytes);
    };
    socket.onclose = () => {
      if (!closed) status.textContent = "the console closed";
    };
    socket.onerror = () => {
      if (!closed) status.textContent = "the console could not be reached";
    };

    screen.addEventListener("keydown", (event) => {
      if (!socket || socket.readyState !== WebSocket.OPEN) return;
      if (grant.readOnly) return;
      // Let the browser's own chords through: a console that swallowed copy
      // would be one nobody could get output out of.
      if (event.ctrlKey && (event.key === "c" || event.key === "v")) {
        if (window.getSelection && String(window.getSelection())) return;
      }
      const bytes = keystroke(event);
      if (bytes === null) return;
      event.preventDefault();
      socket.send(new TextEncoder().encode(bytes));
    });
  })();

  return () => {
    closed = true;
    if (socket) socket.close();
  };
}

/// One key, as the bytes a serial line expects.
///
/// `null` means "not ours" — a modifier on its own, a function key this does not
/// speak — and the browser keeps it. Everything here is what a terminal sends:
/// the arrows and Home/End as the escape sequences a shell's line editor reads,
/// Enter as a carriage return because that is what a tty expects, and Ctrl+key
/// as the control character it names.
function keystroke(event) {
  const key = event.key;
  if (event.ctrlKey && key.length === 1) {
    const code = key.toUpperCase().charCodeAt(0);
    if (code >= 64 && code < 96) return String.fromCharCode(code - 64);
    return null;
  }
  switch (key) {
    case "Enter": return "\r";
    case "Backspace": return "\x7f";
    case "Tab": return "\t";
    case "Escape": return "\x1b";
    case "ArrowUp": return "\x1b[A";
    case "ArrowDown": return "\x1b[B";
    case "ArrowRight": return "\x1b[C";
    case "ArrowLeft": return "\x1b[D";
    case "Home": return "\x1b[H";
    case "End": return "\x1b[F";
    case "Delete": return "\x1b[3~";
    case "PageUp": return "\x1b[5~";
    case "PageDown": return "\x1b[6~";
    default:
      return key.length === 1 && !event.altKey && !event.metaKey ? key : null;
  }
}

/// The console as it appears on a guest's sheet: a button, and a screen once it
/// is pressed.
///
/// Not opened on its own. A ticket is spent by attaching and expires in a
/// minute, so a sheet that attached whenever it was rendered would burn a
/// session every time somebody glanced at a machine — and hold a guest's serial
/// line against nobody watching it.
function consoleSection(host, coll, id) {
  let close = null;
  const screenHost = el("div");
  const button = el("button.btn.quiet", { type: "button" }, "Attach");

  const stop = () => {
    if (close) close();
    close = null;
    screenHost.textContent = "";
    button.textContent = "Attach";
  };

  button.onclick = () => {
    if (close) {
      stop();
      return;
    }
    button.textContent = "Detach";
    close = consoleInto(screenHost, coll, id);
  };

  host.appendChild(button);
  host.appendChild(screenHost);
  // A sheet that is closed with a socket open leaves a session attached against
  // a guest nobody is watching, and the ticket cannot be reused.
  onSheetClose(stop);
}
