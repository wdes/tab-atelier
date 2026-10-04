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
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import okio.ByteString
import okio.ByteString.Companion.toByteString
import org.connectbot.R
import org.connectbot.data.entity.Host
import org.connectbot.service.DisconnectReason
import org.connectbot.tabatelier.TabAtelierBase
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
 */
class TabAtelier : AbsTransport() {

    // Written by the listener's own thread (which adopts the socket in onOpen)
    // and read by whoever is sending a keystroke, so it is volatile rather than
    // merely late-initialised.
    @Volatile
    private var socket: WebSocket? = null

    /** Bytes the daemon sent, waiting to be handed to the terminal emulator. */
    private val inbound = LinkedBlockingQueue<ByteArray>()

    @Volatile
    private var open = false

    /** Set once the daemon has gone away, so `read` can report it. */
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
        val base = client.base(host.tabAtelierUrl?.takeIf { it.isNotBlank() })
        if (base == null) {
            bridge?.outputLine(service.getString(R.string.tabatelier_url_missing))
            bridge?.dispatchDisconnect(DisconnectReason.REMOTE_EOF)
            return
        }
        // Kept for switchTab, which has to reach the same server again long after
        // this has returned.
        this.client = client
        this.base = base

        // Which tab, and only just decided: taken rather than read, so a later
        // session cannot inherit it from this one.
        val tabKey = service.takePendingTabKey(host.id)
        if (tabKey.isNullOrBlank()) {
            bridge?.outputLine(service.getString(R.string.tabatelier_tab_not_chosen))
            bridge?.dispatchDisconnect(DisconnectReason.REMOTE_EOF)
            return
        }

        currentTabKey = tabKey
        openSocket(tabKey)

