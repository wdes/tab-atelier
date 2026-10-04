/**
 * Changed for Tab Atelier Remote (Apache-2.0 section 4(b)): new file, ours —
 * upstream has no tab-atelier server type, so there is nothing to preserve here.
 *
 * Three kinds of test in one class.
 *
 * The URL, JSON and header tests start an HTTP server on loopback themselves, so
 * they run everywhere, CI included, and they pin the things the user reported
 * broken: a server addressed by URL (scheme, port and path prefix), plain http
 * being possible at all, the list ordered most-recently-used first, and the
 * User-Agent identifying us the way the retired client did.
 *
 * The trust tests ask the trust manager itself which certificate it may use and
 * on what evidence — the decision that matters, and the one a live server cannot
 * exercise both halves of, since a fixture cannot be a public CA. See
 * `theDeviceDecidesWhatIsPinned` below.
 *
 * The TLS test is end to end over a socket and needs a real daemon, so it skips
 * unless `TABATELIER_TEST_URL` (and optionally `TABATELIER_TEST_TOKEN`) is set. A
 * test that only passes on one machine is worse than no test, so CI skips it; run
 * it with:
 *
 *     TABATELIER_TEST_URL=https://192.168.1.15:7891 \
 *     TABATELIER_TEST_TOKEN=$(cat ~/.local/state/tab-atelier/api.token) \
 *     ./gradlew :app:testOssDebugUnitTest --tests '*TabAtelierClientTest*'
 *
 * A daemon whose certificate the device itself validates — a Cloudflare Origin
 * certificate with its CA installed — is *not pinned* on purpose, so that test
 * asserts whichever of the two cases the server it was pointed at is in.
 */
package org.connectbot.tabatelier

import android.content.Context
import android.util.Base64
import androidx.test.core.app.ApplicationProvider
import com.sun.net.httpserver.HttpServer
import okhttp3.CertificatePinner
import org.connectbot.BuildConfig
import org.connectbot.util.SecurePasswordStorage
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import java.io.ByteArrayInputStream
import java.net.InetSocketAddress
import java.security.cert.CertificateException
import java.security.cert.CertificateFactory
import java.security.cert.X509Certificate
import java.util.concurrent.atomic.AtomicReference
import javax.net.ssl.HostnameVerifier
import javax.net.ssl.X509TrustManager

@RunWith(RobolectricTestRunner::class)
class TabAtelierClientTest {

    private val context: Context
        get() = ApplicationProvider.getApplicationContext()

    private fun client() = TabAtelierClient(context, SecurePasswordStorage(context))

    // ---------------- the URL the editor stores ----------------

    @Test
    fun baseFillsInThePortFromTheScheme() {
        val c = client()
        assertEquals(443, c.base("https://host.example")!!.port)
        assertEquals(80, c.base("http://host.example")!!.port)
        assertEquals(8443, c.base("https://host.example:8443")!!.port)
    }

    @Test
    fun baseKeepsAPathPrefixAndJoinsItWithOneSlash() {
        val c = client()
        assertEquals("https://host.example/tabs", c.base("https://host.example")!!.tabsUrl)
        assertEquals("https://host.example/prefix/tabs", c.base("https://host.example/prefix")!!.tabsUrl)
        assertEquals(
            "a stored trailing slash must not double",
            "https://host.example/prefix/tabs",
            c.base("https://host.example/prefix/")!!.tabsUrl,
        )
    }

    @Test
    fun baseRejectsWhatIsNotAServerUrl() {
        val c = client()
        val rejected = listOf(
            null, "", "   ",
            "host.example", "host.example:7890", "ftp://host.example",
            "https://", "https://host.example:0", "https://host.example:99999",
        )
        for (bad in rejected) {
            assertNull("should be rejected: $bad", c.base(bad))
        }
    }

    @Test
    fun httpIsNotSilentlyUpgradedToHttps() {
        val c = client()
        assertTrue(c.base("https://host.example")!!.secure)
        assertFalse("a LAN daemon over http must stay http", c.base("http://host.example:7890")!!.secure)
    }

    @Test
    fun webSocketUrlFollowsTheSameBaseAsHttp() {
        val c = client()
        assertEquals("wss://host.example/x", c.base("https://host.example")!!.webSocketUrl("/x"))
        assertEquals("ws://host.example:7890/x", c.base("http://host.example:7890")!!.webSocketUrl("/x"))
        assertEquals("wss://host.example/prefix/x", c.base("https://host.example/prefix")!!.webSocketUrl("/x"))
    }

    // ---------------- which certificate may be trusted, and on what evidence ----------------

    /**
     * The decision this app makes about a certificate, asked of the thing that
     * makes it — no socket, because which of the two cases a server is in is
     * decided by the device's own trust store, and no test can install a CA
     * there that a public server would have. Here the device's answer is the
     * fixture, and the subject under test is what the app does with it.
     */

