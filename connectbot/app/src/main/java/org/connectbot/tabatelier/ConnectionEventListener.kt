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

package org.connectbot.tabatelier

import okhttp3.Call
import okhttp3.EventListener
import okhttp3.Handshake
import okhttp3.Protocol
import okhttp3.Request
import okhttp3.Response
import org.connectbot.R
import timber.log.Timber
import java.io.IOException
import java.net.InetAddress
import java.net.InetSocketAddress
import java.net.Proxy

/**
 * Turns OkHttp's own idea of what a call is doing into the phases [ConnectionProgress] writes.
 *
 * The point of using OkHttp's events rather than instrumenting the call by hand is honesty:
 * these are the events the client actually experienced, so "TCP connection to …" appearing
 * means a TCP connection was being opened, not that the app guessed one probably was. It is
 * also the only way to know the resolved address, which is the one piece of information that
 * tells a DNS problem apart from a routing one.
 *
 * **This listener stops at the upgrade.** The WebSocket's own lifecycle — open, the daemon's
 * meta frame, the scrollback replay — is the socket's business, not OkHttp's, and is reported
 * from `TabAtelier.Listener`. In particular [connectionAcquired] is *not* used: it is a
 * connection-*pool* event that fires before the request is even written, so it never meant
 * what its old message ("WebSocket connection open") claimed.
 *
 * Every callback is a no-op without a [progress], because the client this is attached to is
 * cached and shared: one built during a dial keeps its factory afterwards, and a later call
 * on it — a tab switch, a retry — attaches its own.
 */
internal class ConnectionEventListener(
    private val progress: ConnectionProgress?,
) : EventListener() {

    /**
     * DNS is the phase that most often needs explaining.
     *
     * A name that resolves slowly, or to an address on the wrong network, produces "nothing
     * happens" for several seconds and no error at all.
     */
    override fun dnsStart(call: Call, domainName: String) {
        progress?.phase(R.string.tabatelier_phase_dns, domainName)
    }

    /**
     * The resolved addresses, which is what makes a slow resolver distinguishable from a slow
     * server: the phase either names an address and moves on, or does not.
     */
    override fun dnsEnd(call: Call, domainName: String, inetAddressList: List<InetAddress>) {
        val addresses = inetAddressList.joinToString(", ") { it.hostAddress.orEmpty() }
        if (addresses.isEmpty()) {
            // OkHttp reports an empty list when the lookup produced nothing usable, which is a
            // different failure from the lookup throwing: one is a broken resolver, the other
            // a name with no records.
            progress?.phase(R.string.tabatelier_phase_dns_empty, domainName)
        } else {
            progress?.phase(R.string.tabatelier_phase_dns_resolved, domainName, addresses)
        }
    }

    /**
     * TCP, including which address is being tried.
     *
     * A host with several addresses tries them in turn, and a connection that spends four
     * seconds on each of three addresses is a different problem from one that spends twelve on
     * a single address.
     */
    override fun connectStart(call: Call, inetSocketAddress: InetSocketAddress, proxy: Proxy) {
        progress?.phase(R.string.tabatelier_phase_tcp, inetSocketAddress.describe())
    }

    override fun connectEnd(
        call: Call,
        inetSocketAddress: InetSocketAddress,
        proxy: Proxy,
        protocol: Protocol?,
    ) {
        progress?.phase(R.string.tabatelier_phase_tcp_ok, inetSocketAddress.describe())
    }

    override fun connectFailed(
        call: Call,
        inetSocketAddress: InetSocketAddress,
        proxy: Proxy,
        protocol: Protocol?,
        ioe: IOException,
    ) {
        // Named with the address that refused, so a user diagnosing a wrong LAN address sees
        // which one rather than a generic error.
        progress?.phase(
            R.string.tabatelier_phase_tcp_failed,
            inetSocketAddress.describe(),
            ioe.summary(),
        )
    }

    override fun secureConnectStart(call: Call) {
        progress?.phase(R.string.tabatelier_phase_tls, call.resolvedHost())
    }

    override fun secureConnectEnd(call: Call, handshake: Handshake?) {
        val protocol = handshake?.tlsVersion?.javaName ?: "TLS"
        progress?.phase(R.string.tabatelier_phase_tls_ok, call.resolvedHost(), protocol)
    }

    /**
     * The upgrade request is being written: the step that was missing.
     *
     * Reported from [requestHeadersStart] rather than from a socket callback because this is
     * the last thing OkHttp knows before the connection stops being HTTP and becomes a
     * WebSocket. Everything after it — whether the daemon accepted, which tab, how much
     * scrollback — is reported by the socket.
     */
    override fun requestHeadersEnd(call: Call, request: Request) {
        progress?.phase(R.string.tabatelier_phase_ws_opening)
    }

    /**
     * The daemon's answer to the upgrade.
     *
     * A WebSocket upgrade answers **101**, which is outside `200..299` — so the previous
     * version, which only spoke up outside that range, would have printed a successful upgrade
     * as "Server answered 101 Switching Protocols". Anything else is the daemon (or a proxy in
     * front of it) refusing, and is worth saying plainly: it is not a network problem.
     */
    override fun responseHeadersEnd(call: Call, response: Response) {
        when (upgradeOutcome(response.code)) {
            // Deliberately silent. The socket's own `onOpen` is the authority for "WebSocket
            // open", and it fires a moment later: announcing it here as well would show the
            // line twice, and the first time would be *before* the socket could be written to.
            UpgradeOutcome.SUCCESS -> Unit
            UpgradeOutcome.REFUSED ->
                progress?.phase(
                    R.string.tabatelier_phase_ws_refused,
                    "${response.code} ${response.message}".trim(),
                )
            // A 2xx to an upgrade request is the server answering instead of upgrading —
            // something in front of the daemon, most likely. Nothing to announce: the socket
            // will fail next and say so.
            UpgradeOutcome.IGNORED -> Unit
        }
    }

    override fun callFailed(call: Call, ioe: IOException) {
        // The summary only: OkHttp's messages already name the address, and a stack trace
        // belongs in the log, not on the strip the user is watching.
        Timber.w(ioe, "tab-atelier call failed")
    }

    /** What an upgrade response status means for the strip. */
    internal enum class UpgradeOutcome {
        /** The WebSocket is open. */
        SUCCESS,

        /** The daemon answered, and not with an upgrade. */
        REFUSED,

        /** Still HTTP: 2xx means the request was answered, not upgraded. */
        IGNORED,
    }

    /** The remote's host, for messages that name who we are talking to. */
    private fun Call.resolvedHost(): String = request().url.host

    private fun InetSocketAddress.describe(): String =
        "${address?.hostAddress ?: hostString}:$port"

    /** OkHttp's message without a class-name prefix. */
    private fun IOException.summary(): String = message?.takeIf { it.isNotBlank() } ?: "failed"

    companion object {
        /**
         * A status that answers "did the WebSocket open?".
         *
         * Extracted and pure so it has a test: the 101 case is the one that used to be wrong,
         * and it cannot be exercised through a real upgrade without a live daemon.
         */
        internal fun upgradeOutcome(status: Int): UpgradeOutcome = when {
            // The RFC 6455 answer to `Upgrade: websocket`.
            status == 101 -> UpgradeOutcome.SUCCESS
            status in 200..299 -> UpgradeOutcome.IGNORED
            else -> UpgradeOutcome.REFUSED
        }
    }
}
