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
      fontFamily: 'ui-monospace, "JetBrains Mono", "Fira Code", Menlo, monospace',
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
        const files = Array.from(e.dataTransfer?.files || []);
        if (!files.length) return;
        for (const f of files) {
          await uploadFile(f);
        }
        // Refresh the outbox panel in case a server-side script
        // moves uploads into outbox/ on receipt.
        refreshOutbox();
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
    // /stream endpoint hands us `[since, X-Stream-Length)` on each
    // poll — the ring captures every byte the PTY emitted, BEFORE
    // alacritty's parser saw it, so it survives Claude's `\x1b[3J`
    // wipes and contains content from in-place TUI redraws that
    // alacritty's grid history never accumulates. xterm.js's own
    // terminal emulation runs over those bytes here and produces a
    // matching scrollback as a side effect.
    let streamOffset = 0;
    let bootstrapped = false;
    let serverCols = 0;
    let serverRows = 0;
    // Live mirror of the server's X-Tab-Locked header. Toggling this
    // disables stdin on xterm.js, hides the cursor blink, drops the
    // input POST wiring, and reveals the red banner. The lock state
    // can change at any poll — the user may flip it from the GUI
    // right-click menu mid-session.
    let serverLocked = false;
    let serverBg = TAB.bg;
    // True while the user has scrolled up into xterm.js's scrollback.
    // While in this state we drop ALL writes — Claude's spinner or
    // any TUI redrawing would otherwise twitch the viewport and we
    // already have everything we need in xterm.js's local buffer to
    // browse history undisturbed. On return-to-bottom we reset the
    // delta cursor so the next poll triggers a full repaint and the
    // user catches up to whatever happened while they were reading.
    let inScrollback = false;
    let wasInScrollback = false;
    term.onScroll(() => {
      const buf = term.buffer.active;
      inScrollback = buf.viewportY < buf.baseY;
      // Returning to bottom no longer needs a resync — /stream is
      // append-only, so xterm.js's scrollback is already coherent
      // with whatever the server thinks the current state is.
      wasInScrollback = inScrollback;
    });

    async function poll() {
      const url = `${BASE}stream?since=${streamOffset}`;
      try {
        const r = await fetch(url, { headers });
        if (!r.ok) { status.textContent = `http ${r.status}`; return; }
        const len = parseInt(r.headers.get("X-Stream-Length") || "0", 10);
        const start = parseInt(r.headers.get("X-Stream-Start") || "0", 10);
        const cols = parseInt(r.headers.get("X-Output-Cols") || "0", 10);
        const rows = parseInt(r.headers.get("X-Output-Rows") || "0", 10);
        // Background color can change mid-session via the daemon's
        // /bg-color endpoint or a Preferences write. Validate the
        // hex and apply to both <body> and xterm.js theme so the
        // gutter matches the terminal.
        const bgNow = r.headers.get("X-Tab-Bg") || "";
        if (/^#[0-9a-fA-F]{6}$/.test(bgNow) && bgNow !== serverBg) {
          serverBg = bgNow;
          document.body.style.background = bgNow;
          term.options.theme = { ...term.options.theme, background: bgNow };
        }
        // Stale-viewer detection. X-Build-Hash carries the binary's
        // compile-time git hash; mismatch means the binary was
        // upgraded since this page loaded. Plain daemon restarts
        // (same binary, same hash) are a silent no-op. Skip when
        // either side is empty or literally "unknown" — that's the
        // non-git-repo fallback and we don't want false positives.
        const serverHash = r.headers.get("X-Build-Hash") || "";
        if (
          BUILD_HASH && BUILD_HASH !== "unknown" &&
          serverHash && serverHash !== "unknown" &&
          serverHash !== BUILD_HASH
        ) {
          document.body.classList.add("update-available");
        }
        // File counts — buttons are always visible (discoverability),
        // badges show the live count, and a count INCREASE pops a
        // toast so the user notices new files without polling.
        const outboxCount = parseInt(r.headers.get("X-Outbox-Count") || "0", 10);
        const inboxCount = parseInt(r.headers.get("X-Inbox-Count") || "0", 10);
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
        const lockedNow = r.headers.get("X-Tab-Locked") === "1";
        if (lockedNow !== serverLocked) {
          serverLocked = lockedNow;
          document.body.classList.toggle("locked", serverLocked);
          term.options.disableStdin = serverLocked || READ_ONLY;
          term.options.cursorBlink = !(serverLocked || READ_ONLY);
        }
        // Agent state badge in the browser tab title, mirroring the
        // desktop GUI's per-tab indicator. Server omits the header
        // when no agent is attached, in which case we reset to plain
        // TAB_NAME.
        const agentState = r.headers.get("X-Agent-State") || "";
        const agentLabelRaw = r.headers.get("X-Agent-Label") || "";
        let agentLabel = "";
        if (agentLabelRaw) {
          try { agentLabel = decodeURIComponent(agentLabelRaw); }
          catch { agentLabel = agentLabelRaw; }
        }
        const STATE_ICON = { thinking: "\u{1f9e0}", waiting: "⏳", error: "❗" };
        const nextTitleTag = agentState && STATE_ICON[agentState]
          ? ` ${STATE_ICON[agentState]}${agentLabel ? " " + agentLabel : ""}`
          : "";
        const nextTitle = `${nextTitleTag ? nextTitleTag.trim() + " · " : ""}${TAB_NAME} · tab-atelier`;
        if (document.title !== nextTitle) document.title = nextTitle;
        // Resize xterm.js to match the server's PTY grid. Without
        // this the browser's wider grid breaks the server's wrapping
        // (long lines stay short, prompt header floats over empty
        // space on the right).
        if (cols > 0 && rows > 0 && (cols !== serverCols || rows !== serverRows)) {
          term.resize(cols, rows);
          serverCols = cols;
          serverRows = rows;
          fitFontToViewport();
        }
        // Drop the playback while the user is reading history. Claude
        // and other TUIs would otherwise jerk the viewport — xterm.js
        // already has every byte it needs to browse history
        // undisturbed. `streamOffset` is still advanced from the
        // header so the catch-up write on return-to-bottom only
        // contains bytes that arrived during the pause.
        if (inScrollback) {
          streamOffset = len;
          status.textContent = `${TAB_NAME} · paused (scrolled up) · ${len}B`;
          return;
        }
        const rawBody = await r.text();
        // Strip alt-screen toggles from the byte stream. Claude Code's
        // TUI (and vim/less/htop/…) emits `\x1b[?1049h` to enter the
        // scratch alt-buffer; xterm.js correctly honors it, and the
        // alt-buffer has no scrollback by spec. With our byte-stream
        // replay model that means EVERY post-toggle byte writes into
        // a buffer the user can never scroll into — the scrollbar
        // visually disappears. Keep xterm.js in the main buffer so
        // scrollback accumulates for all session content. Side effect:
        // alt-screen TUIs render inline rather than in a scratch
        // overlay — desirable for a read-only viewer (the user wants
        // history, not isolation). Also strip the older 1047 / 47
        // variants for the same reason.
        // eslint-disable-next-line no-control-regex
        const body = rawBody.replace(/\x1b\[\?(?:1049|1047|47)[hl]/g, "");
        // /stream is append-only by construction: each response is
        // exactly the bytes the PTY emitted since our `since` offset.
        // xterm.js's terminal emulator runs over them and reproduces
        // the server's grid (including in-place redraws that
        // alacritty's history can never accumulate) and naturally
        // builds up scrollback as old rows are pushed off the top.
        // No `\x1b[2J\x1b[H` resync — that would obliterate xterm.js's
        // local scrollback, defeating the whole point of the ring.
        if (body.length > 0) {
          if (start > streamOffset) {
            // Ring's `base_offset` raced ahead of our `since` —
            // we lost bytes. Best we can do is play the available
            // suffix; the missing prefix would have to come from a
            // ring with bigger capacity. log() to surface in the
            // browser console so users can size up the ring.
            console.warn(`stream gap: requested since=${streamOffset}, got start=${start} (${start - streamOffset} bytes aged out)`);
          }
          term.write(body);
        }
        streamOffset = len;
        bootstrapped = true;
        const lockTag = serverLocked ? " · LOCKED" : "";
        status.textContent = `${TAB_NAME} · ${serverCols}x${serverRows} · ${len}B${lockTag}`;
      } catch (e) {
        status.textContent = `offline · ${e.message || e}`;
      }
    }

    if (!READ_ONLY) {
      term.onData(data => {
        // Server-enforced lock is the source of truth — but we
        // also short-circuit here so we don't fire pointless POSTs
        // that will 403. xterm.js's disableStdin should already
        // suppress these, but a tab that locks mid-session may
        // have keypresses already in flight.
        if (serverLocked) return;
        fetch(`${BASE}input`, {
          method: "POST",
          headers: { ...headers, "Content-Type": "application/octet-stream" },
          body: new TextEncoder().encode(data),
        }).catch(() => {});
      });
    }

    // 80 ms is a good balance — fast enough that typed-then-echoed
    // chars render in ~one frame on a LAN, slow enough that 12 polls
    // per second don't saturate the server. CRC delta keeps each
    // request tiny (a few bytes when only the cursor moves).
    setInterval(poll, 80);
    poll();