    @Test
    fun aChainTheDeviceValidatesIsNeverPinned() {
        val hostId = 7L
        val base = client().base("https://host.example")!!
        val key = pinKeyOf(hostId, base)
        // A pin left over from when nothing could vouch for this server. It has
        // to go: the reason to pin was that the key was the only identity, and
        // that is no longer the case — so a renewal must not be refused.
        pins().edit().putString(key, tamperedPin()).commit()

        var remembered: X509Certificate? = null
        var forgotten = false
        val manager = TrustOnFirstUseManager(
            hostId = hostId,
            base = base,
            pins = pins(),
            device = DeviceTrustManager(validates = true),
            deviceHostnameVerifier = HostnameVerifier { _, _ -> true },
            rememberAsFirstUse = { remembered = it },
            forgetPinnedKey = {
                forgotten = true
                pins().edit().remove(key).commit()
            },
        )

        manager.checkServerTrusted(arrayOf(fixtureCertificate()), "EC")

        assertNull("a certificate the device validates must not be handed on as a pin", remembered)
        assertTrue("the pin of a server the device validates must be dropped", forgotten)
        assertNull("the pin must be gone, not merely ignored", pins().getString(key, null))
    }

    @Test
    fun aChainTheDeviceRejectsIsPinnedOnFirstUse() {
        val hostId = 8L
        val base = client().base("https://host.example")!!
        val certificate = fixtureCertificate()
        var remembered: X509Certificate? = null

        val manager = TrustOnFirstUseManager(
            hostId = hostId,
            base = base,
            pins = pins(),
            device = DeviceTrustManager(validates = false),
            deviceHostnameVerifier = HostnameVerifier { _, _ -> true },
            rememberAsFirstUse = { remembered = it },
            forgetPinnedKey = {},
        )

        // Nothing may be thrown: this is the connection that earns the pin, and
        // refusing it is the bug that made a self-signed server unreachable.
        manager.checkServerTrusted(arrayOf(certificate), "EC")

        assertSame("the key has to be handed on, or nothing is ever pinned", certificate, remembered)
    }

    @Test
    fun aPinnedKeyIsAcceptedAndAChangedOneIsRefused() {
        val hostId = 9L
        val base = client().base("https://host.example")!!
        val certificate = fixtureCertificate()
        pins().edit().putString(pinKeyOf(hostId, base), CertificatePinner.pin(certificate)).commit()

        val manager = TrustOnFirstUseManager(
            hostId = hostId,
            base = base,
            pins = pins(),
            device = DeviceTrustManager(validates = false),
            deviceHostnameVerifier = HostnameVerifier { _, _ -> true },
            rememberAsFirstUse = {},
            forgetPinnedKey = {},
        )

        manager.checkServerTrusted(arrayOf(certificate), "EC")

        // The recorded pin is only ever the value CertificatePinner produces, so
        // a pin cannot fail to verify against the certificate it came from.
        assertTrue(manager.matchesPin(listOf(certificate)))

        pins().edit().putString(pinKeyOf(hostId, base), tamperedPin()).commit()
        val failure = runCatching { manager.checkServerTrusted(arrayOf(certificate), "EC") }
            .exceptionOrNull()
        assertTrue(
            "a key that is not the pinned one must fail during the handshake, was: $failure",
            failure is CertificateException,
        )
        assertFalse("and must not be accepted by the verifier either", manager.matchesPin(listOf(certificate)))
    }

    /**
     * Building a client for an https server must work on the device, because it
     * reaches for two platform facilities — the device's X.509 trust manager and
     * its default hostname verifier — that are the app's only source of "is this
     * certificate one the device can vouch for". A platform that did not provide
     * one would fail this at first use of any https server, which is a crash the
     * user would see as the app being unable to talk to a server at all.
     *
     * The plain-http client is built too, since it must *not* touch TLS at all.
     */
    @Test
    fun aClientBuildsForBothAValidatedAndAPlainServer() {
        val c = client()
        assertNotNull(c.clientFor(1L, c.base("https://host.example")!!))
        assertNotNull(c.clientFor(2L, c.base("http://host.example:7890")!!))
    }

    private fun pins() = context.getSharedPreferences("tabatelier_host_pins", Context.MODE_PRIVATE)

    private fun pinKeyOf(hostId: Long, base: TabAtelierBase) = "pin_$hostId@${base.origin}"

    private fun tamperedPin(): String = "sha256/" + Base64.encodeToString(ByteArray(32) { 0x42.toByte() }, Base64.NO_WRAP)

    /** The device's trust store, with the answer the test is about. */
    private class DeviceTrustManager(private val validates: Boolean) : X509TrustManager {
        override fun checkServerTrusted(chain: Array<out X509Certificate>, authType: String) {
            if (!validates) throw CertificateException("not a chain the device's store validates")
        }

