#!/usr/bin/env node
// Drives the tab viewer's reconnect logic in a stubbed browser, and checks what the operator sees.
//
// This exists because of a reported bug: after a network blip the viewer showed "connection lost"
// **while it was working** — output kept arriving and input did nothing, and it never recovered.
// The cause was structural and invisible to a grep: `ws.onclose` ran for any socket's close,
// unconditionally. `ws` is reassigned by every `connect()`, so a close arriving from a socket that
// had already been replaced nulled the *live* connection and re-added `ws-down` — and nothing was
// left to remove it, because the live socket's `onopen` had already fired. Sends check `ws`, so
// input went nowhere; output continued, because `onmessage` closures keep working.
//
// So the assertions here are about behaviour a person would notice: is the banner up, does typing
// reach the server, which socket is the live one. A test that grepped for `ws !== sock` would pass
// on a version of that check written the wrong way round.
//
// The stubs are the boundary: this checks the viewer's logic, not xterm, not the relay.
import { readFileSync } from "node:fs";
import vm from "node:vm";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const source = readFileSync(join(here, "main.js"), "utf8");

// --- the fake browser ---------------------------------------------------------

/** Every socket the viewer opened, in order. */
const sockets = [];

/** Every XHR the viewer opened, in order, so an upload can be driven from the test. */
const requests = [];

/** Timers scheduled but not yet fired, so a reconnect can be stepped through deterministically. */
const timers = [];

/**
 * Just enough `XMLHttpRequest` to run an upload and decide its outcome.
 *
 * The viewer's upload writes progress into the status line and, before the fix, never put the real
 * status back — so the line kept `uploading photo.jpg · 100%` stuck at the bottom of the screen.
 * Driving the request from here is what makes that assertable: the handler that clears it only runs
 * when the request ends, and nothing else can end it.
 */
class FakeXhr {
    constructor() {
        this.upload = { addEventListener: (name, fn) => { this._upload[name] = fn; } };
        this._upload = {};
        this._events = {};
        this.readyState = 0;
        this.status = 0;
        this.responseText = "";
        this.sent = null;
        requests.push(this);
    }

    open(method, url) {
        this.method = method;
        this.url = url;
    }

    setRequestHeader() {}

    addEventListener(name, fn) {
        this._events[name] = fn;
    }

    send(body) {
        this.sent = body;
    }

    /** The server-side progress report. */
    fireProgress(loaded, total) {
        this._upload.progress?.({ lengthComputable: true, loaded, total });
    }

    /** The server-side reply. */
    fireLoad(status = 201, body = '{"path":"inbox/photo.jpg"}') {
        this.status = status;
        this.responseText = body;
        this._events.load?.({});
    }

    /** The network giving up. */
    fireError() {
        this._events.error?.({});
    }
}

class FakeSocket {
    static CONNECTING = 0;
    static OPEN = 1;
    static CLOSING = 2;
    static CLOSED = 3;

    constructor(url) {
        this.url = url;
        this.readyState = FakeSocket.CONNECTING;
        this.sent = [];
        this.closedByViewer = false;
        this.onopen = null;
        this.onmessage = null;
        this.onerror = null;
        this.onclose = null;
        sockets.push(this);
    }

    send(data) {
        if (this.readyState !== FakeSocket.OPEN) {
            throw new Error("send on a socket that is not open");
        }
        this.sent.push(data);
    }

    close() {
        this.closedByViewer = true;
        this.readyState = FakeSocket.CLOSED;
    }

    /** The server-side open. */
    fireOpen() {
        this.readyState = FakeSocket.OPEN;
        this.onopen?.({});
    }

    /** The server-side close — what a dropped network looks like. */
    fireClose(code = 1006) {
        this.readyState = FakeSocket.CLOSED;
        this.onclose?.({ code, reason: "", wasClean: false });
    }
}

/**
 * A real element for the ids this test reads or writes, and a stub for the rest.
 *
 * `#status` has to be genuine: its text is one of the two things under test, so it must be readable
 * and writable. A `Proxy` gives that while every other property — the dozens of methods the viewer
 * calls on other elements — stays a stub, so growing a call in the viewer does not break this test.
 */
function fakeElement() {
    const classes = new Set();
    return new Proxy(
        {
            textContent: "",
            classList: {
                add: (c) => classes.add(c),
                remove: (c) => classes.delete(c),
                contains: (c) => classes.has(c),
            },
            style: {},
            dataset: {},
            _classes: classes,
        },
        {
            get: (target, key) => (key in target ? target[key] : stub(String(key))),
            set: (target, key, value) => {
                target[key] = value;
                return true;
            },
        },
    );
}

