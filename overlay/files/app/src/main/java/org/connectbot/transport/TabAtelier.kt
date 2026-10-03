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
// TabAtelierClient, and opening a tab's terminal over the daemon's WebSocket is
// stage 2, so connect() below is an honest stub rather than a terminal
// implementation. See overlay/README.md.

package org.connectbot.transport

import android.content.Context
import android.net.Uri
import org.connectbot.R
import org.connectbot.data.entity.Host
import org.connectbot.service.DisconnectReason
import java.io.IOException

/**
 * A tab-atelier daemon: it serves a list of terminal tabs over HTTP +
 * WebSocket, and each tab is one terminal session on the workstation.
 *
 * Stage 1 only needs this class to exist and be honest: the type shows up in
 * the editor, and the host list lists the daemon's tabs, but nothing here opens
 * a session yet.
 */
class TabAtelier : AbsTransport() {

    override fun connect() {
        // TODO(stage 2): open the selected tab's terminal over the daemon's WebSocket.
        bridge?.outputLine(manager?.res?.getString(R.string.tabatelier_transport_not_yet))
        bridge?.dispatchDisconnect(DisconnectReason.REMOTE_EOF)
    }

    @Throws(IOException::class)
    override fun read(buffer: ByteArray, offset: Int, length: Int): Int = throw IOException("tab-atelier sessions are not implemented yet")

    @Throws(IOException::class)
    override fun write(buffer: ByteArray) = Unit

    @Throws(IOException::class)
    override fun write(c: Int) = Unit

    @Throws(IOException::class)
    override fun flush() = Unit

    override fun close() = Unit

    override fun setDimensions(columns: Int, rows: Int, width: Int, height: Int) = Unit

    override fun isConnected(): Boolean = false

    override fun isSessionOpen(): Boolean = false

    override fun getDefaultPort(): Int = DEFAULT_PORT

    override fun getDefaultNickname(username: String?, hostname: String?, port: Int): String = getUri(hostname).toString()

    override fun getSelectionArgs(uri: Uri, selection: MutableMap<String, String>) {
        selection["protocol"] = PROTOCOL
        selection["hostname"] = uri.host ?: ""
        selection["port"] = uri.port.toString()
    }

    override fun createHost(uri: Uri): Host {
        val hostname = uri.host ?: ""
        val nickname = uri.fragment?.takeIf { it.isNotEmpty() } ?: getDefaultNickname(null, hostname, uri.port)
        return Host(
            nickname = nickname,
            protocol = PROTOCOL,
            username = "",
            hostname = hostname,
            port = if (uri.port > 0) uri.port else DEFAULT_PORT,
        )
    }

    override fun usesNetwork(): Boolean = true

    override fun getLocalIpAddress(): String? = null

    companion object {
        const val PROTOCOL = "tabatelier"
        const val DEFAULT_PORT = 443

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