        override fun checkClientTrusted(chain: Array<out X509Certificate>, authType: String) =
            throw CertificateException("unused: this side is always the client")

        override fun getAcceptedIssuers(): Array<X509Certificate> = emptyArray()
    }

    /**
     * A self-signed certificate, as a fixture.
     *
     * Nothing on the unit-test classpath can mint one — no CA, no BouncyCastle —
     * so it is a fixed DER blob parsed by the platform's own certificate parser.
     * It is a fixture and not a secret: nothing trusts it, and nothing signs with
     * its key (which is discarded).
     */
    private fun fixtureCertificate(): X509Certificate {
        val der = Base64.decode(FIXTURE_CERTIFICATE_DER, Base64.DEFAULT)
        return CertificateFactory.getInstance("X.509")
            .generateCertificate(ByteArrayInputStream(der)) as X509Certificate
    }

    // ---------------- against a daemon this test starts ----------------

    @Test
    fun fetchesOverPlainHttp_sortsNewestFirst_andSaysWhoItIs() {
        val body = """
            {"tabs":[
              {"id":"seen-long-ago","name":"second","preview":"older output","active":false,"last_used_at":1000},
              {"id":"seen-last","name":"first","preview":"newest output","active":true,"last_used_at":2000},
              {"id":"never","name":"third","preview":"","active":false}
            ]}
        """.trimIndent()

        withServer(body) { url, headers ->
            val c = client()
            val base = c.base(url)!!
            assertFalse(base.secure)

            val tabs = c.fetchTabs(hostId = 1L, base = base, token = "secret-token")

            // The original complaint: the list has to be ordered by last use, and
            // a tab the API omits a timestamp for (never viewed) belongs last.
            assertEquals(
                listOf("first", "second", "third"),
                tabs.map { it.name },
            )
            assertEquals(2000L, tabs.first().lastUsedAt)
            assertEquals(0L, tabs.last().lastUsedAt)

            // The daemon does not branch on the request's User-Agent, but we
            // identify ourselves the way the retired client did so its logs stay
            // comparable.
            assertEquals("ta-remote/${BuildConfig.VERSION_NAME} (Android)", headers()["user-agent"])
            assertEquals("Bearer secret-token", headers()["authorization"])
        }
    }

    @Test
    fun anUnauthorizedResponseIsAnErrorAndNotAnEmptyList() {
        withServer(status = 401, body = """{"error":"unauthorized"}""") { url, _ ->
            val c = client()
            val failure = runCatching { c.fetchTabs(1L, c.base(url)!!, "wrong-token") }.exceptionOrNull()
            assertTrue(
                "a rejected token must not render as 'this server has no tabs': $failure",
                failure is IllegalStateException,
            )
        }
    }

    // ---------------- the reported bug: TLS with a self-signed certificate ----------------

    /**
     * The reported bug, end to end: a self-signed certificate was rejected
     * outright. The first connection is now accepted, and — when nothing but the
     * key identifies the server — that key is recorded and required afterwards,
     * with a *different* key refused rather than trusted.
     *
     * Each step evicts the connection pool first. Without that OkHttp reuses the
     * established TLS connection and no handshake happens, so the pin would never
     * be consulted and this test would pass while proving nothing.
     */
    @Test
    fun selfSignedIsAcceptedOnFirstUseThenPinnedAndAChangedKeyIsRefused() {
        val url = System.getenv("TABATELIER_TEST_URL")
        assumeTrue(
            "set TABATELIER_TEST_URL to a tab-atelier server over https:// to run this",
            url != null,
        )
        val token = System.getenv("TABATELIER_TEST_TOKEN")
        val hostId = 4242L
        val c = client()
        val base = c.base(url)!!
        assertTrue("this test is about TLS, so $url must be https", base.secure)

        // Start from "never seen this server", so a previous run's tampered pin
        // cannot leak into this one.
        c.clearPin(hostId)

        // 1. First use. This is the case that used to fail with
        //    CERTIFICATE_VERIFY_FAILED, because OkHttp's default trust manager
        //    checked the chain against the system CA store before our verifier
        //    was ever consulted. It is accepted now either because the device
        //    validates the certificate, or on first use.
        val first = c.fetchTabs(hostId, base, token)
        assertTrue("expected at least one tab from $url", first.isNotEmpty())

        // 2. Whether anything is pinned is the decision this client makes, and
        //    it depends on the server, so the rest of the test follows it instead
        //    of assuming one of the two.
        val pin = c.pin(hostId, base)
        if (pin == null) {
            // The device validated this server's certificate: a CA signed it, or
            // the user installed its CA — a Cloudflare Origin certificate with
            // its CA installed, through the documented deployment. Pinning it
            // would be the bug, since a renewal would then be refused. The only
            // thing left to assert is that a fresh handshake still succeeds.
            c.clientFor(hostId, base).connectionPool.evictAll()
            assertEquals(
                "a device-validated certificate must be accepted again, and never pinned",
                first.size,
                c.fetchTabs(hostId, base, token).size,
            )
            return
        }
        assertTrue(
            "the pin must be in the form a pin is compared in, was $pin",
            pin.startsWith("sha256/"),
        )

        // 3. The recorded key is accepted on a fresh handshake.
        c.clientFor(hostId, base).connectionPool.evictAll()
        assertEquals(
            "the pinned key must be accepted again",
            first.size,
            c.fetchTabs(hostId, base, token).size,
        )

        // 4. A changed key is refused. Tampering with the recorded pin is the
        //    only honest way to simulate this without a second certificate: the
        //    handshake now presents a key that does not match what was pinned.
        tamperWithRecordedPins()
        c.clientFor(hostId, base).connectionPool.evictAll()
        val failure = runCatching { c.fetchTabs(hostId, base, token) }.exceptionOrNull()
        assertNotNull("a changed key must fail the connection, not be trusted", failure)
        val messages = generateSequence(failure) { it.cause }.mapNotNull { it.message }.toList()
        // Deliberately loose about the wording and strict about the substance:
        // the connection must be refused, and the refusal must be about this
        // host rather than an empty result or a timeout.
        assertTrue(
            "a changed key must be refused in a way that names the host, was: $messages",
            messages.any { it.contains(base.host) },
        )
    }