/** The element the viewer asks for by id. */
function fakeDocument() {
    const classes = new Set();
    const listeners = new Map();
    // The two elements this test touches are real; the rest are shared stubs.
    const elements = new Map();
    const byId = (id) => {
        if (!elements.has(id)) elements.set(id, fakeElement());
        return elements.get(id);
    };
    const body = {
        classList: {
            add: (c) => classes.add(c),
            remove: (c) => classes.delete(c),
            contains: (c) => classes.has(c),
        },
        // The viewer binds clicks on the body to refocus the terminal. Stubbed, because a click
        // handler cannot affect the socket state machine this test is about.
        addEventListener: () => {},
        removeEventListener: () => {},
    };
    return {
        body,
        getElementById: byId,
        querySelector: () => fakeElement(),
        querySelectorAll: () => [],
        createElement: () => fakeElement(),
        addEventListener: (name, fn) => {
            listeners.set(name, [...(listeners.get(name) ?? []), fn]);
        },
        removeEventListener: () => {},
        visibilityState: "visible",
        _classes: classes,
        _listeners: listeners,
        _byId: byId,
    };
}

/**
 * A stand-in for anything the viewer pokes at that this test does not care about.
 *
 * A proxy rather than a hand-written list of methods: the viewer calls `fit()`, `write()`,
 * `scrollToBottom()`, reads `buffer.active`, adds listeners to `visualViewport` — enumerating all
 * of that would be a stub that breaks every time the viewer grows a call. Any property is another
 * stub, calling one returns a stub, and iterating gives nothing.
 */
function stub(name = "stub") {
    const target = function () {};
    return new Proxy(target, {
        get: (_t, key) => {
            if (key === Symbol.toPrimitive) return () => "";
            if (key === Symbol.iterator) return function* () {};
            if (key === "then") return undefined; // not a promise
            if (key === "length") return 0;
            if (key === "name") return name;
            return stub(String(key));
        },
        apply: () => stub(name + "()"),
        set: () => true,
    });
}

function loadViewer() {
    sockets.length = 0;
    requests.length = 0;
    timers.length = 0;
    // Key handlers registered on the terminal, so a test can invoke the real one rather than a
    // copy of its logic. `stub()` is a Proxy whose `set` trap accepts and discards, so assigning
    // the handler onto the stub and reading it back would return another stub — the Terminal stub
    // therefore wraps it in a Proxy that keeps the one function we need.
    const keyHandlers = [];
    const document = fakeDocument();
    const sandbox = {
        TAB: { key: "12", name: "a tab", buildHash: "test" },
        document,
        // A real pathname: the viewer derives its base path from it, so a stub that returned
        // nothing would throw before the code under test ever ran.
        location: {
            search: "",
            pathname: "/tabs/12/view",
            protocol: "http:",
            host: "localhost:7890",
        },
        navigator: { userAgent: "node", clipboard: undefined },
        WebSocket: FakeSocket,
        XMLHttpRequest: FakeXhr,
        FormData: function () { return { append: () => {} }; },
        Terminal: function () {
            const term = stub("term");
            // `attachCustomKeyEventHandler` is a *method* in xterm.js — the viewer registers its
            // handler by calling `term.attachCustomKeyEventHandler(fn)`, not by assignment — so the
            // handler has to be captured on `get`. A set trap would silently capture nothing, and
            // the test would then pass by asserting against a handler that was never registered.
            return new Proxy(term, {
                get: (target, key) =>
                    key === "attachCustomKeyEventHandler"
                        ? (fn) => { keyHandlers.push(fn); }
                        : Reflect.get(target, key),
                set: () => true,
            });
        },
        requestAnimationFrame: (fn) => fn(),
        fetch: () => Promise.resolve(stub("response")),
        // Timers are captured rather than scheduled so a reconnect can be stepped through without
        // waiting out the backoff — the first delay is a second, and a test that slept would be
        // both slow and flaky.
        setTimeout: (fn, ms) => {
            timers.push({ fn, ms });
            return timers.length;
        },
        clearTimeout: (id) => {
            if (id) timers[id - 1] = null;
        },
        console,
        URLSearchParams,
        TextEncoder,
        TextDecoder,
        Uint8Array,
        Blob,
        Intl,
        Date,
    };
    sandbox.window = sandbox;
    sandbox.globalThis = sandbox;
    // `window` is the sandbox itself, so the window-shaped members have to be on it directly. They
    // are stubs: this test is about the socket state machine, and a resize handler it never fires
    // cannot affect that.
    sandbox.addEventListener = () => {};
    sandbox.removeEventListener = () => {};
    sandbox.innerWidth = 1024;
    sandbox.innerHeight = 768;
    sandbox.visualViewport = stub("visualViewport");
    vm.createContext(sandbox);
    // The script ends by calling `connect()`, so loading it is the first connection.
    vm.runInContext(source, sandbox, { filename: "main.js" });
    return { sandbox, document, keyHandlers };
}

