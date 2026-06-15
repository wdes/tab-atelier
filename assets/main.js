    // __TAB_KEY__ is the route segment after `/tabs/` (numeric idx or
    // `by-id/<uuid>` form). The same value is what every subrequest
    // uses, so the share URL identifies one tab end-to-end.
    const TAB_KEY = TAB.key;
    // __TAB_NAME_JS__ is JSON-encoded server-side (handles quotes,
    // backslashes, newlines), so wrapping it in quotes here is safe.
    const TAB_NAME = TAB.name;
    // Short git commit hash of the binary that served this HTML.
    // Compared to every /stream response's X-Build-Hash; a mismatch
    // means the binary was upgraded since this page loaded (i.e. a
    // new deb was installed) and this page is running stale JS.
    // Literal value "unknown" appears for builds outside a git repo
    // (source tarball etc.); the comparison short-circuits for that
    // case so we don't false-positive into the chip.
    const BUILD_HASH = TAB.buildHash;
    const PARAMS = new URLSearchParams(location.search);
    const TOKEN = PARAMS.get("token") || "";
    const READ_ONLY = PARAMS.get("ro") === "1";
    const headers = TOKEN ? { Authorization: "Bearer " + TOKEN } : {};
    const status = document.getElementById("status");

    // The page lives at `<some-prefix>/tabs/<TAB_KEY>/view`. Resolve
    // siblings (`output`, `input`) as relative paths so a reverse
    // proxy (Caddy, nginx) that mounts us under any prefix continues
    // to work — absolute `/tabs/...` URLs bypass the prefix and 404.
    // Strip the trailing `view` to land on the parent directory;
    // fetch('output') from that base resolves correctly.
    const BASE = location.pathname.replace(/\/view\/?$/, "/");

    const term = new Terminal({
      // LF must reset to col 0; without convertEol xterm.js leaves the
      // cursor at the previous column on the next row, so each row of
      // the row-by-row /output dump starts wherever the previous row
      // happened to end (= mid-line garbage in the screenshot).
      convertEol: true,
      cursorBlink: !READ_ONLY,
      disableStdin: READ_ONLY,
      // 'TermSymbols' is the bundled WOFF2 with media-control +
      // dingbat + box-drawing coverage. Listed FIRST so the browser
      // consults it before the system mono — for codepoints in the
      // font's unicode-range (see main.css @font-face), it wins; for
      // everything else the next font in the stack takes over.
      fontFamily: '"TermSymbols", ui-monospace, "JetBrains Mono", "Fira Code", Menlo, monospace',
      fontSize: 13,
      cols: 80,
      rows: 24,
      scrollback: 5000,
      theme: { background: TAB.bg, foreground: "#cccccc" },
      // xterm.js 6.0+: push the erased viewport into scrollback when
      // an ESC[2J (Erase in Display All) sequence fires. Claude Code's
      // TUI redraws on every turn via ESC[2J\ESC[H — without this
      // option those rows are lost forever. With it on, every redraw
      // preserves the previous frame in scrollback. This is THE fix
      // for the missing scrollbar bug. (Combined with stripping the
      // alt-screen toggle in the byte stream so Claude stays in main
      // buffer where scrollback exists at all.)
      scrollOnEraseInDisplay: true,
    });
    const termEl = document.getElementById("term");
    term.open(termEl);

    // Mobile-keyboard incognito hints. xterm.js renders its input
    // path through a hidden helper textarea (.xterm-helper-textarea
    // — created by term.open above). Mobile soft keyboards (Gboard,
    // iOS) honour these attributes to disable user-dictionary
    // learning, autocorrect, autocapitalization, and inline word
    // suggestions. Without them the OS treats a terminal as a
    // normal text input and offers wrong "corrections" on shell
    // commands (`ls`→`is`, `cd`→`cs`) AND adds whatever the user
    // types into the system's predictive-text history.
    //
    // The Grammarly attributes (data-gramm*) are an out-of-band
    // convention the Grammarly browser extension reads to skip the
    // field — its inline UI corrupts the terminal grid otherwise.
    //
    // aria-autocomplete="none" is the accessibility/IME hint that
    // some Android keyboards respect even when autocomplete="off"
    // alone doesn't reach the IME layer.
    const helperTextarea = termEl.querySelector(".xterm-helper-textarea");
    if (helperTextarea) {
      helperTextarea.setAttribute("autocomplete", "off");
      helperTextarea.setAttribute("autocorrect", "off");
      helperTextarea.setAttribute("autocapitalize", "off");
      helperTextarea.setAttribute("spellcheck", "false");
      helperTextarea.setAttribute("inputmode", "text");
      helperTextarea.setAttribute("aria-autocomplete", "none");
      helperTextarea.setAttribute("data-gramm", "false");
      helperTextarea.setAttribute("data-gramm_editor", "false");
      helperTextarea.setAttribute("data-enable-grammarly", "false");
    }

    // Silence xterm.js's terminal-query auto-responses.
    //
    // The viewer is a passive renderer of a byte stream that ALREADY
    // includes the responses the real terminal (server-side alacritty)
    // generated for any queries running programs emitted. xterm.js
    // doesn't know that — when it parses `\x1b[c` (DA), `\x1b[6n`
    // (cursor position), `\x1b]10;?`/`\x1b]11;?` (color queries),
    // `\x1bP+q...\x1b\\` (termcap), etc. in the replay stream, it
    // generates its OWN reply and ships it through `term.onData()` to
    // our /input POST → the shell echoes it → next refresh re-replays
    // the original query and the cycle adds another copy of the reply
    // to the prompt line. Visible bug: `1;2c1;2c1;2c1;...` accumulating
    // every page refresh (that's the printable suffix of `\x1b[?1;2c`).
    //
    // Returning `true` from a parser handler tells xterm.js "I
    // handled it" and skips its default reply. No app-level state —
    // we just drop the response on the floor.
    if (term.parser) {
      // CSI queries with final byte 'c' (DA1/DA2/DA3) and 'n' (DSR/CPR).
      for (const final of ["c", "n"]) {
        term.parser.registerCsiHandler({ final }, () => true);
      }
      // OSC color & hyperlink queries — 10/11/12 are fg/bg/cursor color,
      // 4/104/105 are palette, 8 is hyperlink. The viewer doesn't own
      // any of these — the live terminal does.
      for (const osc of [4, 8, 10, 11, 12, 104, 105]) {
        term.parser.registerOscHandler(osc, () => true);
      }
      // DCS termcap/terminfo query (`\x1bP+q...\x1b\\`).
      term.parser.registerDcsHandler({ final: "q" }, () => true);
    }

    // Touch-scroll the terminal. xterm.js v6 doesn't ship native
    // touch-scroll for mobile — finger drags fall through, the
    // scrollback is unreachable, and the only way to see history is
    // a hardware keyboard's Shift+PgUp (which mobile doesn't have).
    //
    // Map a single-finger vertical drag to `term.scrollLines(N)`:
    // finger DOWN ⇒ OLDER content (scrollLines negative, matches
    // the iOS/Android natural-scroll convention every other app
    // uses), finger UP ⇒ NEWER. Two-finger gestures bypass us so
    // pinch-zoom + page scroll still work.
    //
    // Cell height comes from the rendered viewport (adapts to
    // fitFontToViewport's dynamic sizing); fall back to 18 px if
    // the .xterm-viewport child isn't laid out yet.
    {
      let lastTouchY = null;
      let accumPx = 0;
      const cellHeightPx = () => {
        const v = termEl.querySelector(".xterm-viewport") || termEl;
        const rows = serverRows || term.rows || 24;
        const h = v.getBoundingClientRect().height;
        return h > 0 ? h / rows : 18;
      };
      termEl.addEventListener("touchstart", (e) => {
        if (e.touches.length !== 1) { lastTouchY = null; return; }
        lastTouchY = e.touches[0].clientY;
        accumPx = 0;
      }, { passive: true });
      termEl.addEventListener("touchmove", (e) => {
        if (lastTouchY === null || e.touches.length !== 1) return;
        const y = e.touches[0].clientY;
        accumPx += y - lastTouchY;
        lastTouchY = y;
        const ch = cellHeightPx();
        if (Math.abs(accumPx) >= ch) {
          const lines = Math.trunc(accumPx / ch);
          // finger down (accumPx > 0) ⇒ older content ⇒ negative
          term.scrollLines(-lines);
          accumPx -= lines * ch;
        }
      }, { passive: true });
      const endTouch = () => { lastTouchY = null; accumPx = 0; };
      termEl.addEventListener("touchend", endTouch, { passive: true });
      termEl.addEventListener("touchcancel", endTouch, { passive: true });
    }

    // Copy-selection UX. The /stream poll already pauses term.write()
    // while term.hasSelection() is true (see the gate further down),
    // but xterm.js only flags a selection as "live" on mouseup —
    // during the drag itself, hasSelection() is false. A poll landing
    // mid-drag would still wipe the gesture. Track mousedown/mouseup
    // explicitly so the gate covers both phases.
    //
    // The button uses term.getSelection() (a string snapshot) so the
    // copy works even if a stray write between user click and clipboard
    // write would have cleared xterm.js's internal selection.
    let isSelecting = false;
    termEl.addEventListener("mousedown", () => {
      isSelecting = true;
      document.body.classList.add("selecting");
    });
    document.addEventListener("mouseup", () => {
      isSelecting = false;
      document.body.classList.remove("selecting");
    });
    // Touch-driven selection on mobile uses long-press → drag in
    // xterm.js. The browser fires touchstart immediately; we treat
    // any touch ON the terminal as a potential selection so the
    // overlay buttons stop overlapping the user's gesture.
    termEl.addEventListener("touchstart", () => {
      document.body.classList.add("selecting");
    }, { passive: true });
    const clearSelectingClass = () => document.body.classList.remove("selecting");
    termEl.addEventListener("touchend", clearSelectingClass, { passive: true });
    termEl.addEventListener("touchcancel", clearSelectingClass, { passive: true });
    if (term.onSelectionChange) {
      term.onSelectionChange(() => {
        document.body.classList.toggle("has-selection", term.hasSelection());
      });
    }
    const copyBtn = document.getElementById("copy-btn");
    function copySelectionAndClear() {
      const sel = term.getSelection();
      if (!sel) {
        toast("nothing selected");
        return;
      }
      copyText(sel, `copied ${sel.length} char${sel.length === 1 ? "" : "s"}`);
      // Clear xterm.js's selection so the chip disappears via
      // onSelectionChange. Pre-WS reasoning kept the selection alive
      // "until the next term.write()" so the user could verify; with
      // alt-screen TUIs (Claude Code) the next write can be minutes
      // away, leaving a dangling chip floating over the terminal
      // content. The copy already succeeded — clearing the selection
      // is the natural next state.
      if (term.clearSelection) {
        term.clearSelection();
      }
    }
    if (copyBtn) {
      copyBtn.addEventListener("click", (e) => {
        e.stopPropagation();
        copySelectionAndClear();
      });
    }

    // Catch Ctrl+Shift+C (Linux/Windows terminal copy) BEFORE the
    // browser sees it — otherwise Chrome interprets that combo as
    // "Inspect Element" and DevTools springs open. xterm.js's
    // attachCustomKeyEventHandler runs first; returning false from
    // it prevents both xterm.js's own handling and the browser
    // default action.
    //
    // Cmd+C on macOS works without us intervening — Safari/Chrome
    // copy the live browser selection (which xterm.js maintains as
    // a real DOM selection on the off-screen helper renderer).
    if (term.attachCustomKeyEventHandler) {
      term.attachCustomKeyEventHandler((ev) => {
        if (ev.type !== "keydown") return true;
        if (ev.ctrlKey && ev.shiftKey && (ev.code === "KeyC" || ev.key === "C" || ev.key === "c")) {
          ev.preventDefault();
          ev.stopPropagation();
          copySelectionAndClear();
          return false;
        }
        return true;
      });
    }

    // Persistent top-left keyboard toggle. The Android share-viewer
    // and mobile web both need a deterministic way to bring the soft
    // keyboard back after focus drifts (toolbar tap, drag, screen
    // rotation, scroll). Focusing the term programmatically asks the
    // browser to surface its IME.
    const kbdToggle = document.getElementById("kbd-toggle");
    if (kbdToggle) {
      // Focus the underlying helper textarea DIRECTLY rather than
      // calling term.focus() — on mobile, the IME only surfaces when
      // a real form element receives focus from a user gesture, and
      // xterm.js's helper-textarea may have `display:none` /
      // `width:0` styling that the browser uses as a heuristic to
      // refuse the IME. Calling .focus() on the textarea itself
      // (inside the click handler = a fresh user gesture) is the
      // most reliable cross-browser way to ask for the keyboard.
      kbdToggle.addEventListener("click", (e) => {
        e.preventDefault();
        e.stopPropagation();
        const helper = termEl.querySelector(".xterm-helper-textarea");
        if (helper) {
          helper.focus({ preventScroll: true });
        } else {
          term.focus();
        }
      });
    }

    // ─────────────────────────────────────────────────────────────
    // Mobile keyboard header — port of the Slint KeyboardHeader from
    // android/ta-remote/ui/app.slint. Two rows of buttons (escape +
    // arrows + nav) with an FN mode for F1-F12, plus sticky
    // CTRL/ALT. Each press emits a `TAG_IN` WS frame via the same
    // sendInputBytes path that `term.onData` uses.
    //
    // Only shows on touch devices AND only when the soft keyboard is
    // up (Visual Viewport API tells us). On desktop the toolbar is
    // never useful — Ctrl, ESC, arrows are on the real keyboard.
    // ─────────────────────────────────────────────────────────────
    const kbdHeader = document.getElementById("mobile-kbd-header");
    const isTouchDevice = "ontouchstart" in window || (navigator.maxTouchPoints || 0) > 0;
    if (kbdHeader && isTouchDevice) {
      let ctrlSticky = false;
      let altSticky = false;
      let fnMode = false;

      function sendKeyBytes(bytes) {
        if (!ws || ws.readyState !== WebSocket.OPEN) return;
        // CTRL+printable maps to control codes (A→\x01, …, _→\x1f).
        // Only honoured when bytes is a single printable char; ESC
        // sequences / arrows already carry their own semantics.
        let payload = bytes;
        if (ctrlSticky && bytes.length === 1) {
          const code = bytes.charCodeAt(0);
          if (code >= 0x40 && code <= 0x7f) {
            payload = String.fromCharCode(code & 0x1f);
          }
        }
        if (altSticky && payload.length >= 1 && payload.charCodeAt(0) !== 0x1b) {
          payload = "\x1b" + payload;
        }
        const enc = new TextEncoder().encode(payload);
        try { ws.send(encodeFrame(0x01, enc)); } catch {}
        if (ctrlSticky) { ctrlSticky = false; }
        if (altSticky) { altSticky = false; }
        renderKbdHeader();
      }

      // Each row entry: { label?, icon?, bytes?, type?, action? }
      //   - bytes: send a raw byte string straight to the PTY
      //   - type: "ctrl"/"alt"/"fn"/"kbd" — special sticky toggles
      //   - label: text label (used if no icon)
      //   - icon: emoji-ish glyph (used in priority over label)
      const ROW_NORMAL_1 = [
        { label: "ESC", bytes: "\x1b" },
        { label: "/", bytes: "/" },
        { label: "|", bytes: "|" },
        { label: "-", bytes: "-" },
        { label: "HOME", bytes: "\x1b[H" },
        { icon: "↑", bytes: "\x1b[A" },
        { label: "END", bytes: "\x1b[F" },
        { label: "PG↑", bytes: "\x1b[5~" },
        { label: "FN", type: "fn" },
      ];
      const ROW_NORMAL_2 = [
        { label: "TAB", bytes: "\t" },
        { label: "CTRL", type: "ctrl" },
        { label: "ALT", type: "alt" },
        { icon: "←", bytes: "\x1b[D" },
        { icon: "↓", bytes: "\x1b[B" },
        { icon: "→", bytes: "\x1b[C" },
        { label: "PG↓", bytes: "\x1b[6~" },
        { icon: "⌨", type: "kbd" },
      ];
      const ROW_FN_1 = [
        { label: "F1", bytes: "\x1bOP", afterFnOff: true },
        { label: "F2", bytes: "\x1bOQ", afterFnOff: true },
        { label: "F3", bytes: "\x1bOR", afterFnOff: true },
        { label: "F4", bytes: "\x1bOS", afterFnOff: true },
        { label: "F5", bytes: "\x1b[15~", afterFnOff: true },
        { label: "F6", bytes: "\x1b[17~", afterFnOff: true },
        { label: "F7", bytes: "\x1b[18~", afterFnOff: true },
      ];
      const ROW_FN_2 = [
        { label: "F8", bytes: "\x1b[19~", afterFnOff: true },
        { label: "F9", bytes: "\x1b[20~", afterFnOff: true },
        { label: "F10", bytes: "\x1b[21~", afterFnOff: true },
        { label: "F11", bytes: "\x1b[23~", afterFnOff: true },
        { label: "F12", bytes: "\x1b[24~", afterFnOff: true },
        { icon: "←", type: "fn-exit" },
      ];

      function renderKbdHeader() {
        const rows = fnMode ? [ROW_FN_1, ROW_FN_2] : [ROW_NORMAL_1, ROW_NORMAL_2];
        kbdHeader.innerHTML = "";
        for (const row of rows) {
          const rowEl = document.createElement("div");
          rowEl.className = "kbd-row";
          for (const key of row) {
            const btn = document.createElement("button");
            btn.className = "kbd-key";
            btn.type = "button";
            btn.textContent = key.icon || key.label || "";
            if (key.type === "ctrl" && ctrlSticky) btn.classList.add("sticky-on");
            if (key.type === "alt" && altSticky) btn.classList.add("sticky-on");
            if (key.type === "fn" && fnMode) btn.classList.add("sticky-on");
            btn.addEventListener("touchstart", (e) => { e.preventDefault(); handleKey(key); }, { passive: false });
            btn.addEventListener("click", (e) => { e.preventDefault(); handleKey(key); });
            rowEl.appendChild(btn);
          }
          kbdHeader.appendChild(rowEl);
        }
      }

      function handleKey(key) {
        if (key.type === "ctrl") { ctrlSticky = !ctrlSticky; renderKbdHeader(); return; }
        if (key.type === "alt") { altSticky = !altSticky; renderKbdHeader(); return; }
        if (key.type === "fn") { fnMode = true; renderKbdHeader(); return; }
        if (key.type === "fn-exit") { fnMode = false; renderKbdHeader(); return; }
        if (key.type === "kbd") { term.focus(); return; }
        if (typeof key.bytes === "string") {
          sendKeyBytes(key.bytes);
          if (key.afterFnOff) { fnMode = false; renderKbdHeader(); }
        }
      }

      renderKbdHeader();

      // Visual-viewport-driven soft-keyboard detection. When the IME
      // opens, visualViewport.height shrinks below window.innerHeight.
      // Threshold 150 px catches Android's split-keyboard mode too.
      function updateKbdUp() {
        if (!window.visualViewport) {
          document.body.classList.add("kbd-up"); // fall back to always-show
          return;
        }
        const collapsed = window.innerHeight - window.visualViewport.height;
        document.body.classList.toggle("kbd-up", collapsed > 150);
      }
      updateKbdUp();
      if (window.visualViewport) {
        window.visualViewport.addEventListener("resize", updateKbdUp);
        window.visualViewport.addEventListener("scroll", updateKbdUp);
      }
    }

    // Pick an xterm.js font size so the server's full PTY grid fits
    // the browser viewport width. Monospace char width on the JBM /
    // Fira stack is ~0.6 × fontSize; subtract a small gutter so the
    // page scroll doesn't appear. Clamped 8–18 px — below 8 reads as
    // pixel noise, above 18 the terminal looks comically large on a
    // narrow PTY.
    function fitFontToViewport() {
      if (serverCols <= 0) return;
      const vw = window.innerWidth - 12;
      let target = Math.floor(vw / serverCols / 0.6);
      target = Math.max(8, Math.min(18, target));
      if (target !== term.options.fontSize) {
        term.options.fontSize = target;
      }
    }
    window.addEventListener("resize", fitFontToViewport);
    // Tell xterm.js what this terminal is called (OSC 2). Drives the
    // value xterm.js exposes via onTitleChange / its internal `title`
    // property so addons or screencap tools see the real tab name
    // instead of an empty string. Browser tab title is set via the
    // <title> element from the same __TAB_NAME__ substitution.
    if (TAB_NAME) term.write(`\x1b]2;${TAB_NAME}\x07`);
    // Auto-focus so typing works without an extra click. Skipped in
    // read-only mode so the recipient can scroll without their
    // keypresses being silently dropped into a disabled stdin.
    if (!READ_ONLY) term.focus();
    // Clicking the surrounding area refocuses too — easy to lose
    // focus by tabbing to another browser pane.
    document.body.addEventListener("click", () => { if (!READ_ONLY) term.focus(); });
    // Click handler on the update-available chip — opt-in reload.
    document.getElementById("update-chip").addEventListener("click", (e) => {
      e.stopPropagation();
      location.reload();
    });

    // ── File transfer (inbox/ uploads + outbox/ downloads) ──────────
    // Drag-drop is gated on RW (the POST /files endpoint is rejected
    // for read-only share tokens). Download is available for both
    // RW and RO viewers.
    const UPLOAD_MAX_BYTES = 100 * 1024 * 1024;
    const toastEl = document.getElementById("toast");
    let toastTimer = null;
    function toast(msg, ms = 4000) {
      toastEl.textContent = msg;
      document.body.classList.add("toasting");
      clearTimeout(toastTimer);
      toastTimer = setTimeout(() => document.body.classList.remove("toasting"), ms);
    }
    // Suffix the auth token so `<a download>` clicks work — browsers
    // don't send the Authorization header for navigations.
    function tokenSuffix(initial = "?") {
      return TOKEN ? `${initial}token=${encodeURIComponent(TOKEN)}` : "";
    }
    // Upload a single File via XMLHttpRequest (fetch can't surface
    // upload-progress events). Reports percentage in the status bar,
    // pops a toast on success / error.
    function uploadFile(file) {
      return new Promise((resolve) => {
        if (file.size > UPLOAD_MAX_BYTES) {
          toast(`${file.name}: too large (${Math.round(file.size / 1048576)} MiB > 100 MiB limit)`);
          resolve(false);
          return;
        }
        const xhr = new XMLHttpRequest();
        const url = `${BASE}files?name=${encodeURIComponent(file.name)}${TOKEN ? "&token=" + encodeURIComponent(TOKEN) : ""}`;
        xhr.open("POST", url);
        if (TOKEN) xhr.setRequestHeader("Authorization", "Bearer " + TOKEN);
        xhr.setRequestHeader("Content-Type", "application/octet-stream");
        xhr.upload.addEventListener("progress", (e) => {
          if (e.lengthComputable) {
            const pct = Math.round((e.loaded / e.total) * 100);
            status.textContent = `uploading ${file.name} · ${pct}%`;
          }
        });
        xhr.addEventListener("load", () => {
          if (xhr.status === 201 || xhr.status === 200) {
            // Parse the server's response for the relative path
            // ("inbox/<name>") and offer it as a click-to-copy
            // toast so the user can paste straight into Claude.
            let rel = `inbox/${file.name}`;
            try {
              const j = JSON.parse(xhr.responseText);
              if (j.relpath) rel = j.relpath;
            } catch {}
            toast(`uploaded → ${rel} · click to copy`, 6000);
            const onClick = () => { copyText(rel, "copied: " + rel); toastEl.removeEventListener("click", onClick); };
            toastEl.addEventListener("click", onClick, { once: true });
            resolve(true);
          } else {
            toast(`upload failed (${xhr.status}): ${xhr.responseText.slice(0, 120)}`);
            resolve(false);
          }
        });
        xhr.addEventListener("error", () => {
          toast(`upload failed: network error`);
          resolve(false);
        });
        xhr.send(file);
      });
    }
    // Drag-drop wiring. dragover/leave maintain the overlay; drop
    // fires the upload. Multiple files are uploaded sequentially so
    // the status bar stays a single "uploading X" string instead of
    // racing N progress values.
    if (!READ_ONLY) {
      let dragDepth = 0;  // counter — dragenter/leave fire on every
                          // child element, so we need to balance them
      document.body.addEventListener("dragenter", (e) => {
        e.preventDefault();
        dragDepth++;
        document.body.classList.add("drag-over");
      });
      document.body.addEventListener("dragover", (e) => {
        e.preventDefault();
        e.dataTransfer.dropEffect = "copy";
      });
      document.body.addEventListener("dragleave", (e) => {
        e.preventDefault();
        dragDepth = Math.max(0, dragDepth - 1);
        if (dragDepth === 0) document.body.classList.remove("drag-over");
      });
      document.body.addEventListener("drop", async (e) => {
        e.preventDefault();
        dragDepth = 0;
        document.body.classList.remove("drag-over");
        // Mirror the server-side refusal: POST /files returns 423
        // Locked when serverLocked is true, so suppress the upload
        // pre-flight too and give the user immediate feedback.
        if (serverLocked) {
          toast("tab is locked — uploads refused");
          return;
        }
        const files = Array.from(e.dataTransfer?.files || []);
        if (!files.length) return;
        for (const f of files) {
          await uploadFile(f);
        }
        // Refresh the outbox panel in case a server-side script
        // moves uploads into outbox/ on receipt.
        if (panelKind === "outbox") refreshFiles("outbox");
      });
    }
    // Files panel — single slide-in container, two buttons. The
    // "kind" state is whichever of outbox/inbox is currently open.
    let lastOutboxCount = 0;
    let lastInboxCount = 0;
    let bootstrappedFiles = false;
    let panelKind = null; // "outbox" or "inbox" while the panel is open
    let cachedDir = { outbox: "", inbox: "" };
    if (READ_ONLY) document.body.classList.add("read-only");
    const outboxBtn = document.getElementById("outbox-btn");
    const inboxBtn = document.getElementById("inbox-btn");
    const outboxBadge = document.getElementById("outbox-badge");
    const inboxBadge = document.getElementById("inbox-badge");
    const filesTitle = document.getElementById("files-title");
    const filesDir = document.getElementById("files-dir");
    const filesList = document.getElementById("files-list");
    const filesHint = document.getElementById("files-hint");
    outboxBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      if (panelKind === "outbox" && document.body.classList.contains("files-open")) {
        document.body.classList.remove("files-open");
        panelKind = null;
      } else {
        panelKind = "outbox";
        document.body.classList.add("files-open");
        refreshFiles("outbox");
      }
    });
    inboxBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      if (panelKind === "inbox" && document.body.classList.contains("files-open")) {
        document.body.classList.remove("files-open");
        panelKind = null;
      } else {
        panelKind = "inbox";
        document.body.classList.add("files-open");
        refreshFiles("inbox");
      }
    });
    document.getElementById("files-close").addEventListener("click", (e) => {
      e.stopPropagation();
      document.body.classList.remove("files-open");
      panelKind = null;
    });
    filesDir.addEventListener("click", (e) => {
      e.stopPropagation();
      const path = cachedDir[panelKind] || "";
      if (path) copyText(path, "copied: " + path);
    });
    function humanSize(n) {
      if (n < 1024) return `${n} B`;
      if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KiB`;
      if (n < 1024 * 1024 * 1024) return `${(n / 1048576).toFixed(1)} MiB`;
      return `${(n / 1073741824).toFixed(1)} GiB`;
    }
    function htmlEscape(s) {
      return String(s).replace(/[<>&"']/g, c => ({ "<": "&lt;", ">": "&gt;", "&": "&amp;", '"': "&quot;", "'": "&#39;" }[c]));
    }
    async function copyText(text, toastMsg) {
      try {
        if (navigator.clipboard && navigator.clipboard.writeText) {
          await navigator.clipboard.writeText(text);
        } else {
          // Insecure-context fallback (HTTP, file://, older browsers).
          const ta = document.createElement("textarea");
          ta.value = text; ta.style.position = "fixed"; ta.style.opacity = "0";
          document.body.appendChild(ta); ta.select(); document.execCommand("copy");
          document.body.removeChild(ta);
        }
        if (toastMsg) toast(toastMsg);
      } catch {
        toast("copy failed — select & ⌘C manually");
      }
    }
    // Render either the inbox or outbox listing. Inbox rows are
    // draggable: dragstart populates text/plain + text/uri-list with
    // the file's absolute path so the user can drop it straight into
    // Claude, a code editor, or any other text target. Outbox rows
    // are `<a download>` so left-click downloads, drag drags the
    // file URL.
    async function refreshFiles(kind) {
      filesTitle.textContent = `${kind}/`;
      filesList.textContent = "loading…";
      filesDir.textContent = "";
      try {
        const r = await fetch(`${BASE}${kind}`, { headers });
        if (!r.ok) { filesList.textContent = `error: ${r.status}`; return; }
        const j = await r.json();
        const files = j.files || [];
        cachedDir[kind] = j.dir || "";
        filesDir.textContent = j.dir ? `📁 ${j.dir} (click to copy)` : "";
        filesHint.textContent = kind === "inbox"
          ? "drag a file from this list to drop its full path into Claude or another tool · click to copy the path"
          : "click a file to download · drag to drop the URL elsewhere";
        if (!files.length) {
          filesList.innerHTML = `<div class="empty">${kind} is empty</div>`;
          return;
        }
        filesList.innerHTML = "";
        for (const f of files) {
          const absPath = j.dir ? `${j.dir.replace(/\/+$/, "")}/${f.name}` : `${kind}/${f.name}`;
          const meta = `${humanSize(f.size)} · ${new Date(f.mtime * 1000).toISOString().slice(0, 16).replace("T", " ")}`;
          if (kind === "outbox") {
            const a = document.createElement("a");
            const qpath = encodeURIComponent(`outbox/${f.name}`);
            a.href = `${BASE}files?path=${qpath}${TOKEN ? "&token=" + encodeURIComponent(TOKEN) : ""}`;
            a.download = f.name;
            a.draggable = true;
            a.addEventListener("dragstart", (ev) => {
              ev.dataTransfer.setData("text/plain", absPath);
              ev.dataTransfer.setData("text/uri-list", `file://${absPath}`);
              ev.dataTransfer.effectAllowed = "copyLink";
            });
            a.innerHTML = `${htmlEscape(f.name)}<div class="meta">${meta}</div>`;
            filesList.appendChild(a);
          } else {
            // Inbox row: draggable absolute path, click-to-copy.
            // Not a download link — the agent is what consumes these.
            const div = document.createElement("div");
            div.className = "copy-relpath";
            div.draggable = true;
            div.title = absPath + "\n(drag to Claude or click to copy)";
            div.addEventListener("dragstart", (ev) => {
              ev.dataTransfer.setData("text/plain", absPath);
              ev.dataTransfer.setData("text/uri-list", `file://${absPath}`);
              ev.dataTransfer.effectAllowed = "copyLink";
            });
            div.addEventListener("click", (ev) => {
              ev.stopPropagation();
              copyText(absPath, "copied: " + absPath);
            });
            div.innerHTML = `${htmlEscape(f.name)}<div class="meta">${meta}</div>`;
            filesList.appendChild(div);
          }
        }
      } catch (e) {
        filesList.textContent = `offline: ${e.message || e}`;
      }
    }

    // Monotonic PTY-byte offset we've fed into xterm.js. The server's
    // WebSocket transport. Replaces the previous /stream HTTP polling
    // model: the server PUSHES PTY bytes as soon as they arrive,
    // STATE deltas (lock, schedule, agent, file counts, bg) ride a
    // typed `meta` frame, and there's no 80 ms poll waste at idle.
    //
    // Wire format (mirrors src/api_ws.rs):
    //   tag 0x01 in       C→S  raw bytes typed by the user
    //   tag 0x02 out      S→C  raw PTY bytes
    //   tag 0x03 meta     S→C  JSON state delta
    //   tag 0x04 resize   C→S  JSON {cols, rows}
    //
    // Reconnect: the client tracks `ringOffset` (= total bytes
    // received since session start) and on disconnect reconnects with
    // ?since=<ringOffset> so the server resumes from where we left
    // off. The initial connect uses ?since=0 to replay the full PTY
    // ring on bootstrap (alacritty's grid history is wiped by
    // \x1b[3J and never grows when TUIs redraw in-place; the ring is
    // the only source of historical bytes).

    let ringOffset = 0;
    // Serialises `out` frame handling. A gzip `out-gz` (0x0A) frame
    // inflates asynchronously; without a queue a following raw `out`
    // (0x02) frame could overtake the still-inflating one and corrupt
    // the stream. Every out frame chains off this tail so they apply
    // strictly in arrival order. `.catch` keeps the tail resolved so a
    // single bad frame can't wedge the whole stream.
    let outChain = Promise.resolve();
    let bootstrapped = false;
    let serverCols = 0;
    let serverRows = 0;
    let lockReason = "";
    let scheduleTz = "";
    let scheduleNext = "";
    let scheduleRule = "";
    let serverLocked = false;
    let serverBg = TAB.bg;
    let inScrollback = false;
    let ws = null;
    let reconnectAttempt = 0;
    let reconnectTimer = null;
    let pendingBytesWhileSelecting = "";

    term.onScroll(() => {
      const buf = term.buffer.active;
      inScrollback = buf.viewportY < buf.baseY;
    });

    function wsUrl() {
      const proto = location.protocol === "https:" ? "wss:" : "ws:";
      const params = new URLSearchParams();
      if (TOKEN) params.set("token", TOKEN);
      params.set("since", String(ringOffset));
      return `${proto}//${location.host}${BASE}ws?${params.toString()}`;
    }

    function renderStatus() {
      let lockTag = "";
      if (serverLocked) {
        if (lockReason === "schedule" && scheduleNext) {
          let formattedNext = scheduleNext;
          try {
            const d = new Date(scheduleNext);
            const fmt = new Intl.DateTimeFormat(undefined, {
              weekday: "short", hour: "2-digit", minute: "2-digit",
              timeZone: scheduleTz || undefined,
            });
            formattedNext = fmt.format(d);
          } catch { /* fall through */ }
          lockTag = ` · LOCKED until ${formattedNext}${scheduleTz ? ` ${scheduleTz}` : ""}`;
        } else if (lockReason === "schedule") {
          lockTag = " · LOCKED (schedule)";
        } else {
          lockTag = " · LOCKED";
        }
      }
      status.textContent = `${TAB_NAME} · ${serverCols}x${serverRows} · ${ringOffset}B${lockTag}`;
    }

    function handleMeta(meta) {
      // Lock state
      const lockedNow = !!meta.locked;
      if (lockedNow !== serverLocked) {
        serverLocked = lockedNow;
        document.body.classList.toggle("locked", serverLocked);
        term.options.disableStdin = serverLocked || READ_ONLY;
        term.options.cursorBlink = !(serverLocked || READ_ONLY);
      }
      lockReason = meta.lock_reason || "";
      scheduleTz = meta.schedule_tz || "";
      scheduleNext = meta.schedule_next || "";
      scheduleRule = meta.schedule_rule || "";

      // Background color
      const bgNow = meta.bg_color || "";
      if (/^#[0-9a-fA-F]{6}$/.test(bgNow) && bgNow !== serverBg) {
        serverBg = bgNow;
        document.body.style.background = bgNow;
        term.options.theme = { ...term.options.theme, background: bgNow };
      }

      // Build hash → update-available chip
      const serverHash = meta.build_hash || "";
      if (
        BUILD_HASH && BUILD_HASH !== "unknown" &&
        serverHash && serverHash !== "unknown" &&
        serverHash !== BUILD_HASH
      ) {
        document.body.classList.add("update-available");
      }

      // File counts. RO viewers receive inbox_count=0 (server zeroes
      // it for Authz::Ro); the inbox button is also hidden by CSS
      // in the body.read-only class. Outbox is allowed for both.
      const outboxCount = Number(meta.outbox_count || 0);
      const inboxCount = Number(meta.inbox_count || 0);
      outboxBadge.textContent = String(outboxCount);
      inboxBadge.textContent = String(inboxCount);
      outboxBtn.classList.toggle("has-files", outboxCount > 0);
      inboxBtn.classList.toggle("has-files", inboxCount > 0);
      if (bootstrappedFiles && outboxCount > lastOutboxCount) {
        const delta = outboxCount - lastOutboxCount;
        toast(`📥 ${delta} new file${delta > 1 ? "s" : ""} in outbox/`);
        if (panelKind === "outbox" && document.body.classList.contains("files-open")) refreshFiles("outbox");
      }
      if (bootstrappedFiles && inboxCount > lastInboxCount && panelKind === "inbox" && document.body.classList.contains("files-open")) {
        refreshFiles("inbox");
      }
      bootstrappedFiles = true;
      lastOutboxCount = outboxCount;
      lastInboxCount = inboxCount;

      // Agent state → browser tab title badge
      const agentState = meta.agent_state || "";
      const agentLabel = meta.agent_label || "";
      const STATE_ICON = { thinking: "\u{1f9e0}", waiting: "⏳", error: "❗" };
      const nextTitleTag = agentState && STATE_ICON[agentState]
        ? ` ${STATE_ICON[agentState]}${agentLabel ? " " + agentLabel : ""}`
        : "";
      const nextTitle = `${nextTitleTag ? nextTitleTag.trim() + " · " : ""}${TAB_NAME} · tab-atelier`;
      if (document.title !== nextTitle) document.title = nextTitle;

      // Server-side PTY dims → resize xterm.js to match.
      const cols = Number(meta.cols || 0);
      const rows = Number(meta.rows || 0);
      if (cols > 0 && rows > 0 && (cols !== serverCols || rows !== serverRows)) {
        term.resize(cols, rows);
        serverCols = cols;
        serverRows = rows;
        fitFontToViewport();
      }
      renderStatus();
    }

    // Strip terminal-private mode toggles that hurt the viewer UX:
    //
    //   ?1000 / ?1002 / ?1003   X10 / VT200 / any-motion mouse reports
    //   ?1006 / ?1005 / ?1015   SGR / UTF-8 / urxvt mouse encodings
    //   ?9                      X10 button-only mouse
    //   ?1004                   focus in/out reports
    //
    // Apps inside the PTY (Claude Code, vim, tmux) enable these to
    // grab mouse + focus events. In a SHARE VIEWER we want the
    // opposite: the user must be free to SELECT text with the
    // mouse / touch, and the WS shouldn't be flooded with
    // `\x1b[I` / `\x1b[O` every time the browser tab loses focus.
    // The TUI degrades to keyboard-only input — acceptable for a
    // viewer surface.
    //
    // Alt-screen (?1049 etc.) is NOT in this list — it's restored.
    function stripUIModes(s) {
      // eslint-disable-next-line no-control-regex
      return s.replace(/\x1b\[\?(?:9|1000|1001|1002|1003|1004|1005|1006|1015)[hl]/g, "");
    }

    // Inflate a gzip `out-gz` (0x0A) payload back to the original PTY
    // bytes using the browser's native DecompressionStream — no JS
    // inflate library is shipped. Returns a Promise<Uint8Array>.
    function inflateGzip(bytes) {
      const stream = new Blob([bytes]).stream().pipeThrough(new DecompressionStream("gzip"));
      return new Response(stream).arrayBuffer().then((ab) => new Uint8Array(ab));
    }

    function handleOut(bytes) {
      // Always advance the offset — even while paused — so reconnect
      // can resume from the right place. Defer the actual write to
      // xterm.js if the user is selecting / scrolled up, then flush
      // the buffered bytes once the gate opens.
      //
      // We used to strip the alt-screen toggle (\x1b[?1049h/l) so
      // TUIs would paint into the main buffer where xterm.js's
      // scrollback could accumulate. Cost: TUIs that paint full
      // screens (Claude Code, vim, htop, less) only land 3-4 rows
      // because each repaint cycle uses cursor-positioning over a
      // fixed grid — the rest of the viewport stays the bg colour.
      // Visible on mobile as "Cooked for 10m" + 2 lines on an
      // otherwise empty screen.
      //
      // Re-instated: alt-buffer is the right destination for TUI
      // output. Same model the desktop's local tab uses. While
      // inside the TUI, no scrollback (alt-buffer doesn't have
      // one — terminal spec). When the user exits the TUI,
      // xterm.js drops alt-buffer and restores the main buffer
      // with its full scrollback intact.
      ringOffset += bytes.length;
      const text = new TextDecoder("utf-8", { fatal: false }).decode(bytes);
      const stripped = stripUIModes(text);
      if (inScrollback || isSelecting || term.hasSelection()) {
        pendingBytesWhileSelecting += stripped;
        const queued = pendingBytesWhileSelecting.length;
        if (inScrollback) {
          status.textContent = `${TAB_NAME} · paused (scrolled up) · ${queued}B queued`;
        } else {
          status.textContent = `${TAB_NAME} · selection · click 📋 to copy · ${queued}B queued`;
        }
        return;
      }
      if (pendingBytesWhileSelecting) {
        term.write(pendingBytesWhileSelecting);
        pendingBytesWhileSelecting = "";
      }
      if (stripped.length > 0) term.write(stripped);
      bootstrapped = true;
      renderStatus();
    }

    function encodeFrame(tag, payload) {
      const out = new Uint8Array(1 + payload.length);
      out[0] = tag;
      out.set(payload, 1);
      return out;
    }

    function connect() {
      // Clear any pending reconnect timer — we're connecting now.
      if (reconnectTimer) { clearTimeout(reconnectTimer); reconnectTimer = null; }
      let url;
      try { url = wsUrl(); }
      catch (e) { status.textContent = `bad url · ${e.message || e}`; return; }
      try {
        ws = new WebSocket(url);
      } catch (e) {
        status.textContent = `ws · ${e.message || e}`;
        scheduleReconnect();
        return;
      }
      ws.binaryType = "arraybuffer";
      ws.onopen = () => {
        reconnectAttempt = 0;
        status.textContent = `${TAB_NAME} · connected`;
        document.body.classList.remove("ws-down");
      };
      ws.onmessage = (ev) => {
        if (!(ev.data instanceof ArrayBuffer)) return; // ignore text frames
        const view = new Uint8Array(ev.data);
        if (view.length === 0) return;
        const tag = view[0];
        const payload = view.subarray(1);
        if (tag === 0x02) { // out — raw PTY bytes
          outChain = outChain.then(() => handleOut(payload)).catch((e) => console.warn("out:", e));
        } else if (tag === 0x0a) { // out-gz — gzip-compressed PTY bytes
          outChain = outChain
            .then(() => inflateGzip(payload))
            .then(handleOut)
            .catch((e) => console.warn("out-gz:", e));
        } else if (tag === 0x03) { // meta
          try {
            const json = JSON.parse(new TextDecoder("utf-8").decode(payload));
            handleMeta(json);
          } catch (e) {
            console.warn("bad meta frame:", e);
          }
        }
        // 0x01 in / 0x04 resize / 0x07-0x09 are C→S only — ignore.
      };
      ws.onerror = () => {
        // Defer the user-visible "offline" until onclose so we don't
        // race onclose's reconnect scheduling.
      };
      ws.onclose = (ev) => {
        ws = null;
        document.body.classList.add("ws-down");
        const banner = document.getElementById("ws-state-banner");
        if (ev.code === 1008) {
          // Policy violation — RO sent a write, the tab was locked
          // mid-session, or the process is --read-only. The lock
          // could be transient (a schedule transition), so we retry
          // with capped backoff instead of giving up entirely. The
          // server will simply close us again every reconnect while
          // the policy holds — that's cheap, and it means a tab
          // that unlocks (schedule reopens, user toggled padlock
          // off) recovers automatically without a page refresh.
          const reason = ev.reason || "policy violation";
          status.textContent = `closed · ${reason} · retrying`;
          if (banner) banner.textContent = `🔒 ${reason.toUpperCase()} — retrying every 30 s`;
          // Retry every 30 s — slow because the most common cause
          // is a still-active lock, and we don't want to flood the
          // server.
          reconnectTimer = setTimeout(connect, 30000);
          return;
        }
        status.textContent = `offline · reconnecting…`;
        if (banner) banner.textContent = "⚠ DISCONNECTED — INPUT NOT REACHING SERVER";
        scheduleReconnect();
      };
    }

    function scheduleReconnect() {
      reconnectAttempt = Math.min(reconnectAttempt + 1, 6);
      const delayMs = Math.min(1000 * 2 ** (reconnectAttempt - 1), 30000);
      reconnectTimer = setTimeout(connect, delayMs);
    }

    if (!READ_ONLY) {
      // Send keystrokes straight through. We deliberately do NOT
      // dedup on the client: a per-character mobile IME fires a
      // separate composition for every letter, so an identical
      // back-to-back commit ("ll" in "William", a repeated Backspace)
      // is a REAL second keypress, not an echo. Collapsing those here
      // made it impossible to type doubled letters or hold a key.
      // Duplicate-WORD suppression (the "testtest" mobile-IME echo)
      // is handled server-side by `ImeDedup` in src/api_ws.rs, which
      // whitelists single bytes precisely so repeats survive.
      term.onData(data => {
        // xterm.js's disableStdin should already suppress these, but
        // a tab that locks mid-session may have keypresses already
        // in flight. Also short-circuit if the socket isn't open.
        if (serverLocked) return;
        if (!ws || ws.readyState !== WebSocket.OPEN) return;
        const payload = new TextEncoder().encode(data);
        try { ws.send(encodeFrame(0x01, payload)); } catch { /* swallow */ }
      });
    }

    // Flush pending bytes on selection clear / scroll-back-to-bottom.
    document.addEventListener("mouseup", () => {
      // term.hasSelection() reflects post-mouseup state; the next
      // tick after selection clears, pending bytes flow into the
      // terminal via the next out frame's `handleOut` gate. No
      // explicit flush needed here — but if the user deselects WITH
      // NO new bytes arriving, the queued text would sit forever.
      // Force a flush on the next animation frame.
      requestAnimationFrame(() => {
        if (pendingBytesWhileSelecting && !inScrollback && !isSelecting && !term.hasSelection()) {
          term.write(pendingBytesWhileSelecting);
          pendingBytesWhileSelecting = "";
          renderStatus();
        }
      });
    });

    connect();