    /** Replaces every recorded pin with one no real certificate can match. */
    private fun tamperWithRecordedPins() {
        val keys = pins().all.keys.filter { it.startsWith("pin_") }
        assertTrue("expected a recorded pin to tamper with", keys.isNotEmpty())
        pins().edit().apply { keys.forEach { putString(it, tamperedPin()) } }.commit()
    }

    /**
     * Serves one response at `/tabs` on loopback, and runs [block] with the URL
     * it can be reached at plus the headers that request carried.
     *
     * The headers are a function rather than a value because they only exist once
     * the request has been made — inside [block].
     */
    private fun withServer(
        body: String,
        status: Int = 200,
        block: (url: String, headers: () -> Map<String, String>) -> Unit,
    ) {
        val server = HttpServer.create(InetSocketAddress("127.0.0.1", 0), 0)
        val seen = AtomicReference<Map<String, String>>(emptyMap())
        server.createContext("/tabs") { exchange ->
            // Keyed in lower case: com.sun.net.httpserver.Headers normalises
            // names, so "User-Agent" arrives as "user-agent" and a
            // case-sensitive lookup would never find it.
            seen.set(exchange.requestHeaders.entries.associate { it.key.lowercase() to it.value.first() })
            val bytes = body.toByteArray()
            exchange.responseHeaders.add("Content-Type", "application/json")
            exchange.sendResponseHeaders(status, bytes.size.toLong())
            exchange.responseBody.use { it.write(bytes) }
            exchange.close()
        }
        server.start()
        try {
            block("http://127.0.0.1:${server.address.port}") { seen.get() }
        } finally {
            server.stop(0)
        }
    }

    private companion object {
        /**
         * The DER of [fixtureCertificate], base64. A self-signed P-256
         * certificate for `ta-test-fixture`, whose private key was discarded
         * when it was made — see the method for why it is a fixed blob.
         */
        const val FIXTURE_CERTIFICATE_DER = """
            MIIBpDCCAUugAwIBAgIUEuR1hXpgK6imVNI6DWux6GlosKAwCgYIKoZIzj0EAwIwGjEYMBYGA1UE
            AwwPdGEtdGVzdC1maXh0dXJlMB4XDTI2MTAwNDA3Mzk1N1oXDTM2MTAwMTA3Mzk1N1owGjEYMBYG
            A1UEAwwPdGEtdGVzdC1maXh0dXJlMFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEM4uivSdfevTV
            8H7JzUoAHCWso6CxL9dnwFEoMIdsW6YCsAIcV5dOaqe6DgEtgYhr2K9TiKkSoVn88hcvCajSgaNv
            MG0wHQYDVR0OBBYEFL/2tj/FISEL313eQcMv1VU6LQtTMB8GA1UdIwQYMBaAFL/2tj/FISEL313e
            QcMv1VU6LQtTMA8GA1UdEwEB/wQFMAMBAf8wGgYDVR0RBBMwEYIPdGEtdGVzdC1maXh0dXJlMAoG
            CCqGSM49BAMCA0cAMEQCIH6CDNvr9cTaDtGQow6Knq/U6jFSTFXGxmXqljMqGpjhAiADOCpYemnP
            Kz8ZZBUVgPtPG9jPW5+h1rdlBAMk7SWJIg==
        """
    }
}
