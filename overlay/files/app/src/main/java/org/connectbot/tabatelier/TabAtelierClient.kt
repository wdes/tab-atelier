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

// New file for Tab Atelier Remote, not part of upstream ConnectBot: the URL a
// tab-atelier server is addressed by, the HTTP client for its `GET {base}/tabs`,
// and the trust-on-first-use pinning of its certificate. See overlay/README.md.

package org.connectbot.tabatelier

import android.content.Context
import android.content.SharedPreferences
import dagger.hilt.android.qualifiers.ApplicationContext
import okhttp3.CertificatePinner
import okhttp3.HttpUrl
import okhttp3.HttpUrl.Companion.toHttpUrlOrNull
import okhttp3.Interceptor
import okhttp3.OkHttpClient
import okhttp3.Request
import org.connectbot.BuildConfig
import org.connectbot.util.SecurePasswordStorage
import timber.log.Timber
import java.security.cert.Certificate
import java.security.cert.CertificateException
import java.security.cert.X509Certificate
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.TimeUnit
import javax.inject.Inject
import javax.inject.Singleton
import javax.net.ssl.KeyManager
import javax.net.ssl.SSLContext
import javax.net.ssl.SSLPeerUnverifiedException
import javax.net.ssl.TrustManager
import javax.net.ssl.X509TrustManager

/**
 * A tab-atelier daemon's base URL, parsed once.
 *
 * A server is one URL — `https://host`, `http://host:7890`,
 * `https://host:8443/prefix` — because a scheme and a path prefix are exactly
 * what the internal protocol/hostname/port columns cannot express. The scheme
 * decides whether the daemon is reached over TLS or plain HTTP; the path prefix
 * is the mount point the daemon serves under.
 *
 * Stage 2's terminal WebSocket hangs off the same base, so deriving a URL from
 * the base lives here rather than at every call site.
 */
class TabAtelierBase private constructor(private val url: HttpUrl) {
    /** `http` or `https`. */
    val scheme: String = url.scheme

    val host: String = url.host

    /** The port actually dialled: 443 or 80 was filled in from [scheme]. */
    val port: Int = url.port

    /**
     * The path prefix, as an encoded path with no trailing slash: `""` for a
     * daemon at the root of its host, `/prefix` for one behind a path.
     */
    val prefix: String = url.encodedPath.trimEnd('/')

    /** Whether this base is reached over TLS, which is what decides pinning. */
    val secure: Boolean get() = scheme == HTTPS_SCHEME

    /**
     * `scheme://host:port` — the connection a certificate pin belongs to, and
     * the name it is recorded under. No prefix: one daemon serves one key at
     * every path it is mounted under.
     */
    val origin: String = url.newBuilder()
        .encodedPath(ROOT_PATH)
        .build()
        .toString()
        .removeSuffix(ROOT_PATH)

    /** `GET {this}/tabs`, with exactly one slash between prefix and path. */
    val tabsUrl: String get() = httpUrl(TABS_PATH).toString()

    /**
     * [path] on this server, e.g. `https://host:8443/prefix/tabs`. [path] starts
     * with `/`; the prefix is joined to it without doubling or dropping a slash.
     */
    fun httpUrl(path: String): HttpUrl = url.newBuilder().encodedPath(prefix + path).build()

    /**
     * [path] on this server over WebSocket, e.g. `wss://host:8443/prefix/x`:
     * stage 2's terminal, built from the same base as [httpUrl].
     *
     * A string rather than an [HttpUrl], which accepts only http and https —
     * `HttpUrl.Builder.scheme("wss")` throws, so the scheme is swapped in the
     * canonical URL [httpUrl] produced.
     */
    fun webSocketUrl(path: String): String {
        val http = httpUrl(path).toString()
        return webSocketScheme + http.substring(scheme.length)
    }

    private val webSocketScheme: String get() = if (secure) "wss" else "ws"

    /**
     * The canonical base, e.g. `https://host:8443` or `https://host:8443/prefix`
     * — default port elided, no trailing slash. This is what the editor stores
     * and the host list shows.
     */
    override fun toString(): String = url.newBuilder()
        .encodedPath(prefix.ifEmpty { ROOT_PATH })
        .build()
        .toString()
        .removeSuffix(ROOT_PATH)

    override fun equals(other: Any?): Boolean = other is TabAtelierBase && other.url == url

    override fun hashCode(): Int = url.hashCode()

