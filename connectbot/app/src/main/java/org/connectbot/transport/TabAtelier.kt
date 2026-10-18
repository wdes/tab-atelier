/*
 * ConnectBot: simple, powerful, open-source SSH client for Android
 * Copyright 2025-2026 Kenny Root
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

// New file for Tab Atelier Remote, not part of upstream ConnectBot: the
// tab-atelier transport. The host list reads the daemon's tabs over HTTP in
// TabAtelierClient; this opens one tab's terminal over the daemon's WebSocket.
// The protocol is the daemon's own (see its assets/main.js for the reference
// client), not SSH — one tag byte, then the payload.
//
// TLS is not configured here on purpose: the connection is made with the
// OkHttpClient TabAtelierClient builds for the host, which is what carries the
// certificate pin. Building a client here would quietly opt back into the system
// CA store and bring back the self-signed failure this class exists to avoid.

package org.connectbot.transport

import android.content.Context
import android.net.Uri
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import okio.ByteString
import okio.ByteString.Companion.toByteString
import org.connectbot.R
import org.connectbot.data.entity.Host
import org.connectbot.service.ConnectionState
import org.connectbot.service.DisconnectReason
import org.connectbot.tabatelier.TabAtelierBase
import org.connectbot.tabatelier.ConnectionProgress
import org.connectbot.tabatelier.TabAtelierAddresses
import org.connectbot.service.TerminalManager
import org.connectbot.tabatelier.TabAtelierClient
import org.connectbot.util.SecurePasswordStorage
import org.json.JSONException
import org.json.JSONObject
import timber.log.Timber
import java.io.ByteArrayInputStream
import java.io.IOException
import java.util.concurrent.CountDownLatch
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit
import java.util.zip.GZIPInputStream

/**
 * A tab-atelier daemon: it serves a list of terminal tabs over HTTP +
 * WebSocket, and each tab is one terminal session on the workstation.
 *
 * The session is a WebSocket carrying the daemon's own framing — one tag byte,
 * then the payload — rather than SSH. Which tab to open arrives out of band:
 * `TerminalManager.setPendingTabKey` records it just before the connection
 * starts, and [connect] takes it, because a WebSocket carries no way to ask for
 * a tab and a `Host` row describes the daemon, not one of its tabs.
 *
 * ## This transport reconnects itself, and that is not an optimisation
 *
 * A dropped WebSocket used to leave a session that looked alive and never moved
 * again: the socket's failure was swallowed (see [Listener.onFailure] for why that
 * was wrong), and the app's own reconnect path could not help even once told,
 * because a reconnect builds a *new* transport whose
 * `TerminalManager.takePendingTabKey` is empty — no tab, no session.
 *
 * So the retry lives here, where the tab key still is. That is sound for this
 * protocol and only for this one: a tab is a real terminal on the daemon and the
 * daemon replays its whole scrollback on connect, so after a successful redial the
 * viewer holds the same picture it had before the drop. Nothing is lost by
 * re-dialling, so nothing has to be asked of the user first.
 */
class TabAtelier : AbsTransport() {

    // Written by the listener's own thread (which adopts the socket in onOpen)
    // and read by whoever is sending a keystroke, so it is volatile rather than
    // merely late-initialised.
    @Volatile
    private var socket: WebSocket? = null

    /**
     * Whether the terminal screen still holds something a fresh socket is about to
     * invalidate.
     *
     * Set by [openSocket] whenever a socket is created — the first dial, a tab switch
     * or a redial — and cleared by [read] once the first real bytes have been given
     * the screen-erase that goes with them. See [Reconnect.presentedFrame] for why
     * this exists at all.
     */
    @Volatile
    private var screenStateUnknown = false

    /**
     * Bytes the daemon sent, waiting to be handed to the terminal emulator.
     */
    private val inbound = LinkedBlockingQueue<ByteArray>()

    @Volatile
    private var open = false

    /**
     * Set once the session is over for good, so `read` can report it.
     *
     * **This is the session's death, not its connection's.** Setting it makes `read`
     * return -1, and -1 ends the Relay — which is the only thing that will ever read
     * a socket this transport opens afterwards. So it is set by [close], and by the
     * one failure path where no Relay exists to lose, and by nothing else: a socket
     * that dropped, or a tab switch whose socket would not open, must leave the Relay
     * running so the retry has something to feed.
     *
     * It is also what [isConnected] and [isSessionOpen] test, so a stray write here
     * does not merely stop the feed: it takes the session out of the app's own view
     * of itself, which is how a failed tab switch used to end up looking like a
     * closed session that only a force-disconnect could revive.
     */
    @Volatile
    private var closed = false

    /** A failure worth surfacing instead of a clean end of stream. */
    @Volatile
    private var failure: IOException? = null

    /** The tail of a frame too large for the caller's buffer. */
    private var leftover: ByteArray? = null
    private var leftoverOffset = 0

    /**
     * What a tab switch needs to open another socket on the same server, kept
     * from [connect] because a switch happens long after it has returned.
     */
    private var client: TabAtelierClient? = null
    private var base: TabAtelierBase? = null

    /** The tab this session is on, so a switch to the same one can be a no-op. */
    @Volatile
    private var currentTabKey: String? = null

    /** Every address this server can be reached at. Set in [connect]. */
    @Volatile
    private var addresses: TabAtelierAddresses = TabAtelierAddresses(primary = null, fallbacks = emptyList())

    /**
     * Every address this server can be reached at, parsed, **keyed by the address exactly as
     * it was typed**.
     *
     * Keyed by the typed string and not by the parsed origin, and that is not a detail: an
     * earlier version looked a base up with `bases.first { it.origin == candidate }`, where
     * the candidate is raw text and the origin is normalised. Any host whose address differed
     * between the two — a trailing slash, an uppercase scheme — made `first` throw
     * `NoSuchElementException` and the connection crash. Keeping the typed string as the key
     * means the lookup is a map access that cannot fail.
     *
     * Parsed once in [connect] rather than per dial so that a redial cannot start
     * honouring a different set of addresses than the session came up with — the user
     * editing the host mid-session would otherwise move the session without saying so.
     */
    @Volatile
    private var bases: Map<String, TabAtelierBase> = emptyMap()

    /**
     * The token this session's sockets authenticate with, taken once in [connect].
     *
     * Held rather than looked up per dial because a redial happens whatever the
     * app is doing, and `SecurePasswordStorage` is a keystore call: the token cannot
     * change without the user editing the host, which ends the session anyway.
     */
    @Volatile
    private var token: String? = null

    /**
     * Whether the daemon has accepted a socket for this session yet.
     *
     * Decides what a drop means, and what "connected" means to the app. Before the
     * session has a socket there is nothing to keep alive — the terminal is still on
     * "connecting via tabatelier…" — so a failed attempt is retried in silence. Once
     * one has opened the session is live, and from then on it stays live through a
     * drop: see [handleDrop].
     *
     * Cleared only by [close] and [connect], which is what makes a redial behind the
     * user's back impossible.
     */
    @Volatile
    private var established = false

    /** How many dials in a row have failed, for the backoff. */
    @Volatile
    private var attempts = 0

    /**
     * The redial, when one is in flight, so it can be cancelled once it is not
     * wanted any more — a session closed by the user, or a switch that has replaced
     * the socket it was watching.
     */
    @Volatile
    private var retryJob: Job? = null