// --- assertions ---------------------------------------------------------------

let failures = 0;
function check(what, condition, detail = "") {
    if (condition) {
        console.log(`  ok    ${what}`);
        return;
    }
    failures += 1;
    console.log(`  FAIL  ${what}${detail ? `\n        ${detail}` : ""}`);
}

console.log("reconnect behaviour\n");

// --- 1. the reported bug ------------------------------------------------------
//
// A reconnect happens while a socket is open — which is what the viewer did after a blip — and the
// superseded socket's close arrives afterwards. That must not mark the live session down, and
// typing must still reach the server.
{
    const { sandbox, document } = loadViewer();
    const first = sockets[0];
    check("connecting opens a socket", sockets.length === 1, `opened ${sockets.length}`);
    first.fireOpen();
    check("a working socket clears the banner", !document.body.classList.contains("ws-down"));

    // The reconnect. Whatever caused it — a retry, a key, a lock retry — a second connect must not
    // leave the first alive to close later.
    const before = sockets.length;
    sandbox.connect();
    const second = sockets.at(-1);
    check("a reconnect opens a new socket", sockets.length === before + 1);
    check(
        "and retires the one it replaces",
        first.closedByViewer,
        "the old socket is still open, so it can still close on us",
    );
    check(
        "detaching its handlers",
        first.onclose === null && first.onmessage === null,
        "a retired socket must not be able to reach the state machine",
    );

    second.fireOpen();
    document.body.classList.remove("ws-down");

    // Now the stale close. The browser may already have queued this event, and the point of the
    // guard is that it is harmless even if one arrives.
    first.fireClose(1006);

    check(
        "a stale close does not mark a live session down",
        !document.body.classList.contains("ws-down"),
        "the banner went up while the session was working — the reported bug",
    );

    // And input still reaches the server: sends check `ws`, so a clobbered `ws` is silent loss.
    const sentBefore = second.sent.length;
    sandbox.sendFocus();
    check(
        "and typing still reaches the live socket",
        second.sent.length > sentBefore,
        `the live socket received nothing (had ${sentBefore} frames)`,
    );
}

// --- 2. a genuine close still reports ----------------------------------------
//
// The guard must not swallow the real thing: if the *current* socket closes, the banner goes up and
// a reconnect is scheduled, or the viewer would sit there looking fine and dead.
{
    const { sandbox, document } = loadViewer();
    const socket = sockets[0];
    socket.fireOpen();
    check("a live session starts clear", !document.body.classList.contains("ws-down"));

    socket.fireClose(1006);
    check(
        "a real close does raise the banner",
        document.body.classList.contains("ws-down"),
        "the guard swallowed a genuine disconnect",
    );
    check("and schedules a reconnect", timers.some(Boolean), "no timer was left pending");

    // Fire the timer: a new socket appears, and opening it recovers.
    const pending = timers.find(Boolean);
    pending.fn();
    const next = sockets.at(-1);
    check("which opens a fresh socket", next !== socket && !next.closedByViewer);
    next.fireOpen();
    check(
        "and recovers the banner",
        !document.body.classList.contains("ws-down"),
        "the session stayed marked down after reconnecting",
    );
    // The recovered socket is the live one, so typing works again.
    const sentBefore = next.sent.length;
    sandbox.sendFocus();
    check("with input working again", next.sent.length > sentBefore);
}

// --- 3. one timer, one socket -------------------------------------------------
//
// Two reconnect paths both assigned the timer without clearing it, so two could be pending and both
// fire — which is how a second socket came to exist beside a live one, and therefore how a stale
// close came to exist at all.
{
    const { sandbox } = loadViewer();
    sockets[0].fireOpen();
    sandbox.scheduleReconnect();
    sandbox.scheduleReconnect();
    sandbox.scheduleReconnect();

    const pending = timers.filter(Boolean);
    check(
        "scheduling three times leaves one pending reconnect",
        pending.length === 1,
        `${pending.length} timers were left to fire`,
    );

    const before = sockets.length;
    pending[0].fn();
    check(
        "and firing it opens exactly one socket",
        sockets.length === before + 1,
        `${sockets.length - before} sockets were opened by one timer`,
    );
}

