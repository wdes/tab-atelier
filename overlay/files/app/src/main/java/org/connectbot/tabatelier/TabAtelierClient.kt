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

// New file for Tab Atelier Remote, not part of upstream ConnectBot: the HTTP
// client for a tab-atelier daemon's `GET {base}/tabs`, and the trust-on-first-
// use pinning of its certificate. See overlay/README.md.

package org.connectbot.tabatelier

import android.content.Context
import android.content.SharedPreferences
import dagger.hilt.android.qualifiers.ApplicationContext
import okhttp3.CertificatePinner
import okhttp3.OkHttpClient
import okhttp3.Request
import org.connectbot.util.SecurePasswordStorage
import timber.log.Timber
import java.security.cert.Certificate
import java.security.cert.X509Certificate
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.TimeUnit
import javax.inject.Inject
import javax.inject.Singleton
import javax.net.ssl.SSLPeerUnverifiedException

/**
 * Talks to a tab-atelier daemon.
 *
 * Stage 1 uses exactly one endpoint, `GET {base}/tabs`; a tab's terminal is a
 * WebSocket and belongs to stage 2.
 */
@Singleton
class TabAtelierClient @Inject constructor(
    @ApplicationContext private val context: Context,
    private val securePasswordStorage: SecurePasswordStorage,
) {
    private val basePrefs: SharedPreferences by lazy {
        context.getSharedPreferences(PREFS_FILE_NAME, Context.MODE_PRIVATE)
    }

    /**
     * A per-host client and the hostname it was built for, so a host's pin is
     * enforced on its own calls and a certificate learned for one server never
     * weakens another's.
     */
    private data class HostClient(val hostname: String, val client: OkHttpClient)

    private val clients = ConcurrentHashMap<Long, HostClient>()

    /**
     * The token a host's API calls carry, from ConnectBot's Keystore-backed
     * per-host secret store — the same store SSH passwords use. It is
     * deliberately outside the database, so it is not in exports or backups.
     */
    fun token(hostId: Long): String? = securePasswordStorage.getPassword(hostId)

    fun saveToken(hostId: Long, token: String?) = securePasswordStorage.savePassword(hostId, token)

    /** The SHA-256 Subject Public Key Info pin recorded for this host, if any. */
    fun pin(hostId: Long): String? = basePrefs.getString(pinKey(hostId), null)

    /** Forget a host's pin, so the next successful call learns it again. */
    fun clearPin(hostId: Long) {
        basePrefs.edit().remove(pinKey(hostId)).apply()
        clients.remove(hostId)
    }

    /** Forget a host's token and pin, for when the host itself is deleted. */
    fun forgetHost(hostId: Long) {
        securePasswordStorage.deletePassword(hostId)
        clearPin(hostId)
    }

    /**
     * `GET {base}/tabs`, sorted by `last_used_at` descending.
     *
     * Blocking network I/O: never call this on the main thread.
     *
     * @throws Exception on any network, TLS or parse failure.
     */
    fun fetchTabs(hostId: Long, hostname: String, port: Int, token: String?): List<TabAtelierTab> {
        val url = "https://$hostname:$port/tabs"
        val request = Request.Builder()
            .url(url)
            .apply { if (!token.isNullOrEmpty()) header("Authorization", "Bearer $token") }
            .build()

        val response = clientFor(hostId, hostname).newCall(request).execute()
        return response.use {
            if (!it.isSuccessful) {
                throw IllegalStateException("GET $url returned HTTP ${it.code}")
            }
            // The key is only recorded once a request has come back over the
            // pinned connection successfully, never merely because the socket
            // was accepted.
            recordPin(hostId, it.handshake?.peerCertificates)
            parseTabAtelierTabs(it.body.string())
        }
    }

    private fun clientFor(hostId: Long, hostname: String): OkHttpClient =
        clients.computeIfAbsent(hostId) { id -> HostClient(hostname, buildClient(id, hostname)) }
            // A hostname edit invalidates the client: the pin is registered
            // against the name it was learned from.
            .let { entry ->
                if (entry.hostname == hostname) {
                    entry.client
                } else {
                    val rebuilt = HostClient(hostname, buildClient(hostId, hostname))
                    clients[hostId] = rebuilt
                    rebuilt.client
                }
            }

    private fun buildClient(hostId: Long, hostname: String): OkHttpClient {
        val builder = OkHttpClient.Builder()
            .connectTimeout(CONNECT_TIMEOUT_SECONDS, TimeUnit.SECONDS)
            .readTimeout(READ_TIMEOUT_SECONDS, TimeUnit.SECONDS)
            .callTimeout(CALL_TIMEOUT_SECONDS, TimeUnit.SECONDS)
            // The daemon's certificate is self-signed or a Cloudflare Origin
            // certificate, so name validation cannot be the gate. The gate is
            // the key pin below: trust on first use, then require that exact
            // key. This is NOT a blanket trust-all — a certificate whose key
            // does not match the pin fails the call.
            .hostnameVerifier { _, session ->
                val chain = try {
                    session.peerCertificates
                } catch (e: SSLPeerUnverifiedException) {
                    null
                }
                pinMatches(hostId, chain?.toList())
            }

        // Belt and braces: OkHttp's CertificatePinner pins the Subject Public
        // Key Info rather than the certificate (so a renewal that keeps the key
        // still matches while a swapped key does not), and it reports the
        // failure as an SSLPeerUnverifiedException naming the pin.
        pin(hostId)?.let { recorded ->
            builder.certificatePinner(
                CertificatePinner.Builder()
                    .add(hostname, recorded)
                    .build(),
            )
        }
        return builder.build()
    }

    /**
     * Accept a certificate for host [hostId] only if it reproduces the pinned
     * key; the first certificate seen is accepted and recorded (trust on first
     * use).
     */
    private fun pinMatches(hostId: Long, chain: List<Certificate>?): Boolean {
        val expected = pin(hostId) ?: return true
        val leaf = chain?.firstOrNull() ?: return false
        return pinOf(leaf) == expected
    }

    /**
     * Record the leaf certificate's SPKI pin the first time a request succeeds
     * against this host. Later requests must reproduce it.
     */
    private fun recordPin(hostId: Long, chain: List<Certificate>?) {
        if (pin(hostId) != null) return
        val leaf = chain?.firstOrNull() as? X509Certificate ?: return
        basePrefs.edit().putString(pinKey(hostId), pinOf(leaf)).apply()
        Timber.d("Recorded tab-atelier certificate pin for host %d", hostId)
    }

    /**
     * The OkHttp pin string for a certificate. Delegating to
     * [CertificatePinner.pin] is deliberate: the value recorded here is then
     * exactly what [CertificatePinner] compares against, so a pin we recorded
     * cannot fail to verify against itself.
     */
    private fun pinOf(certificate: Certificate): String? =
        (certificate as? X509Certificate)?.let { CertificatePinner.pin(it) }

    private fun pinKey(hostId: Long): String = "$PIN_PREFIX$hostId"

    companion object {
        private const val PREFS_FILE_NAME = "tabatelier_host_pins"
        private const val PIN_PREFIX = "pin_"

        private const val CONNECT_TIMEOUT_SECONDS = 10L
        private const val READ_TIMEOUT_SECONDS = 15L
        private const val CALL_TIMEOUT_SECONDS = 20L
    }
}

/**
 * A user-facing explanation of a failed tab-atelier call. The token is never
 * part of the message.
 */
fun tabAtelierErrorMessage(e: Throwable): String = when (e) {
    is SSLPeerUnverifiedException ->
        "This server's certificate key does not match the key pinned for it. " +
            "Its certificate was replaced; clear the host's trust to accept the new key."

    else -> e.message ?: e.javaClass.simpleName
}