    companion object {
        private const val HTTP_SCHEME = "http"
        private const val HTTPS_SCHEME = "https"
        private const val ROOT_PATH = "/"
        private const val TABS_PATH = "/tabs"

        /**
         * Parse a server's base URL, or null if it cannot be one.
         *
         * Rejected, all for the same reason — quietly dropping part of what the
         * user typed is worse than saying no:
         * - what [HttpUrl] itself refuses: no scheme, a scheme other than
         *   http/https, no host, a port outside 1..65535, a malformed URL;
         * - credentials, a query or a fragment, which a daemon address has no
         *   use for and which the request would have to discard.
         *
         * Everything else is kept: the scheme chooses TLS or not, the port
         * defaults to 443/80, and a path prefix is preserved.
         */
        fun parse(input: String?): TabAtelierBase? {
            val url = input?.trim()?.toHttpUrlOrNull() ?: return null
            if (url.username.isNotEmpty() || url.password.isNotEmpty()) return null
            if (url.query != null || url.fragment != null) return null
            return TabAtelierBase(url)
        }
    }
}

/**
 * Talks to a tab-atelier daemon.
 *
 * Stage 1 uses exactly one endpoint, `GET {base}/tabs`; a tab's terminal is a
 * WebSocket and belongs to stage 2, which asks [clientFor] for the client and
 * [TabAtelierBase.webSocketUrl] for the URL rather than deriving either again.
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
     * A client per host *and* origin, so a pin learned for one server is
     * enforced on that server's own calls and never weakens another's. The
     * origin is part of the key because it is what a pin is recorded against:
     * editing a host's URL changes the origin, and the client for the old one is
     * dropped rather than reused.
     */
    private data class ClientKey(val hostId: Long, val origin: String)

    private val clients = ConcurrentHashMap<ClientKey, OkHttpClient>()

    /**
     * Keys accepted as first use, before they have earned being pinned.
     *
     * The trust manager is where a key is actually seen and verified, so it is
     * where the candidate comes from. `Response.handshake` would have been the
     * other source, and it was the original one — but it is not always
     * populated, and when it is absent nothing was ever recorded. That is how
     * trust-on-first-use quietly became "trust everything, forever": the accept
     * happened, the record did not.
     *
     * A candidate becomes a pin only once a request has come back over it.
     */
    private val pendingPins = ConcurrentHashMap<String, String>()

    /** The base URL a host is addressed by, or null if what it holds is not one. */
    fun base(url: String?): TabAtelierBase? = TabAtelierBase.parse(url)

    /**
     * The token a host's API calls carry, from ConnectBot's Keystore-backed
     * per-host secret store — the same store SSH passwords use. It is
     * deliberately outside the database, so it is not in exports or backups.
     */
    fun token(hostId: Long): String? = securePasswordStorage.getPassword(hostId)

    fun saveToken(hostId: Long, token: String?) = securePasswordStorage.savePassword(hostId, token)

    /**
     * Whether a token is stored for a host, without decrypting it.
     *
     * One half of what decides whether a server needs probing again — adding or
     * clearing a token changes how the daemon answers — so it is asked on every
     * hosts-Flow emission and stays a cheap lookup.
     */
    fun hasToken(hostId: Long): Boolean = securePasswordStorage.hasPassword(hostId)

    /** The SPKI pin recorded for a host's connection, if it has one. */
    fun pin(hostId: Long, base: TabAtelierBase): String? =
        basePrefs.getString(pinKey(hostId, base.origin), null)

    /** Forget a host's pins, so the next successful call learns them again. */
    fun clearPin(hostId: Long) {
        val prefix = pinPrefix(hostId)
        val recorded = basePrefs.all.keys.filter { it.startsWith(prefix) }
        basePrefs.edit().apply { recorded.forEach { remove(it) } }.apply()
        pendingPins.keys.filter { it.startsWith(prefix) }.forEach(pendingPins::remove)
        clients.keys.filter { it.hostId == hostId }.forEach(clients::remove)
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
    fun fetchTabs(hostId: Long, base: TabAtelierBase, token: String?): List<TabAtelierTab> {
        val request = Request.Builder()
            .url(base.tabsUrl)
            .apply { if (!token.isNullOrEmpty()) header("Authorization", "Bearer $token") }
            .build()

        val response = clientFor(hostId, base).newCall(request).execute()
        return response.use {
            if (!it.isSuccessful) {
                throw IllegalStateException("GET ${base.tabsUrl} returned HTTP ${it.code}")
            }
            // The key is only recorded once a request has come back over the
            // accepted connection, never merely because the socket was accepted:
            // see recordPin. A plain-http daemon has no handshake, and so
            // nothing to pin.
            if (base.secure) {
                recordPin(hostId, base, it.handshake?.peerCertificates)
            }
            parseTabAtelierTabs(it.body.string())
        }
    }

    /**
     * The client for a server: built on first use for a host and origin, and
     * configured for that server alone. Stage 2 reuses it for the WebSocket,
     * which is what carries the User-Agent and the pin there too.
     */
    fun clientFor(hostId: Long, base: TabAtelierBase): OkHttpClient {
        val key = ClientKey(hostId, base.origin)
        // The address is part of the key, so editing a host's URL leaves the
        // client built for its old address unused; drop it, the way the address
        // it was built for was dropped.
        clients.keys.filter { it.hostId == hostId && it != key }.forEach(clients::remove)
        return clients.computeIfAbsent(key) { buildClient(hostId, base) }
    }

    private fun buildClient(hostId: Long, base: TabAtelierBase): OkHttpClient {
        val builder = OkHttpClient.Builder()
            .connectTimeout(CONNECT_TIMEOUT_SECONDS, TimeUnit.SECONDS)
            .readTimeout(READ_TIMEOUT_SECONDS, TimeUnit.SECONDS)
            .callTimeout(CALL_TIMEOUT_SECONDS, TimeUnit.SECONDS)
            .addInterceptor(userAgentInterceptor)

        // A daemon serving plain HTTP — a trusted LAN, or a tunnel that brings
        // its own encryption — is reached with no TLS configuration at all: no
        // pin, no trust manager, no hostname verifier. It is deliberately not
        // upgraded to https, which would only fail against a daemon with no
        // certificate to offer.
        if (!base.secure) return builder.build()

        val trustManager = TrustOnFirstUseManager(hostId, base, basePrefs) { accepted ->
            // Remembered here, pinned later: fetchTabs commits it once a
            // response has come back over this key. pinOf is what turns the
            // certificate into the form CertificatePinner compares, so the
            // candidate is stored in that same form.
            pinOf(accepted)?.let { pendingPins[pinKey(hostId, base.origin)] = it }
        }
        val sslContext = SSLContext.getInstance(TLS_PROTOCOL).apply {
            init(null as Array<KeyManager>?, arrayOf<TrustManager>(trustManager), null)
        }

        builder
            // The daemon's certificate is self-signed or a Cloudflare Origin
            // certificate, so neither the system CA store nor name validation
            // can be the gate. The gate is this trust manager, which decides on
            // the certificate's public key: trust on first use, then require
            // that exact key. Setting only a hostname verifier — as this client
            // did before the trust manager was added — is not enough: OkHttp's
            // default trust manager rejects the chain during the handshake,
            // before any verifier is consulted, so a self-signed server never
            // reaches the pin at all.
            .sslSocketFactory(sslContext.socketFactory, trustManager)
            // The second, independent check that it is the same connection: it
            // compares the pin, and so cannot pass a key the trust manager would
            // have rejected.
            .hostnameVerifier { _, session ->
                val chain = try {
                    session.peerCertificates
                } catch (e: SSLPeerUnverifiedException) {
                    null
                }
                trustManager.matchesPin(chain?.toList())
            }

        // Belt and braces: OkHttp's CertificatePinner pins the Subject Public
        // Key Info rather than the certificate (so a renewal that keeps the key
        // still matches while a swapped key does not), and it reports the
        // failure as an SSLPeerUnverifiedException naming the pin.
        pin(hostId, base)?.let { recorded ->
            builder.certificatePinner(
                CertificatePinner.Builder()
                    .add(base.host, recorded)
                    .build(),
            )
        }
        return builder.build()
    }

    /**
     * Record the leaf certificate's SPKI pin the first time a response arrives
     * from this host's origin. Later connections must reproduce it.
     *
     * The first handshake is *accepted* before anything is recorded, because the
     * request that earns the pin cannot be made otherwise; what makes that safe
     * is that the acceptance only buys that one request, and the pin is written
     * only once its response has come back. A server that is not the daemon —
     * anything that answers and then fails, or is refused a response — never
     * becomes the pinned key, and every later connection to it is rejected
     * against the pin the real daemon earned.
     */
    private fun recordPin(hostId: Long, base: TabAtelierBase, chain: List<Certificate>?) {
        val key = pinKey(hostId, base.origin)
        val candidate = pendingPins.remove(key) ?: return
        // The handshake's own view of the same connection is only used to
        // disagree: a pin that is not the key just used is worse than no pin.
        val fromHandshake = chain?.firstOrNull()?.let(::pinOf)
        if (fromHandshake != null && fromHandshake != candidate) {
            Timber.w(
                "Not pinning %s: the handshake key is not the one the trust manager accepted",
                base.origin,
            )
            return
        }
        if (pin(hostId, base) != null) return
        basePrefs.edit().putString(key, candidate).apply()
        Timber.d("Recorded tab-atelier certificate pin for host %d at %s", hostId, base.origin)
    }

    companion object {
        private const val PREFS_FILE_NAME = "tabatelier_host_pins"

        private const val CONNECT_TIMEOUT_SECONDS = 10L
        private const val READ_TIMEOUT_SECONDS = 15L
        private const val CALL_TIMEOUT_SECONDS = 20L
        private const val TLS_PROTOCOL = "TLS"

        /**
         * How this app names itself to a daemon: `ta-remote/0.6.10 (Android)`,
         * the shape the retired Slint client sent, so a daemon's logs read the
         * same across both clients. The daemon does not branch on it — the
         * `app` field of its own `/tabs` response is the daemon's User-Agent,
         * not ours — so this is about identifying ourselves honestly in logs.
         */
        private val userAgent: String = "ta-remote/${BuildConfig.VERSION_NAME} (Android)"

        /**
         * Sent on every request, so the WebSocket upgrade of stage 2 carries it
         * too without having to remember to.
         */
        private val userAgentInterceptor = Interceptor { chain ->
            chain.proceed(
                chain.request().newBuilder().header("User-Agent", userAgent).build(),
            )
        }
    }
}