// --- 4. the status line does not keep an upload's progress --------------------
//
// Reported: a status line stuck at the bottom showing an upload that had finished. `uploadFile`
// writes `uploading name · N%` into `#status` and, before the fix, never put the real status back —
// and the only other writer is `renderStatus`, which runs on a meta message. So the line kept the
// last progress report after every upload, and a batch showed a stale percentage between files.
{
    const { sandbox, document } = loadViewer();
    const status = document.getElementById("status");
    check("the status element starts empty", status.textContent === "");

    const uploading = sandbox.uploadFile({ name: "photo.jpg", size: 1000 });
    const request = requests.at(-1);

    request.fireProgress(470, 1000);
    check(
        "progress is shown while uploading",
        status.textContent.includes("uploading photo.jpg") && status.textContent.includes("47%"),
        `the progress line read ${JSON.stringify(status.textContent)}`,
    );

    request.fireLoad(201);
    await uploading;
    check(
        "a finished upload steps the progress line aside",
        !status.textContent.includes("uploading"),
        `the status line kept ${JSON.stringify(status.textContent)} — which is the reported stuck \
line`,
    );
    check(
        "leaving the session's real status",
        status.textContent.includes("connected") || status.textContent.length > 0,
        "the line was left blank rather than restored",
    );

    // The other way an upload ends: the network gives up. Same restore, and an error to explain it.
    const failing = sandbox.uploadFile({ name: "big.zip", size: 999 });
    const second = requests.at(-1);
    second.fireProgress(310, 999);
    second.fireError();
    await failing;
    check(
        "a failed upload does not keep its progress either",
        !status.textContent.includes("uploading"),
        `the status line kept ${JSON.stringify(status.textContent)}`,
    );
}

// --- Shift+Enter: a newline, not a submit -------------------------------------
//
// xterm.js sends a bare `\r` for Shift+Enter, which is indistinguishable from Enter, so the
// newline has to be encoded in the viewer. These checks cover both halves: that the decision is
// right, and that the decision is actually wired to the wire — the second is the one that fails
// silently when a handler is registered but never consulted.

{
    const { sandbox, keyHandlers } = loadViewer();

    // The pure decision, called directly. `\x1b[13;2u` is kitty-protocol Enter with the shift
    // modifier, which is what tab-atelier emits and crossterm decodes.
    const shifted = sandbox.shiftEnterBytes({ key: "Enter", shiftKey: true });
    check(
        "Shift+Enter encodes as the kitty-protocol sequence",
        shifted instanceof Uint8Array
            && Array.from(shifted).join(",") === "27,91,49,51,59,50,117",
        `got ${shifted ? Array.from(shifted).join(",") : String(shifted)}`,
    );
    check(
        "a plain Enter is left to xterm",
        sandbox.shiftEnterBytes({ key: "Enter", shiftKey: false }) === null,
        "plain Enter must not be intercepted, or submitting would break",
    );
    check(
        "no other key is intercepted",
        sandbox.shiftEnterBytes({ key: "a", shiftKey: true }) === null
            && sandbox.shiftEnterBytes({ key: "Tab", shiftKey: true }) === null,
        "Shift+A and Shift+Tab must not become newlines",
    );
    check(
        "an absent event does not throw",
        sandbox.shiftEnterBytes(undefined) === null && sandbox.shiftEnterBytes(null) === null,
        "the handler is called with whatever the browser passes",
    );

    // The wiring: the handler the viewer registered has to route Shift+Enter to the socket and
    // stop xterm emitting its own `\r` as well.
    check(
        "the viewer registers a key handler",
        keyHandlers.length === 1,
        `expected one handler, found ${keyHandlers.length}`,
    );
    const socket = sockets.at(-1);
    socket.readyState = FakeSocket.OPEN;
    const consumed = keyHandlers[0]({ type: "keydown", key: "Enter", shiftKey: true, preventDefault() {}, stopPropagation() {} });
    const frame = socket.sent.at(-1);
    check(
        "Shift+Enter is sent and consumed",
        consumed === false && frame && frame[0] === 0x01
            && Array.from(frame.slice(1)).join(",") === "27,91,49,51,59,50,117",
        `consumed=${consumed} frame=${frame ? Array.from(frame).join(",") : "none"}`,
    );
    check(
        "Shift+Enter does not also send a carriage return",
        socket.sent.length === 1,
        `expected one frame, got ${socket.sent.length}`,
    );

    // And a plain Enter must fall through to xterm rather than being swallowed, or nothing
    // would ever submit.
    const before = socket.sent.length;
    const passed = keyHandlers[0]({ type: "keydown", key: "Enter", shiftKey: false, preventDefault() {}, stopPropagation() {} });
    check(
        "a plain Enter falls through to xterm",
        passed === true && socket.sent.length === before,
        `returned ${passed} after ${socket.sent.length - before} extra frames`,
    );
}

console.log(failures === 0 ? "\nreconnect: ok" : `\nreconnect: ${failures} failed`);
process.exit(failures === 0 ? 0 : 1);