    /**
     * Outlives any one socket, because it has to: it is what keeps the session
     * together across a drop. A [SupervisorJob] so a failed redial cannot take the
     * scope with it, and [Dispatchers.IO] because dialling blocks by design (see
     * [openSocket]).
     */
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)

    /**
     * What this session has told the bridge about its socket.
     *
     * Kept here rather than only pushed, so a redial that lands on the state already
     * on screen does not wake the UI to redraw the same colour, and so the state
     * survives the brief window in which there is no socket to ask.
     */
    @Volatile
    private var socketState = ConnectionState.UNKNOWN

    /**
     * Moves this session's reported socket state, when it is actually a move.
     *
     * The bridge is woken as well, because the state is read out of a snapshot the
     * host list took earlier: without the notification the flow would hold the new
     * value and nothing would recompose around it. That is the same
     * `disconnected`-was-set-and-nobody-told failure this class is being fixed for,
     * one layer down.
     */
    private fun setSocketState(state: ConnectionState) {
        if (socketState == state) return
        socketState = state
        bridge?.onSocketStateChanged(state)
    }

    /**
     * Identifies the socket that speaks for this session.
     *
     * A switch replaces the socket, and the socket being replaced still calls back
     * — `onClosed` for the one closed, and possibly `onFailure` for one that was
     * still handshaking. Neither is this session ending, so every listener carries
     * the marker it was made with and ignores callbacks once a later socket has
     * replaced it. Without this, moving a session to another tab would read as the
     * session dying, which is the one thing a switch must not look like.
     */
    @Volatile
    private var currentAttempt: Any? = null

    override fun connect() {
        val host = host
        val service = manager
        if (host == null || service == null) {
            // No service means no context to render anything with, so this is a
            // log line rather than a terminal message.
            Timber.w("tab-atelier transport started without a host or a terminal manager")
            return
        }

        val client = TabAtelierClient(service, SecurePasswordStorage(service))
        val addresses = TabAtelierAddresses(
            primary = host.tabAtelierUrl,
            fallbacks = TabAtelierAddresses.parseFallbacks(host.tabAtelierUrls),
        )
        // Every address has to be parseable, or the session would come up on whichever
        // one happened to work and quietly ignore the rest. Reporting it as "no address"
        // rather than connecting to half of them is the honest answer: the user has a
        // typo in a fallback, and the way to fix it is the editor.
        val parsed = addresses.parsed()
        if (parsed.isEmpty()) {
            bridge?.outputLine(service.getString(R.string.tabatelier_url_missing))
            bridge?.dispatchDisconnect(DisconnectReason.REMOTE_EOF)
            return
        }
        // Kept for switchTab, which has to reach the same server again long after
        // this has returned, and for the redials, which reach it many times more.
        this.client = client
        this.addresses = addresses
        this.bases = parsed
        // The address [openSocket] starts on is decided there, from [addresses] and the
        // current network. This is only the one [switchTab] falls back to when it has to
        // rebuild the transport, and the first address as typed is the right default for
        // that: it is the one the user calls the server.
        this.base = parsed.values.first()
        this.token = client.token(host.id)

        // The tab this session is on is read once and then held, because whoever
        // puts it there is what a redial does not have: see [retryLoop] for why a
        // reconnect that goes through `TerminalManager` cannot work for this
        // protocol.
        val tabKey = service.takePendingTabKey(host.id) ?: currentTabKey
        if (tabKey.isNullOrBlank()) {
            bridge?.outputLine(service.getString(R.string.tabatelier_tab_not_chosen))
            bridge?.dispatchDisconnect(DisconnectReason.REMOTE_EOF)
            return
        }

        // A fresh transport for the same host — the app's own reconnect, or a
        // session restored after the process was killed — starts its history over
        // rather than inheriting the previous transport's counters.
        attempts = 0
        established = false
        closed = false
        failure = null
        currentTabKey = tabKey

        // Before the daemon has said anything there is no session to draw, and the
        // terminal is on its "connecting" line; this is the state that says the app
        // is working on it rather than waiting on the user.
        setSocketState(ConnectionState.RECONNECTING)

        try {
            openSocket(tabKey)
        } catch (e: IOException) {
            // A dial that never opened has nothing to keep alive, and the retry the
            // failed socket started is aimed at a session that does not exist yet:
            // the bridge never got its `onConnected`, so there is no Relay to read
            // whatever that retry would eventually open, and it would attach a
            // viewer to the user's tab that renders nowhere. The caller reports this
            // — `TerminalBridge.startConnection` catches it — and the user retries
            // from the list, which is what every other transport here does.
            cancelRetry()
            setSocketState(ConnectionState.DISCONNECTED)
            throw e
        }

        bridge?.onConnected()
    }

    /**
     * Opens a socket for [tabKey] and waits until the daemon has accepted it.
     *
     * Shared by [connect], [switchTab] and [retryLoop], so every way this session
     * gets a socket sets it up exactly like a fresh one: same route, same auth, same
     * first frames. It says nothing to the bridge and never touches [closed] — a
     * switch or a redial is not a new session, and reporting the old socket's end
     * would stop the read loop that has to survive the move.
     *
     * Waiting for the daemon is not merely tidy. `bridge.onConnected()` is what
     * creates the Relay, which is what reads this transport; starting it on a
     * socket that is not open yet leaves the terminal on "connecting via
     * tabatelier…" forever, which is the failure this replaced. Blocking is
     * therefore expected here, and safe: this runs on the io dispatcher
     * (TerminalBridge.startConnection), never on the UI thread.
     *
     * @throws IOException when the daemon did not accept the socket, which is what
     *   [retryLoop] retries and what [connect] lets the bridge report.
     */
    private fun openSocket(tabKey: String) {
        val host = host ?: throw IOException("No host to open a tab-atelier session for")
        val client = client ?: throw IOException("No connection to open a tab-atelier session on")
        val reachable = bases
        if (reachable.isEmpty()) throw IOException("No address to reach ${host.nickname} at")

        // Which address to start on, and therefore the order to try the rest in. A LAN
        // address goes first when the phone is on the same network as one, so being at
        // home costs no round trip to the remote address before falling back.
        val candidates = addresses.candidates(onLocalNetwork(manager))
        val ordered = walkActiveFirst(reachable, candidates, activeBase)

        var lastFailure: IOException? = null
        for (base in ordered) {
            try {
                openSocketAt(base, tabKey)
                // Remembered so the next dial — a tab switch, or a redial after a drop —
                // starts where this one succeeded rather than re-walking a list whose
                // first entries have just proved unreachable. Moving between networks
                // changes [candidates] and so re-orders the rest, but the address that
                // works keeps its place at the front.
                activeBase = base
                return
            } catch (e: IOException) {
                // One address being down is the expected case, not an error: it is why
                // there is more than one. The last one's reason is the one reported, as
                // that is the address the user is most likely to think of as "the"
                // server — the primary, when every address failed.
                lastFailure = e
                Timber.d(e, "tab-atelier address %s did not answer; trying the next", base.origin)
            }
        }
        throw lastFailure ?: IOException("No address to reach ${host.nickname} at")
    }

    /**
     * The order to walk the parsed addresses in: the one that last worked, then the rest
     * in [TabAtelierAddresses.candidates] order.
     *
     * The first part is what stops a fallback being re-walked every dial. Without it,
     * a redial after a drop would try the unreachable primary first every time, so
     * reconnecting from the phone would cost a full connect timeout per failed address
     * before reaching the one that works.
     *
     * The second part is the LAN preference, applied on top: when the phone has moved to
     * a network with a local address available, that address is a candidate again and the
     * active one drops to its place, so a session does not stay pinned to the remote
     * address it happened to come up on.
     */
    private fun walkActiveFirst(
        reachable: Map<String, TabAtelierBase>,
        candidates: List<String>,
        active: TabAtelierBase?,
    ): List<TabAtelierBase> {
        // A map access per candidate, so a candidate that was not parsed simply does not
        // appear — no lookup can fail, which is what the previous version got wrong.
        val ordered = candidates.mapNotNull { reachable[it] } +
            reachable.filterKeys { it !in candidates }.values

        if (active == null || active !in ordered) return ordered
        return listOf(active) + ordered.filter { it !== active }
    }

    /**
     * Opening a socket has to know the phone's networks, which is a platform call, so it
     * is a seam rather than a direct call: the tests for the ordering are about
     * [TabAtelierAddresses], which is pure.
     */
    private fun onLocalNetwork(service: TerminalManager?): Boolean = service?.isOnLocalNetwork == true

    /**
     * Opens one socket, at one address, and waits for the daemon to accept it.
     *
     * [openSocket] decides *which* address; this does the dialling and throws so that the
     * caller can move on to the next address. Splitting them is what makes a fallback a
     * fallback rather than a retry of the same thing.
     */
    private fun openSocketAt(base: TabAtelierBase, tabKey: String) {
        val host = host ?: throw IOException("No host to open a tab-atelier session for")
        val client = client ?: throw IOException("No connection to open a tab-atelier session on")

        // The token rides in the query string, not an Authorization header: the
        // daemon refuses the header alone on a WS upgrade. See
        // TabAtelierBase.webSocketUrl for the why.
        val url = base.webSocketUrl("/tabs/by-id/$tabKey/ws", token)
        val request = Request.Builder().url(url).build()

        // Marks this socket as the one that speaks for the session, so the socket
        // it replaces — closed, but still calling back — cannot be mistaken for it.
        // Set before the socket exists, so a listener that runs before
        // newWebSocket returns still finds itself current. See [currentAttempt].
        val attempt = Any()
        currentAttempt = attempt
        open = false

        // Set before the socket is created, so there is no window in which the
        // daemon's first bytes could be rendered onto a screen that still holds
        // something else. See [screenStateUnknown].
        screenStateUnknown = true

        val opened = CountDownLatch(1)
        Timber.d("Opening tab-atelier session for host %d at %s", host.id, base.origin)

        // The phases the dialling goes through are reported as they happen, so a slow or
        // stuck connection says *where* it is slow instead of going quiet after one line.
        // The listener is OkHttp's own, so these are real events rather than a guess at
        // what the client is probably doing.
        val progress = startProgress(base)
        try {
            val webSocket = client.clientFor(host.id, base, progress)
                .newWebSocket(request, Listener(opened, attempt))
            socket = webSocket

            if (!opened.await(CONNECT_TIMEOUT_SECONDS, TimeUnit.SECONDS)) {
                webSocket.cancel()
                if (socket === webSocket) socket = null
                throw IOException("Timed out connecting to $url")
            }
            // A socket that failed or closed before opening is a failed connection, not
            // a session that ends immediately: the bridge has to hear about it as such,
            // so the user gets the reason instead of an empty terminal.
            failure?.let { throw it }
            if (!open) {
                throw IOException("The tab-atelier session at $url closed before it opened")
            }
        } finally {
            // Stopped on every path: a ticker left running would outlive the connection and
            // keep writing dots into a terminal that has since started showing real output.
            progress.finish()
            progress.stop()
            // And forgotten, so a later call on the cached client cannot find a stopped
            // reporter and write somewhere that no longer shows anything.
            client?.clearProgress(host.id, progress)
        }
    }

    /**
     * Starts reporting what a dial is doing, into the terminal.
     *
     * Only while connecting: once the session is up the terminal belongs to the remote
     * program, and this must never write into it again — see [ConnectionProgress] for why
     * the dots are dots, and [TerminalBridge.writeProgress] for why whole lines would not do.
     *
     * A null bridge means there is nowhere to write, which happens in tests and in a session
     * that was torn down between the caller deciding to dial and getting here. The progress
     * still ticks, harmlessly — stopping it is the caller's job either way, and making this
     * conditional would mean two shutdown paths.
     */
    private fun startProgress(base: TabAtelierBase): ConnectionProgress {
        val progress = ConnectionProgress(
            write = { text -> bridge?.writeProgress(text) },
        )
        progress.start()
        // Named before any event arrives, so the first thing on screen is that a connection
        // is being attempted at all rather than silence followed by dots.
        progress.phase(manager?.getString(R.string.tabatelier_phase_starting, base.host) ?: base.host)
        return progress
    }

    /** The address this session last established a socket on, if any. */
    @Volatile
    private var activeBase: TabAtelierBase? = null

    /**
     * Moves this session to another tab of the same server.
     *
     * Added for Tab Atelier Remote (Apache-2.0 section 4(b)). The app is a viewer
     * and a tab never dies on the daemon, so another tab of a server the user is
     * already on is a *move* rather than a second session: ConnectBot keys a
     * session by host — which is what the running notification, the host list's
     * connected indicator and the session maps all assume — and a server with
     * several tabs is the case that assumption did not have.
     *
     * **It must not end the session, and that is the whole difficulty.** Relay
     * reads [read], and `-1` ends its read loop, so a switch done by closing this
     * transport and building a new one would kill the session it is moving. So
     * nothing here touches [closed], the socket being replaced is detached from
     * [currentAttempt] before it is closed so its callbacks cannot read as the
     * session ending, and between the two sockets [read] reports "nothing right
     * now" — which is what a socket with no bytes yet reports anyway.
     *
     * The screen needs no clearing. The daemon's replay opens with a form feed, so
     * the new tab's first bytes clear before they repaint, which is what the
     * daemon's own browser client relies on for exactly this. What is dropped is
     * the *queue*: frames of the tab being left are not the tab being opened, and
     * the full replay that follows would otherwise be drawn underneath them.
     *
     * @return true when this session took the move — it moved, or it failed and is
     *   now reporting that failure itself. False when there was nothing to move
     *   (no connection yet, or the same tab again), so the caller should open a
     *   session instead.
     */
    fun switchTab(tabKey: String): Boolean {
        if (tabKey.isBlank() || tabKey == currentTabKey) return false

        // A session can exist without a client and a base. They are set by connect(),
        // and connect() returns before reaching them when the host's address will not
        // parse — so the bridge stays in the session map while the transport has
        // nothing to reach the daemon with.
        //
        // Refusing in that state is what made a server look broken in two ways at
        // once: every tab tap returned here in silence, leaving the user looking at
        // whichever session was already on screen, and because no socket had ever
        // opened, the screen never updated either — which reads as a frozen session
        // rather than as a failed connection.
        //
        // Both are derivable from the host this transport already holds, so they are
        // rebuilt here rather than depended on. A restored session is the other case
        // that lands in this branch, and it is fixed by the same three lines.
        if (client == null || base == null) {
            val host = host ?: return false
            val service = manager ?: return false
            val rebuilt = TabAtelierClient(service, SecurePasswordStorage(service))
            // Rebuilt from the host, and all three fields together: leaving [bases] empty
            // would send [openSocket] down its "no address" path, so the switch would fail
            // with a message about a missing address that the host plainly has.
            val rebuiltAddresses = TabAtelierAddresses(
                primary = host.tabAtelierUrl,
                fallbacks = TabAtelierAddresses.parseFallbacks(host.tabAtelierUrls),
            )
            val rebuiltParsed = rebuiltAddresses.parsed()
            val rebuiltBase = rebuiltParsed.values.firstOrNull()
            if (rebuiltBase == null) {
                // There is nothing to switch *with*, and saying so is the whole point:
                // the address the user gave cannot be reached, and the silence here is
                // what made that take so long to notice. The same message connect()
                // gives, because it is the same problem.
                bridge?.outputLine(service.getString(R.string.tabatelier_url_missing))
                return false
            }
            client = rebuilt
            addresses = rebuiltAddresses
            bases = rebuiltParsed
            base = rebuiltBase
            token = rebuilt.token(host.id)
            Timber.d("Rebuilt the tab-atelier client to switch %d to tab %s", host.id, tabKey)
        }

        // A redial in flight is watching a socket this switch is about to replace,
        // and the state it would land on is not the state the new tab starts in.
        cancelRetry()

        // Switching tabs is the app doing something deliberate and likely to work,
        // so the backoff earned by a server that was down is forgiven here rather
        // than carried into the new tab.
        attempts = 0

        val previous = socket
        currentAttempt = null
        socket = null
        previous?.close(CLOSE_NORMAL, null)

        inbound.clear()
        leftover = null
        leftoverOffset = 0
        // The old socket's failure, if it had one, is not this socket's.
        failure = null

        // The tab the user asked for becomes the session's tab *before* the socket for
        // it is opened, not after.
        //
        // Set here rather than on success because the failure path below redials, and
        // the redial dials [currentTabKey]: leaving it on the tab being left would send
        // the retry back to the tab the user just moved away from. It also makes a
        // second tap on the same tab during that retry a no-op, which is right — the
        // retry is already on it.
        currentTabKey = tabKey

        return try {
            openSocket(tabKey)
            // The socket reports CONNECTED from its own onOpen; nothing to do here.
            Timber.d("Moved the tab-atelier session to tab %s", tabKey)
            true
        } catch (e: IOException) {
            Timber.w(e, "Could not move the tab-atelier session to tab %s", tabKey)
            failure = e

            when (Reconnect.decideSwitchFailure(established)) {
                Reconnect.Action.RETRY_AND_REPORT -> {
                    // **The move failing is a drop, not the end of the session.** The
                    // socket is gone but the session is not: the Relay is still
                    // running, and the same retry that recovers a dropped socket
                    // recovers this — to [currentTabKey], which is the tab the user
                    // chose.
                    //
                    // This is what the previous version got wrong, and it produced
                    // both of the symptoms it was reported for. It set `closed = true`,
                    // which is permanent: `read` then returns -1, -1 ends the Relay,
                    // and the Relay is the only thing that will ever read the socket a
                    // later switch opens. So one failed switch killed the feed for the
                    // rest of the session and left the terminal frozen on the
                    // *previous* tab's content — one tab shown, another one asked for —
                    // until the user force-disconnected, which builds a fresh
                    // transport with a fresh Relay. That is why disconnecting
                    // "restored the terminal feed".
                    setSocketState(ConnectionState.RECONNECTING)
                    manager?.let { service ->
                        bridge?.emitSessionStatus(service.getString(R.string.tabatelier_connection_lost))
                    }
                    retrying()
                }

                else -> {
                    // Nothing was ever established, so no Relay exists to feed and
                    // there is nothing to keep alive. Ending it is the honest outcome;
                    // a retry here would open a socket that nothing reads.
                    closed = true
                    setSocketState(ConnectionState.DISCONNECTED)
                    inbound.offer(END_OF_STREAM)
                }
            }
            true
        }
    }

    private inner class Listener(
        private val opened: CountDownLatch,
        private val attempt: Any,
    ) : WebSocketListener() {

        /**
         * Whether this listener's socket is still the one speaking for the
         * session.
         *
         * A socket that [switchTab] replaced must not be able to end the session
         * it was replaced in, so every callback checks this first. See
         * [currentAttempt].
         */
        private fun isCurrent(): Boolean = currentAttempt === attempt

        override fun onOpen(webSocket: WebSocket, response: Response) {
            if (!isCurrent()) return
            // Adopted here as well as by openSocket, because this can run before
            // newWebSocket returns and the first frames below must not be
            // dropped for want of a socket to send them on.
            socket = webSocket
            open = true
            // A socket that has opened is a live connection, whatever it took to
            // get here, so the backoff starts over: a server that was down and has
            // come back must not keep being retried on the schedule of its outage.
            attempts = 0
            // The daemon stamps the tab's last_used_at on this frame, which is
            // what puts the tab the user just opened at the top of the list — and
            // what moves it there when an open session is switched to it.
            webSocket.send(byteArrayOf(TAG_FOCUS).toByteString())
            onSocketReady()
            opened.countDown()
        }

        override fun onMessage(webSocket: WebSocket, bytes: ByteString) {
            if (!isCurrent()) return
            when (val frame = decodeFrame(bytes.toByteArray())) {
                is Frame.Output -> if (frame.bytes.size > 0) inbound.offer(frame.bytes.toByteArray())
                is Frame.Meta -> {
                    // The name the console's title shows, and the grid its
                    // terminal has to mirror.
                    frame.name?.let { bridge?.setRemoteTabName(it) }
                    frame.grid?.let { bridge?.setRemoteGridSize(it.rows, it.cols) }
                    // Applied on every meta frame, including a null: a tab whose
                    // viewer has no background of its own must not keep the colour
                    // of the tab the session was on before it.
                    bridge?.setRemoteBackgroundColor(frame.bgColor)
                }
                Frame.Ignored -> Unit
            }
        }

        // The daemon sends no text frames; a session that does is not one we
        // understand, and the comment is here so the unused parameter is not
        // mistaken for an oversight.
        override fun onMessage(webSocket: WebSocket, text: String) = Unit

        /**
         * The daemon closed the socket.
         *
         * A close that is not this session asking is the connection dropping, and it
         * is handled exactly like a failure, because to the user it is one: the
         * difference between "the peer said goodbye" and "the peer vanished" is not
         * one a viewer can act on. A close this side asked for never reaches here —
         * [close] detaches [currentAttempt] first, so the callback fails [isCurrent]
         * and is ignored.
         */
        override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
            if (!isCurrent()) {
                Timber.d("Ignoring the close of a tab-atelier socket this session replaced")
                return
            }
            Timber.d("tab-atelier session closed: %d %s", code, reason)
            opened.countDown()
            handleDrop(IOException("the tab-atelier connection closed ($code $reason)"))
        }

        /**
         * The socket failed.
         *
         * **This used to only queue an end-of-stream**, which ended the Relay and told
         * the bridge nothing. That was the bug behind a session that kept a green
         * border, kept its tab, and never moved again: `closed` was never set, so the
         * host list went on deriving "connected", and since nothing called
         * `dispatchDisconnect` the app never entered any state the user could act on
         * but force-disconnect. Every other transport here reports its failures to
         * the bridge; this one now does too, and then redials.
         */
        override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
            if (!isCurrent()) {
                Timber.d(t, "Ignoring the failure of a tab-atelier socket this session replaced")
                return
            }
            Timber.w(t, "tab-atelier session failed")
            opened.countDown()
            handleDrop(IOException(t.message ?: "tab-atelier connection failed", t))
        }
    }

    /**
     * Takes the socket being lost and starts getting it back.
     *
     * Runs on the socket's own callback thread, and does nothing there but stop
     * treating the dead socket as live and hand the redial to [retryLoop]: a socket
     * callback is the one place in OkHttp where blocking is unambiguously wrong.
     *
     * Deliberately does **not** set [closed]. That flag is read by [read] to mean the
     * session is over and by [isConnected], and a session whose socket is down and
     * coming back is neither of those. It is set only when there will be no more
     * attempts.
     */
    private fun handleDrop(cause: IOException) {
        // Whatever arrived before the drop has already been queued and is still
        // worth rendering, so the queue is left alone. Nothing more is coming.
        failure = cause
        open = false
        closeSocketQuietly()

        when (Reconnect.decide(established = established, closed = closed)) {
            Reconnect.Action.GIVE_UP -> {
                // The session is over and the dead socket's callback arrived too
                // late. Nothing to retry: a redial here would re-attach a viewer to
                // the user's tab for a session the app has already closed.
                Timber.d(cause, "Ignoring a drop on a tab-atelier session that is over")
            }

            Reconnect.Action.RETRY_SILENTLY -> {
                // No socket has ever opened for this session, so the connection is
                // still being made: the terminal is on its "connecting" line, the
                // bridge has not been given its `onConnected` yet, and news of a
                // failure here would be news of a session it does not know it has.
                // Retried in silence — [read] returns 0 for an empty queue, which is
                // how the Relay waits rather than how it stops — with the one
                // exception of a *first* dial that fails outright, which [connect]
                // cancels.
                Timber.d(cause, "A tab-atelier socket did not open; retrying")
                retrying()
            }

            Reconnect.Action.RETRY_AND_REPORT -> {
                // A socket that did open has now gone. This is the case that used to
                // be invisible, so it is said twice over: the state the host list
                // draws, and the message the console shows beside the terminal —
                // which is where the user is actually looking when it happens.
                //
                // Deliberately not written *into* the terminal. The daemon replays
                // the tab's scrollback on reconnect, so a line printed into the
                // screen would be overwritten by that replay moments later, and
                // until then it would sit among the remote program's own output
                // looking like something the server said.
                Timber.w(cause, "Tab-atelier socket dropped; redialling")
                manager?.let { service ->
                    bridge?.emitSessionStatus(service.getString(R.string.tabatelier_connection_lost))
                }
                setSocketState(ConnectionState.RECONNECTING)
                retrying()
            }
        }
    }

    /** Starts a redial if one is not already running. */
    private fun retrying() {
        if (retryJob?.isActive == true) return
        retryJob = scope.launch { retryLoop() }
    }

    /**
     * Redials until the session has a socket again, or until it is not wanted.
     *
     * Runs on the transport's own scope rather than the bridge's, because what is
     * being kept alive across the wait is this transport's state: the tab key, the
     * token, the frames queued for the terminal. A retry that went through
     * `TerminalManager.requestReconnect` would build a *new* transport, and a new
     * transport has no tab key — the daemon's protocol carries which tab to open out
     * of band, just once, so a reconnect without it has nothing to open. That is why
     * this lives here, and why the app's own reconnect button used to leave the
     * session dead.
     *
     * The backoff is capped and never gives up. Giving up would mean choosing a
     * moment to tell the user their session is over, and for a viewer that would be
     * a lie: the daemon holds the tab either way, so a session that is still on
     * screen is still worth having, and a server that comes back an hour later comes
     * back to a viewer that is waiting for it.
     */
    private suspend fun retryLoop(
        tabKey: String? = currentTabKey,
        reportFailure: Boolean = true,
        dialFirst: Boolean = false,
    ) {
        // A resume supplies the tab explicitly: it was read off [currentTabKey] before the
        // call, and re-reading it inside the loop would let a switch that landed in
        // between redirect it.
        var target = tabKey
        // A resumed session dials at once instead of waiting out a backoff it did not
        // earn: the reason it was parked is known and is already over — the network came
        // back, or the user picked the phone up — so making them watch a two-second pause
        // would be an artefact of the retry design rather than a fact about the network.
        var waitFirst = !dialFirst
        try {
            while (true) {
                if (waitFirst) delay(Reconnect.delayMillis(attempts))
                waitFirst = true

                // Closed while we were waiting — the user is done with this session.
                if (closed) return
                // Suspended while we were waiting — the phone lost its network, or the app
                // went idle. [resumeSession] will start a fresh loop.
                if (suspended) return

                val attemptKey = target ?: currentTabKey ?: return

                attempts++
                try {
                    openSocket(attemptKey)
                    target = currentTabKey
                    Timber.i("Reconnected the tab-atelier session for tab %s after %d tr%s",
                        attemptKey, attempts, if (attempts == 1) "y" else "ies")
                    failure = null
                    return
                } catch (e: IOException) {
                    // Expected while the server is down, and not the user's news:
                    // the state is already RECONNECTING and saying so again every
                    // few seconds would be noise on the terminal they came to read.
                    //
                    // [reportFailure] is false for a resume, where the reason the socket
                    // was down is known and was chosen by the app rather than the network.
                    if (reportFailure) {
                        Timber.d(e, "Tab-atelier redial %d did not connect", attempts)
                    } else {
                        Timber.d(e, "Tab-atelier resume attempt %d did not connect", attempts)
                    }
                }
            }
        } catch (e: CancellationException) {
            throw e
        } catch (e: Exception) {
            // Nothing above should throw, but a redial loop that dies silently is
            // the exact failure this class is being fixed for.
            Timber.e(e, "The tab-atelier reconnection loop stopped unexpectedly")
            setSocketState(ConnectionState.DISCONNECTED)
        }
    }

    /** Stops any redial in flight, and reports that no more are coming. */
    private fun cancelRetry() {
        retryJob?.cancel()
        retryJob = null
    }

    /**
     * Lets go of the socket while keeping the session, so it can be picked up again.
     *
     * **This is how the app saves battery, and it is deliberately not [close].** `close`
     * ends the session: `read` returns -1, the Relay stops, the bridge is disposed, and
     * getting back in means opening the tab from the list again. Suspending keeps all of
     * that — the transport, the Relay (parked on `read` returning 0), the tab key — so
     * [resume] is one redial and nothing else. For a viewer that distinction is the whole
     * feature: the tab never stops running on the daemon, so there is nothing to lose by
     * dropping the socket, and nothing to rebuild either.
     *
     * Applied when the phone has no network, and when the app has gone this long without
     * being touched — see [org.connectbot.service.TabAtelierConnectionPolicy].
     */
    internal fun suspendSession() {
        if (!established) return
        cancelRetry()
        open = false
        closeSocketQuietly()
        // A redial started before the suspend could still be in flight; its socket is
        // closed by closeSocketQuietly because it is the current attempt.
        suspended = true
        // The outage the session was in is over as far as the next attempt is concerned:
        // when it is resumed, the failure count that belonged to whatever happened before
        // would otherwise start it at a thirty-second wait.
        attempts = 0
        setSocketState(ConnectionState.DISCONNECTED)
    }

    /**
     * Redials a session that [suspendSession] let go of.
     *
     * A no-op unless there is a suspended session with a tab to open, so it is safe to
     * call on every interaction rather than only on the edge where it is needed.
     */
    internal fun resumeSession() {
        if (!suspended || closed) return
        val tabKey = currentTabKey ?: return
        suspended = false
        setSocketState(ConnectionState.RECONNECTING)
        // Dialled at once rather than after a backoff: what parked it — no network, or
        // nobody looking — has just stopped being true.
        retryJob = scope.launch { retryLoop(tabKey = tabKey, reportFailure = false, dialFirst = true) }
    }

    /** Whether [suspendSession] has parked this session. */
    internal val isSuspended: Boolean get() = suspended

    /**
     * Whether this session is parked because the app or the phone went away, rather than
     * because something failed.
     *
     * Set only by [suspendSession] and cleared only by [resumeSession]. Its purpose is to
     * make the difference between "nothing is happening on purpose" and "a redial is
     * in flight" — the two look identical from [read] (no socket, empty queue) and mean
     * opposite things to the retry loop.
     */
    @Volatile
    private var suspended = false

    /**
     * What this transport does about a socket it lost: whether the user is told, and
     * when it dials again.
     *
     * Nested here rather than free-standing because it is *this protocol's* answer and
     * not a general one — a tab-atelier tab survives on the daemon, so a viewer that
     * redials loses nothing, which is what makes silent retrying defensible. SSH's
     * answer to the same event is different, and lives in its own transport.
     *
     * Both halves are pure functions of their arguments, which is deliberate: the
     * decision is the substance of this feature and it would otherwise only be
     * reachable by opening a socket to a real daemon.
     */
    internal object Reconnect {

        /** What a lost socket means for the session that lost it. */
        enum class Action {
            /** The session is already over; this is a late callback for a dead socket. */
            GIVE_UP,

            /** Still connecting: keep dialling, and do not tell the user yet. */
            RETRY_SILENTLY,

            /** A live session lost its socket: tell the user, then keep dialling. */
            RETRY_AND_REPORT,
        }

        /**
         * The rule, stated once.
         *
         * [established] is "a socket has opened for this session", not "a socket is
         * open now" — the second is false in the very case being decided, which is
         * why the flag has to mean the first. [closed] is checked first because a
         * session the user has closed must not be resurrected, whatever else is true
         * of it.
         */
        fun decide(established: Boolean, closed: Boolean): Action = when {
            closed -> Action.GIVE_UP
            established -> Action.RETRY_AND_REPORT
            else -> Action.RETRY_SILENTLY
        }

        /**
         * What a *failed tab switch* means, which is not quite what a dropped socket
         * means — hence a second function rather than a flag.
         *
         * An established session that could not open a socket for the tab it was asked
         * for has not ended; it is the same situation as a socket dropping, and the
         * same retry recovers it. A session that had no socket to begin with has no
         * Relay either, so there is nothing for a retry to feed and ending it is the
         * honest outcome.
         *
         * **The established case must never be [Action.GIVE_UP].** GIVE_UP is what sets
         * `closed`, and `closed` is the session's death, not its connection's: it makes
         * `read` return -1, which ends the Relay, which is the only thing that will
         * ever read the socket a later switch opens. Getting this wrong means one
         * failed switch freezes the terminal on the previous tab's content for the rest
         * of the session and only a force-disconnect revives it. That is not a
         * hypothetical — it is the bug this function was extracted from, and it is
         * why this is a named, tested rule rather than an `if` in a catch block.
         */
        fun decideSwitchFailure(established: Boolean): Action =
            if (established) Action.RETRY_AND_REPORT else Action.GIVE_UP

        /**
         * How long to wait before the next dial, given how many have already failed.
         *
         * A backoff and not a fixed interval, because the two cases are different: a
         * dropped socket on a moving phone is usually back within seconds and should
         * not be made to wait, while a server that is down must not be hammered once
         * a second for as long as the user leaves the app on the screen.
         *
         * The last value repeats for every attempt past the end, so this caps and
         * never gives up: for a viewer, a session that comes back is always worth
         * more than a session declared over. Retrying while the phone has no network
         * is not a special case — OkHttp fails such a dial immediately, which just
         * advances the index to the cap, so an offline phone redials every 30 seconds
         * until it is not offline.
         */
        fun delayMillis(attempts: Int): Long =
            DELAYS_SECONDS[attempts.coerceIn(0, DELAYS_SECONDS.lastIndex)] * 1000L

        /**
         * The frame as the terminal should receive it, given that the screen's present
         * contents cannot be trusted.
         *
         * **This is what stops a switched-to tab from being drawn on top of the tab
         * that was left.** The daemon replays a tab's scrollback from the *start of its
         * ring* whenever a socket attaches with no `since` (`api_ws.rs`: `since=0`),
         * and that replay is not a repaint — it is the raw byte stream from an
         * arbitrary offset, with nothing at the front saying "erase what you have". So
         * a terminal that already holds content renders the replay *into* it: the old
         * tab's screen stays, the new tab's output is written over the top, and the two
         * are interleaved. That is the whole of "switching tabs displays another one"
         * — and of the earlier "renders all tabs at once, except the repainted prompt
         * line", which is the same defect seen before the tab switch stopped rebuilding
         * the session.
         *
         * The same applies after a drop, which redials with no `since` either: the
         * ring is replayed onto a screen that already shows its tail, so the content
         * would otherwise double up. Hence "whenever a socket is created", not "whenever
         * the tab changes".
         *
         * Erasing the scrollback (`CSI 3 J`) as well as the display matters here: this
         * app is a viewer, the daemon keeps the tab, and its ring is what the user
         * scrolls back through — so discarding the client's stale copy loses nothing
         * that a screenful of replayed output does not restore. Leaving it would let
         * the user scroll up out of one tab's output into another's.
         *
         * Applied in [read] rather than [switchTab] deliberately: it is terminal
         * *output*, so it belongs in the stream, in order, on the thread the Relay
         * reads on. Clearing the emulator from elsewhere would race that thread.
         */
        fun presentedFrame(frame: ByteArray, screenStateUnknown: Boolean): ByteArray =
            if (screenStateUnknown) SCREEN_ERASE + frame else frame

        /**
         * Erase display, home the cursor, erase the scrollback.
         *
         * `CSI 2 J` alone would leave old lines above the cursor where the replay is
         * shorter than the screen had been, so the home is not decoration.
         */
        private val SCREEN_ERASE = "\u001b[2J\u001b[H\u001b[3J".toByteArray(Charsets.US_ASCII)

        /** The escalating waits, in seconds; the last one is the cap. */
        private val DELAYS_SECONDS = longArrayOf(2, 5, 10, 20, 30)
    }

    /**
     * Blocks until the daemon sends something, the session ends, or the poll
     * window expires.
     *
     * Blocking is the contract here — `TerminalBridge` reads on a thread of its
     * own, and Telnet's implementation blocks the same way. The poll is what
     * keeps a disconnect responsive without busy-waiting: a closed socket is
     * noticed within the window rather than only when bytes happen to arrive.
     *
     * A dropped socket reports 0, not -1, and that is the whole point of the poll
     * window here: while a redial is in flight the session is neither dead nor
     * delivering, and 0 is exactly "nothing right now". Throwing the socket's
     * failure — which this used to do — ends the Relay, and the Relay is the only
     * thing that will ever read the socket the redial opens. A reconnect that
     * destroys its own reader is not a reconnect.
     */
    @Throws(IOException::class)
    override fun read(buffer: ByteArray, offset: Int, length: Int): Int {
        if (length <= 0) return 0

        while (true) {
            val rest = leftover
            if (rest != null) {
                val n = minOf(length, rest.size - leftoverOffset)
                System.arraycopy(rest, leftoverOffset, buffer, offset, n)
                leftoverOffset += n
                if (leftoverOffset >= rest.size) {
                    leftover = null
                    leftoverOffset = 0
                }
                return n
            }

            val frame = try {
                // **A suspended session polls slowly, and that is the point of it.**
                // The poll is what makes a disconnect responsive, but it is also a
                // wake-up, and a suspended session has no socket to be responsive about.
                // Left at [READ_POLL_MILLIS] this loop would keep waking once a second
                // for as long as the session stayed parked — which is more than the
                // nothing it replaced. Nobody is waiting for these bytes, because the
                // session was suspended precisely because nobody is looking, so the
                // interval stretches to something that costs nothing.
                //
                // Waking up is not delayed by it: `close` and `resumeSession` both push
                // onto [inbound], which returns from the poll immediately.
                inbound.poll(
                    if (suspended) SUSPENDED_POLL_MILLIS else READ_POLL_MILLIS,
                    TimeUnit.MILLISECONDS,
                )
            } catch (e: InterruptedException) {
                Thread.currentThread().interrupt()
                throw IOException("interrupted while reading from the tab-atelier session", e)
            }

            if (frame == null) {
                // Only a session that is over reports its failure or its end.
                // While a redial is in flight — [closed] false, no socket — this
                // is "nothing right now", the same as a socket read timeout.
                if (closed) {
                    failure?.let { throw it }
                    return -1
                }
                return 0
            }
            if (frame.isEmpty()) {
                // Queued by [close], the only thing that ends the session outright.
                if (closed) {
                    failure?.let { throw it }
                    return -1
                }
                continue
            }
            leftover = Reconnect.presentedFrame(frame, screenStateUnknown)
            screenStateUnknown = false
            leftoverOffset = 0
        }
    }

    // Keystrokes go out as they are typed. The daemon writes each frame straight
    // to the PTY, so batching would buy nothing but latency, and one frame per
    // keypress is what the reference client does too.
    @Throws(IOException::class)
    override fun write(buffer: ByteArray) {
        send(TAG_INPUT, buffer)
    }

    @Throws(IOException::class)
    override fun write(c: Int) {
        send(TAG_INPUT, byteArrayOf(c.toByte()))
    }

    // The WebSocket's send queue is already asynchronous; there is nothing
    // buffered here to push.
    @Throws(IOException::class)
    override fun flush() = Unit

    /**
     * Ends the session for good.
     *
     * The retry comes first and by its own path, because everything below is about
     * the socket: a redial that survived this would dial a socket for a session the
     * user has closed, and the daemon would then see a viewer attach to their tab
     * that nothing on the phone admits to.
     */
    override fun close() {
        cancelRetry()
        open = false
        established = false
        closed = true
        setSocketState(ConnectionState.DISCONNECTED)
        closeSocketQuietly()
        // A read parked on the queue learns the session ended instead of waiting
        // out its poll window.
        inbound.offer(END_OF_STREAM)
    }

    /**
     * Whether this session has the daemon.
     *
     * **Deliberately true while a redial is in flight**, which is not the same
     * question as "is a socket open right now" and has to be this one because of who
     * asks. `TerminalBridge.dispatchDisconnect` closes the transport only
     * `if (it.isConnected())`, so a false here while the socket is down would make
     * the user's own Disconnect silently fail to stop the retry: the session would
     * disappear from the list while a coroutine went on re-attaching a viewer to
     * their tab. The other askers are port forwards, which a tab-atelier host does
     * not have.
     *
     * It is also what keeps `ConsoleScreen`'s session-open transition from firing
     * when a redial lands — `isSessionOpen` is what tells that screen the session was
     * just established, and it uses it to raise the software keyboard. A reconnect is
     * not an establishment, and popping the keyboard over the terminal the user is
     * reading would be a strange way to say it came back.
     *
     * For this transport the two predicates are the same thing: there is no socket
     * without a session and no session without a socket, unlike SSH, where the
     * connection and the PTY are genuinely two.
     */
    override fun isConnected(): Boolean = !closed && established

    override fun isSessionOpen(): Boolean = !closed && established

    /**
     * Lets go of the current socket without anybody hearing about it.
     *
     * `currentAttempt` is detached *before* the close, so the socket's own `onClosed`
     * fails [Listener.isCurrent] and is ignored: this side asked for it, so it is not
     * a connection dropping. That distinction is what keeps [close] and [switchTab]
     * from starting a redial of the socket they are deliberately throwing away.
     */
    private fun closeSocketQuietly() {
        currentAttempt = null
        val previous = socket
        socket = null
        previous?.close(CLOSE_NORMAL, null)
    }

    /**
     * Records that a socket is open, and says so if it is news.
     *
     * Called from [Listener.onOpen] and nowhere else, so this is the one place
     * "connected" is established: every route to a socket — the first dial, a switch,
     * a redial — goes through it, and the state is therefore never set from a path
     * that could disagree with whether a socket actually opened.
     */
    private fun onSocketReady() {
        established = true
        failure = null
        setSocketState(ConnectionState.CONNECTED)
    }

    /**
     * Deliberately does nothing.
     *
     * The obvious implementation — tell the daemon what size we are rendering at —
     * is wrong twice over, so it is not done at all rather than sent and hoped
     * for. Resizing a tab is a documented no-op in the daemon's v1: a tab is a real
     * workstation terminal and the desktop wins, because a phone viewer must not
     * reflow a shared PTY out from under an agent's TUI or another viewer. And the
     * daemon's own browser client, which is this protocol's reference, does not
     * send this frame either — it mirrors the server's grid instead.
     *
     * So this client mirrors it too: the meta frame's cols/rows become the
     * terminal's forced size (see [TerminalBridge.setRemoteGridSize]), and the
     * terminal fits its font to that grid. Sending a resize would invite a future
     * daemon to believe the size it was told, which is the opposite of what a
     * mirror wants.
     */
    override fun setDimensions(columns: Int, rows: Int, width: Int, height: Int) = Unit

    /**
     * What one server frame means to this client.
     *
     * Only output and the meta frame are understood; everything else, including
     * the daemon's quick preview paint, is dropped rather than guessed at.
     */
    internal sealed interface Frame {
        /**
         * Terminal output to hand to the emulator.
         *
         * Holds a [ByteString] rather than a `ByteArray` so that this class's own
         * equality means what it looks like it means: `ByteArray` compares by
         * reference, so a `data class` wrapping one is two unequal values for the
         * same bytes — a trap for anything that compares two frames, tests
         * included.
         */
        data class Output(val bytes: ByteString) : Frame

        /**
         * The tab metadata this client uses: what the tab is called, the grid its
         * terminal is using, and the background its viewer paints.
         *
         * One frame carries all three, so one type does — see [decodeFrame]. Every
         * field is nullable when the daemon did not send it in a form this client
         * understands, which is not an error: they are useful independently.
         */
        data class Meta(val name: String?, val grid: Grid?, val bgColor: String?) : Frame {
            /** The grid size the daemon's terminal is using, in rows×cols. */
            data class Grid(val rows: Int, val cols: Int)
        }

        /** Nothing this client renders. */
        data object Ignored : Frame
    }

    /**
     * Decodes one frame: a tag byte, then its payload.
     *
     * Nothing here throws. A frame this client cannot read is dropped, because a
     * newer daemon must not be able to break an older client by sending something
     * it has not learned yet — the alternative is a session that dies on a frame it
     * did not need.
     */
    internal fun decodeFrame(frame: ByteArray): Frame {
        if (frame.isEmpty()) return Frame.Ignored
        return when (frame[0]) {
            TAG_OUTPUT -> Frame.Output(frame.copyOfRange(1, frame.size).toByteString())
            TAG_OUTPUT_GZIP -> Frame.Output(gunzip(frame, 1).toByteString())
            // The tab's own metadata, which is where both the name the console
            // shows and the grid it has to mirror come from.
            TAG_META -> parseMeta(frame)
            // The quick preview paint, tag 0x0c, is deliberately not rendered. It
            // is terminal output by shape but not a stream: a hint to paint the
            // last screen without waiting out a large catch-up. Feeding it to the
            // emulator concatenates it with the real replay that follows, so the
            // session draws the same screen twice.
            else -> Frame.Ignored
        }
    }

    /**
     * The parts of a meta frame this client uses, or [Frame.Ignored] when it
     * carries none of them.
     *
     * Both fields are read defensively. The frame is the daemon's, its shape is
     * not this client's to require, and a field that is absent, null or the wrong
     * type must leave the session working with a default rather than fail it — the
     * metadata is a convenience, the terminal is the point.
     */
    private fun parseMeta(frame: ByteArray): Frame {
        val meta = try {
            JSONObject(String(frame, 1, frame.size - 1, Charsets.UTF_8))
        } catch (e: JSONException) {
            Timber.d(e, "tab-atelier meta frame was not JSON this client understands")
            return Frame.Ignored
        }

        // Absent keys, and the JSON null the daemon uses for "unknown", both
        // arrive as an absent optInt default of 0 — which is not a size.
        val rows = meta.optInt("rows", 0)
        val cols = meta.optInt("cols", 0)
        val grid = if (rows > 0 && cols > 0) Frame.Meta.Grid(rows, cols) else null

        // `isNull` and `has` before `optString`, because `optString` does the
        // wrong thing here in a way that would have shipped: for a JSON null it
        // returns the *string* "null", not the fallback, so the daemon's own
        // "unknown" sentinel would become a tab named "null" and the console
        // would read "server - null". Absent and null are therefore both "no
        // name", and only a real non-blank string is one.
        val rawName = if (meta.has("name") && !meta.isNull("name")) meta.optString("name", "") else null
        val name = rawName?.takeIf { it.isNotBlank() }

        // The tab's own background, which the daemon uses to tell one tab's viewer
        // from another's. Validated as a #rrggbb literal before it is used, exactly
        // as the daemon's own browser client validates it — the value ends up as a
        // colour, and one this client cannot parse must leave the tab's usual
        // background alone rather than fail the session over a cosmetic field.
        // Same null-vs-absent trap as the name, hence the same guards.
        val rawBgColor = if (meta.has(BG_COLOR_KEY) && !meta.isNull(BG_COLOR_KEY)) {
            meta.optString(BG_COLOR_KEY, "")
        } else {
            null
        }
        val bgColor = rawBgColor?.takeIf { HEX_COLOR.matches(it) }

        return if (name == null && grid == null && bgColor == null) {
            Frame.Ignored
        } else {
            Frame.Meta(name, grid, bgColor)
        }
    }

    private fun send(tag: Byte, payload: ByteArray) {
        val socket = socket
        if (socket == null || !open) {
            // Dropped rather than queued, and that is the honest choice: the daemon
            // replays the tab's whole scrollback when the redial lands, so a
            // keystroke held back and delivered minutes later would type into a
            // screen the user has not been looking at. The console shows the
            // reconnecting state, so this is not silent to the user — only to the
            // terminal, which is already telling them.
            Timber.d("Dropping a %d byte frame: the tab-atelier socket is not up", payload.size)
            return
        }
        val frame = ByteArray(payload.size + 1)
        frame[0] = tag
        System.arraycopy(payload, 0, frame, 1, payload.size)
        // A full queue fails rather than blocking the caller, which is the right
        // way round for a terminal: dropping a keystroke beats stalling the UI
        // thread. It should not happen at typing rates.
        if (!socket.send(frame.toByteString())) {
            Timber.w("tab-atelier send queue rejected a %d byte frame", frame.size)
        }
    }

    private fun gunzip(frame: ByteArray, from: Int): ByteArray = try {
        GZIPInputStream(ByteArrayInputStream(frame, from, frame.size - from)).use { it.readBytes() }
    } catch (e: IOException) {
        Timber.w(e, "Ignoring a tab-atelier frame that would not decompress")
        ByteArray(0)
    }

    override fun getDefaultPort(): Int = DEFAULT_PORT

    override fun getDefaultNickname(username: String?, hostname: String?, port: Int): String = getUri(hostname).toString()

    override fun getSelectionArgs(uri: Uri, selection: MutableMap<String, String>) {
        selection["protocol"] = PROTOCOL
        selection["hostname"] = uri.host ?: ""
        selection["port"] = uri.port.toString()
    }

    override fun createHost(uri: Uri): Host {
        val hostname = uri.host ?: ""
        val port = if (uri.port > 0) uri.port else DEFAULT_PORT
        val nickname = uri.fragment?.takeIf { it.isNotEmpty() } ?: getDefaultNickname(null, hostname, uri.port)
        return Host(
            nickname = nickname,
            protocol = PROTOCOL,
            username = "",
            hostname = hostname,
            port = port,
            // A `tabatelier://` link carries no scheme of its own, so the host
            // starts on the daemon's usual https address: the URL is the field
            // the editor shows and the row displays, and a host created from a
            // link should not look empty until it is edited. hostname and port
            // stay filled for the shortcut intent and the editor's re-entry.
            tabAtelierUrl = hostname.takeIf { it.isNotEmpty() }?.let { "https://$it:$port" },
        )
    }

    override fun usesNetwork(): Boolean = true

    override fun getLocalIpAddress(): String? = null

    companion object {
        const val PROTOCOL = "tabatelier"
        const val DEFAULT_PORT = 443

        /**
         * Frame tags, from the daemon's `src/api_ws.rs`. One tag byte, then the
         * payload.
         *
         * Two the daemon defines are deliberately absent, because this client does
         * not speak them: `0x04` resize, which the daemon ignores anyway and which
         * a mirror must not send (see [setDimensions]), and `0x0c` preview, whose
         * payload would be drawn twice if it were fed to the emulator as output
         * (see [decodeFrame]).
         */
        private const val TAG_INPUT: Byte = 0x01
        private const val TAG_OUTPUT: Byte = 0x02
        private const val TAG_META: Byte = 0x03
        private const val TAG_OUTPUT_GZIP: Byte = 0x0A
        private const val TAG_FOCUS: Byte = 0x0B

        private const val CLOSE_NORMAL = 1000

        /**
         * The meta frame's key for a tab's own background colour.
         *
         * A wire name, so it is spelled here rather than derived: the daemon's
         * `tab_meta_json` writes `bg_color` and its viewer reads the same.
         */
        private const val BG_COLOR_KEY = "bg_color"

        /**
         * What a usable background colour looks like: `#rrggbb`, exactly as the
         * daemon's own browser client requires before it will apply one.
         *
         * A malformed value is dropped rather than passed on, so that the app can
         * never be the thing that renders a tab in an unreadable colour because a
         * field was in a shape it did not expect.
         */
        private val HEX_COLOR = Regex("^#[0-9a-fA-F]{6}$")

        /**
         * How long [connect] waits for the daemon to accept the WebSocket before
         * giving up.
         *
         * Comfortably longer than the client's own connect timeout, because what
         * is being waited for is not just the TCP connection: the daemon
         * authenticates the request and attaches to the tab's PTY as well, and a
         * tab that is busy spawning should still open.
         */
        private const val CONNECT_TIMEOUT_SECONDS = 15L

        /**
         * How long a read waits before reporting "nothing yet" to the caller.
         *
         * **This is a wake-up budget, not a latency figure.** The poll does not pace
         * anything the user can perceive: bytes wake [read] the instant the listener
         * queues them, and a keystroke never waits on it. All it sets is how often an idle
         * session looks up to notice that it should stop — so one second is the right order
         * of magnitude, and a quarter of one is not.
         *
         * It is not free, either: the loop runs for as long as the app does, so this
         * divided into a second is how many times it wakes per second while nothing is
         * happening, on a phone that is otherwise asleep. One is enough for a disconnect
         * or a suspend to land within a second of being asked for, which no user can
         * distinguish from immediately.
         */
        private const val READ_POLL_MILLIS = 1_000L

        /**
         * How long a read waits while the session is suspended.
         *
         * Half a minute, which is the difference between a parked session costing nothing
         * and a parked session costing a wake-up a second for as long as it stays parked —
         * the latter being worse than the socket it replaced, since a socket at least does
         * something. The interval does not delay anything the user can see: `close` and
         * `resumeSession` both push onto the queue and return from the poll at once.
         */
        private const val SUSPENDED_POLL_MILLIS = 30_000L

        /** Queued to wake a parked read when the session ends. */
        private val END_OF_STREAM = ByteArray(0)

        @JvmStatic
        fun getProtocolName(): String = PROTOCOL

        @JvmStatic
        fun getUri(input: String?): Uri {
            val authority = input.orEmpty()
            return Uri.Builder()
                .scheme(PROTOCOL)
                .authority(authority)
                .build()
        }

        @JvmStatic
        fun getFormatHint(context: Context): String = String.format(
            "%s:%s",
            context.getString(R.string.format_hostname),
            context.getString(R.string.format_port),
        )
    }
}