/**
 * Decides whether a daemon's certificate may be trusted, on the certificate's
 * public key rather than on a CA or a name.
 *
 * First use of a host is accepted, so that a request can be made at all; the key
 * is remembered later, and only once a response has arrived (see
 * [TabAtelierClient.recordPin]). Every later connection must reproduce it. A
 * host whose recorded key no longer matches fails here, during the handshake,
 * with a message naming it — never by quietly adopting the new key. "No pin" is
 * only ever the first-use case from this side: a recorded pin that cannot be
 * read back is a corrupt store, not a reason to trust everything.
 */
private class TrustOnFirstUseManager(
    private val hostId: Long,
    private val base: TabAtelierBase,
    private val pins: SharedPreferences,
    private val rememberAsFirstUse: (X509Certificate) -> Unit,
) : X509TrustManager {

    override fun checkServerTrusted(chain: Array<out X509Certificate>, authType: String) {
        val expected = recordedPin()
        if (expected == null) {
            // Nothing pinned for this server yet: accept, so the request that
            // earns the pin can be made. Accepting is not recording — the key is
            // handed on as a candidate, and only a response makes it a pin.
            Timber.d("First tab-atelier connection to %s; accepting its key for now", base.origin)
            chain.firstOrNull()?.let(rememberAsFirstUse)
            return
        }

        val leaf = chain.firstOrNull()
            ?: throw CertificateException("No certificate from ${base.host} to compare with its pinned key")

        if (pinOf(leaf) != expected) {
            throw CertificateException(
                "The certificate key of ${base.host} does not match the key pinned for it. " +
                    "Its certificate was replaced, or this is not the server that was trusted; " +
                    "clear the host's trust to accept the new key.",
            )
        }
    }

    override fun checkClientTrusted(chain: Array<out X509Certificate>, authType: String) {
        throw CertificateException("A tab-atelier host is a client, not the server being checked")
    }

    override fun getAcceptedIssuers(): Array<X509Certificate> = emptyArray()

    /**
     * Whether the session over [chain] may be used.
     *
     * **True when nothing is pinned**, and that is load-bearing rather than
     * lenient. This backs the hostname verifier, and OkHttp *enforces* a
     * hostname verifier's answer: answering false for "not pinned yet" fails the
     * very connection whose response would have recorded the pin. That is
     * circular — no server could ever be reached for the first time, which is
     * exactly the bug this class exists to fix, only reported as "hostname not
     * verified" instead of "self-signed certificate". The trust manager has
     * already accepted the key as first use by this point; the verifier's job is
     * only to refuse a *changed* key, not to re-litigate a new one.
     *
     * A pin's authority comes from being written after a response has come back
     * over the key (see `recordPin`), never from this check.
     *
     * False when a pin is recorded and [chain] does not reproduce it: the key
     * changed, and the connection must not be made.
     */
    fun matchesPin(chain: List<Certificate>?): Boolean {
        val expected = recordedPin() ?: return true
        val leaf = chain?.firstOrNull() ?: return false
        return pinOf(leaf) == expected
    }

    private fun recordedPin(): String? = pins.getString(pinKey(hostId, base.origin), null)
}

/** The preferences key one host's pin for one origin is recorded under. */
private fun pinKey(hostId: Long, origin: String): String = "${pinPrefix(hostId)}$origin"

/** Every pin key of one host, whatever address it was learned at. */
private fun pinPrefix(hostId: Long): String = "pin_$hostId@"

/**
 * The OkHttp pin string for a certificate. Delegating to
 * [CertificatePinner.pin] is deliberate: the value recorded is then exactly what
 * [CertificatePinner] compares against, so a pin we recorded cannot fail to
 * verify against itself.
 */
private fun pinOf(certificate: Certificate): String? =
    (certificate as? X509Certificate)?.let { CertificatePinner.pin(it) }

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