        bridge?.onConnected()
    }

    /**
     * Opens a socket for [tabKey] and waits until the daemon has accepted it.
     *
     * Shared by [connect] and [switchTab], so a moved session is set up exactly
     * like a fresh one: same route, same auth, same first frames. It says nothing
     * to the bridge and never touches [closed] — a switch is not a new session, and
     * reporting the old socket's end would stop the read loop that has to survive
     * the move.
     *
     * Waiting for the daemon is not merely tidy. `bridge.onConnected()` is what
     * creates the Relay, which is what reads this transport; starting it on a
     * socket that is not open yet leaves the terminal on "connecting via
     * tabatelier…" forever, which is the failure this replaced. Blocking is
     * therefore expected here, and safe: this runs on the io dispatcher
     * (TerminalBridge.startConnection), never on the UI thread.
     */
    private fun openSocket(tabKey: String) {
        val host = host ?: throw IOException("No host to open a tab-atelier session for")
        val client = client ?: throw IOException("No connection to open a tab-atelier session on")
        val base = base ?: throw IOException("No address to reach ${host.nickname} at")

        // The token rides in the query string, not an Authorization header: the
        // daemon refuses the header alone on a WS upgrade. See
        // TabAtelierBase.webSocketUrl for the why.
        val url = base.webSocketUrl("/tabs/by-id/$tabKey/ws", client.token(host.id))
        val request = Request.Builder().url(url).build()

        // Marks this socket as the one that speaks for the session, so the socket
        // it replaces — closed, but still calling back — cannot be mistaken for it.
        // Set before the socket exists, so a listener that runs before
        // newWebSocket returns still finds itself current. See [currentAttempt].
        val attempt = Any()
        currentAttempt = attempt
        open = false

        val opened = CountDownLatch(1)
        Timber.d("Opening tab-atelier session for host %d at %s", host.id, base.origin)
        val webSocket = client.clientFor(host.id, base).newWebSocket(request, Listener(opened, attempt))
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
    }

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
        if (client == null || base == null) return false

        val previous = socket
        currentAttempt = null
        socket = null
        previous?.close(CLOSE_NORMAL, null)

        inbound.clear()
        leftover = null
        leftoverOffset = 0
        // The old socket's failure, if it had one, is not this socket's.
        failure = null

        return try {
            openSocket(tabKey)
            currentTabKey = tabKey
            Timber.d("Moved the tab-atelier session to tab %s", tabKey)
            true
        } catch (e: IOException) {
            // The move failed and the session has no socket left. Reported the way a
            // lost connection is reported, rather than leaving a terminal that looks
            // alive and never updates again.
            Timber.w(e, "Could not move the tab-atelier session to tab %s", tabKey)
            failure = e
            closed = true
            inbound.offer(END_OF_STREAM)
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
            // The daemon stamps the tab's last_used_at on this frame, which is
            // what puts the tab the user just opened at the top of the list — and
            // what moves it there when an open session is switched to it.
            webSocket.send(byteArrayOf(TAG_FOCUS).toByteString())
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
                }
                Frame.Ignored -> Unit
            }
        }

        // The daemon sends no text frames; a session that does is not one we
        // understand, and the comment is here so the unused parameter is not
        // mistaken for an oversight.
        override fun onMessage(webSocket: WebSocket, text: String) = Unit

        override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
            if (!isCurrent()) {
                Timber.d("Ignoring the close of a tab-atelier socket this session replaced")
                return
            }
            Timber.d("tab-atelier session closed: %d %s", code, reason)
            closed = true
            inbound.offer(END_OF_STREAM)
            opened.countDown()
        }

        override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
            if (!isCurrent()) {
                Timber.d(t, "Ignoring the failure of a tab-atelier socket this session replaced")
                return
            }
            Timber.w(t, "tab-atelier session failed")
            failure = IOException(t.message ?: "tab-atelier connection failed", t)
            closed = true
            inbound.offer(END_OF_STREAM)
            opened.countDown()
        }
    }

    /**
     * Blocks until the daemon sends something, the session ends, or the poll
     * window expires.
     *
     * Blocking is the contract here — `TerminalBridge` reads on a thread of its
     * own, and Telnet's implementation blocks the same way. The poll is what
     * keeps a disconnect responsive without busy-waiting: a closed socket is
     * noticed within the window rather than only when bytes happen to arrive.
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
                inbound.poll(READ_POLL_MILLIS, TimeUnit.MILLISECONDS)
            } catch (e: InterruptedException) {
                Thread.currentThread().interrupt()
                throw IOException("interrupted while reading from the tab-atelier session", e)
            }

            if (frame == null) {
                failure?.let { throw it }
                // -1 is what ends the relay's read loop, so it is reserved for
                // the session being over and nothing else. A connection that has
                // merely not finished opening yet is "nothing right now" — the
                // same as a socket read timeout — and reporting EOF there would
                // kill the session before the daemon has said anything.
                if (closed) return -1
                return 0
            }
            if (frame.isEmpty()) {
                failure?.let { throw it }
                return -1
            }
            leftover = frame
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

    override fun close() {
        open = false
        closed = true
        socket?.close(CLOSE_NORMAL, null)
        socket = null
        // A read parked on the queue learns the session ended instead of waiting
        // out its poll window.
        inbound.offer(END_OF_STREAM)
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
         * The tab metadata this client uses: what the tab is called, and the grid
         * its terminal is using.
         *
         * One frame carries both, so one type does — see [decodeFrame]. Either
         * field is null when the daemon did not send it in a form this client
         * understands, which is not an error: the two are useful independently.
         */
        data class Meta(val name: String?, val grid: Grid?) : Frame {
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
        return if (name == null && grid == null) Frame.Ignored else Frame.Meta(name, grid)
    }

    private fun send(tag: Byte, payload: ByteArray) {
        val socket = socket ?: return
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

    override fun isConnected(): Boolean = socket != null && open && !closed

    override fun isSessionOpen(): Boolean = open && !closed

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
         * How long [connect] waits for the daemon to accept the WebSocket before
         * giving up.
         *
         * Comfortably longer than the client's own connect timeout, because what
         * is being waited for is not just the TCP connection: the daemon
         * authenticates the request and attaches to the tab's PTY as well, and a
         * tab that is busy spawning should still open.
         */
        private const val CONNECT_TIMEOUT_SECONDS = 15L

        /** How long a read waits before reporting "nothing yet" to the caller. */
        private const val READ_POLL_MILLIS = 250L

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
