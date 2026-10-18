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
import okhttp3.Connection
import okhttp3.EventListener
import okhttp3.Handshake
import okhttp3.Protocol
import okhttp3.Request
import okhttp3.Response
import timber.log.Timber
import java.io.IOException
import java.net.InetAddress
import java.net.InetSocketAddress
import java.net.Proxy

/**
 * Turns OkHttp's own idea of what a call is doing into the phases [ConnectionProgress]
 * writes.
 *
 * The point of using OkHttp's events rather than instrumenting the call by hand is honesty:
 * these are the events the client actually experienced, so "TCP connection to …" appearing
 * means a TCP connection was being opened, not that the app guessed one probably was. It is
 * also the only way to know the resolved address, which is the one piece of information that
 * tells a DNS problem apart from a routing one.
 *
 * Every callback is a no-op without a [progress], because the client this is attached to is
 * cached and shared: one built during a dial keeps its factory afterwards, and a later call
 * on it — a tab switch, a retry — attaches its own.
 */
internal class ConnectionEventListener(
    private val progress: ConnectionProgress?,
    private val base: TabAtelierBase,
) : EventListener() {

    /** Whether the phases on screen have got as far as TLS, so a failure can say so. */
    @Volatile
    private var reachedTls = false

    /**
     * DNS is the phase that most often needs explaining.
     *
     * A name that resolves slowly, or to an address on the wrong network, produces "nothing
     * happens" for several seconds and no error at all — so the resolved addresses are named
     * as they arrive, which is what makes the difference between a slow resolver and a slow
     * server visible.
     */
    override fun dnsStart(call: Call, domainName: String) {
        progress?.phase("DNS lookup for $domainName")
    }

    override fun dnsEnd(call: Call, domainName: String, inetAddressList: List<InetAddress>) {
        val addresses = inetAddressList.joinToString(", ") { it.hostAddress.orEmpty() }
        if (addresses.isEmpty()) {
            // OkHttp reports an empty list when the lookup produced nothing usable, which
            // is a different failure from the lookup throwing — worth saying separately,
            // because one is a broken resolver and the other is a name with no records.
            progress?.phase("No address for $domainName")
        } else {
            progress?.phase("DNS lookup for $domainName = $addresses")
        }
    }

    /**
     * TCP, including which address is being tried.
     *
     * The address matters: a host with several addresses tries them in turn, and a connection
     * that spends four seconds on each of three addresses is a different problem from one
     * that spends twelve on a single address.
     */
    override fun connectStart(call: Call, inetSocketAddress: InetSocketAddress, proxy: Proxy) {
        val address = inetSocketAddress.address?.hostAddress ?: inetSocketAddress.hostString
        progress?.phase("TCP connection to $address:${inetSocketAddress.port}")
    }

    override fun connectEnd(
        call: Call,
        inetSocketAddress: InetSocketAddress,
        proxy: Proxy,
        protocol: Protocol?,
    ) {
        val address = inetSocketAddress.address?.hostAddress ?: inetSocketAddress.hostString
        progress?.phase("Connected to $address:${inetSocketAddress.port}")
    }

    override fun connectFailed(
        call: Call,
        inetSocketAddress: InetSocketAddress,
        proxy: Proxy,
        protocol: Protocol?,
        ioe: IOException,
    ) {
        val address = inetSocketAddress.address?.hostAddress ?: inetSocketAddress.hostString
        // Named as the failure it is, and with the address that refused — so a user
        // diagnosing a wrong LAN address sees which one, rather than a generic error.
        progress?.phase("Could not connect to $address:${inetSocketAddress.port}: ${ioe.summary()}")
    }

    override fun secureConnectStart(call: Call) {
        reachedTls = true
        progress?.phase("TLS handshake with ${base.host}")
    }

    override fun secureConnectEnd(call: Call, handshake: Handshake?) {
        val protocol = handshake?.tlsVersion?.javaName ?: "TLS"
        progress?.phase("TLS handshake with ${base.host} ($protocol)")
    }

    /**
     * The WebSocket upgrade — the last step, and the one that fails when the daemon is up but
     * refuses the token or the tab.
     */
    override fun responseHeadersEnd(call: Call, response: Response) {
        if (response.code !in 200..299) {
            // A daemon that answers 401 or 404 is not a network problem at all, and saying
            // so here is the whole reason this hook exists: without it the terminal would
            // show a completed TLS handshake and then nothing.
            progress?.phase("Server answered ${response.code} ${response.message}")
        }
    }

    override fun connectionAcquired(call: Call, connection: Connection) {
        progress?.phase("WebSocket connection open")
    }

    override fun callFailed(call: Call, ioe: IOException) {
        // The summary only: OkHttp's exception messages already name the address, and the
        // stack trace belongs in the log, not on the terminal the user is watching.
        Timber.w(ioe, "tab-atelier call failed")
    }

    /** OkHttp's message without the class name it is conventionally prefixed with. */
    private fun IOException.summary(): String = message?.takeIf { it.isNotBlank() } ?: "failed"
}
