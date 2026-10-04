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
import org.connectbot.tabatelier.TabAtelierClient
import org.connectbot.util.SecurePasswordStorage
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

    private var pendingColumns = 0
    private var pendingRows = 0

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

        // Which tab, and only just decided: taken rather than read, so a later
        // session cannot inherit it from this one.
        val tabKey = service.takePendingTabKey(host.id)
        if (tabKey.isNullOrBlank()) {
            bridge?.outputLine(service.getString(R.string.tabatelier_tab_not_chosen))
            bridge?.dispatchDisconnect(DisconnectReason.REMOTE_EOF)
            return
        }

        val token = client.token(host.id)
        // The token rides in the query string, not an Authorization header: the
        // daemon refuses the header alone on a WS upgrade. See
        // TabAtelierBase.webSocketUrl for the why.
        val url = base.webSocketUrl("/tabs/by-id/$tabKey/ws", token)
        val request = Request.Builder().url(url).build()

        // Wait for the daemon to accept the connection, the way Telnet waits for
        // its socket, before telling the bridge the session is up.
        //
        // This is not merely tidy: `bridge.onConnected()` is what creates the
        // Relay, which is what reads this transport. Calling it before the
        // handshake would start the relay on a socket that is not open yet, and
        // the terminal would sit on "connecting" forever — the failure the user
        // saw. It is also why this method may block: `connect()` is called on
        // the io dispatcher (TerminalBridge.startConnection), never on the UI
        // thread.
        val opened = CountDownLatch(1)
        Timber.d("Opening tab-atelier session for host %d at %s", host.id, base.origin)
        val webSocket = client.clientFor(host.id, base).newWebSocket(request, Listener(opened))
        socket = webSocket

        if (!opened.await(CONNECT_TIMEOUT_SECONDS, TimeUnit.SECONDS)) {
            webSocket.cancel()
            socket = null
            throw IOException("Timed out connecting to $url")
        }
        // A socket that failed or closed before opening is a failed connection,
        // not a session that ends immediately: the bridge has to hear about it
        // as such, so the user gets the reason instead of an empty terminal.
        failure?.let { throw it }
        if (!open) {
            throw IOException("The tab-atelier session at $url closed before it opened")
        }

        bridge?.onConnected()
    }

    private inner class Listener(private val opened: CountDownLatch) : WebSocketListener() {

        override fun onOpen(webSocket: WebSocket, response: Response) {
            // Adopted here as well as by connect(), because this can run before
            // newWebSocket returns and the first frames below must not be
            // dropped for want of a socket to send them on.
            socket = webSocket
            open = true
            // The daemon stamps the tab's last_used_at on this frame, which is
            // what puts the tab the user just opened at the top of the list.
            webSocket.send(byteArrayOf(TAG_FOCUS).toByteString())
            sendDimensions()
            opened.countDown()
        }

        override fun onMessage(webSocket: WebSocket, bytes: ByteString) {
            val frame = bytes.toByteArray()
            if (frame.isEmpty()) return
            val payload = when (frame[0]) {
                TAG_OUTPUT -> frame.copyOfRange(1, frame.size)
                TAG_OUTPUT_GZIP -> gunzip(frame, 1)
                // Already renderable text, sent ahead of a large catch-up burst so
                // a phone paints the last screen immediately instead of waiting for
                // the whole scrollback.
                TAG_PREVIEW -> frame.copyOfRange(1, frame.size)
                // Tab metadata (name, size, agent state). The list already shows
                // it and the terminal does not need it, so this is deliberately
                // ignored rather than half-parsed.
                TAG_META -> return
                // Unknown tags are dropped, not fatal: a newer daemon must not be
                // able to break an older client.
                else -> {
                    Timber.d("Ignoring unknown tab-atelier frame tag %d", frame[0].toInt())
                    return
                }
            }
            if (payload.isNotEmpty()) inbound.offer(payload)
        }

        // The daemon sends no text frames; a session that does is not one we
        // understand, and the comment is here so the unused parameter is not
        // mistaken for an oversight.
        override fun onMessage(webSocket: WebSocket, text: String) = Unit

        override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
            Timber.d("tab-atelier session closed: %d %s", code, reason)
            closed = true
            inbound.offer(END_OF_STREAM)
            opened.countDown()
        }

        override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
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

    override fun setDimensions(columns: Int, rows: Int, width: Int, height: Int) {
        pendingColumns = columns
        pendingRows = rows
        sendDimensions()
    }

    /**
     * Tells the daemon the size this phone is rendering at.
     *
     * The payload is JSON, not the two big-endian shorts it looks like it ought
     * to be: the daemon parses `{"cols":N,"rows":M}`.
     *
     * It is sent knowing the current daemon ignores it — resizing a tab is a
     * documented no-op in v1, because a tab is a real workstation terminal with a
     * real size and the desktop wins. So a phone renders the tab's output at its
     * own width, with the wrapping the workstation chose. Sending it anyway costs
     * one frame and means the day the daemon honours it, this already works.
     */
    private fun sendDimensions() {
        if (!open || pendingColumns <= 0 || pendingRows <= 0) return
        send(TAG_RESIZE, """{"cols":$pendingColumns,"rows":$pendingRows}""".toByteArray())
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
         */
        private const val TAG_INPUT: Byte = 0x01
        private const val TAG_OUTPUT: Byte = 0x02
        private const val TAG_META: Byte = 0x03
        private const val TAG_RESIZE: Byte = 0x04
        private const val TAG_OUTPUT_GZIP: Byte = 0x0A
        private const val TAG_FOCUS: Byte = 0x0B
        private const val TAG_PREVIEW: Byte = 0x0C

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
