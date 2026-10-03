/**
 * Changed for Tab Atelier Remote (Apache-2.0 section 4(b)): new file, ours —
 * upstream has no tab-atelier server type, so there is nothing to preserve here.
 *
 * Two kinds of test in one class.
 *
 * The URL, JSON and header tests start an HTTP server on loopback themselves, so
 * they run everywhere, CI included, and they pin the things the user reported
 * broken: a server addressed by URL (scheme, port and path prefix), plain http
 * being possible at all, the list ordered most-recently-used first, and the
 * User-Agent identifying us the way the retired client did.
 *
 * The TLS test is the reported bug itself — a self-signed certificate was
 * rejected outright — but it needs a real daemon over https, so it skips unless
 * `TABATELIER_TEST_URL` (and optionally `TABATELIER_TEST_TOKEN`) is set. A test
 * that only passes on one machine is worse than no test, so CI skips it; run it
 * with:
 *
 *     TABATELIER_TEST_URL=https://192.168.1.15:7891 \
 *     TABATELIER_TEST_TOKEN=$(cat ~/.local/state/tab-atelier/api.token) \
 *     ./gradlew :app:testOssDebugUnitTest --tests '*TabAtelierClientTest*'
 */
package org.connectbot.tabatelier

import android.content.Context
import android.util.Base64
import androidx.test.core.app.ApplicationProvider
import com.sun.net.httpserver.HttpServer
import org.connectbot.BuildConfig
import org.connectbot.util.SecurePasswordStorage
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import java.net.InetSocketAddress
import java.util.concurrent.atomic.AtomicReference

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
     * Trust on first use, end to end: the first connection to a server whose
     * certificate is self-signed is accepted, the key it used is recorded, a
     * later connection over that same key is accepted, and a *different* key is
     * refused rather than trusted.
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
        //    was ever consulted.
        val first = c.fetchTabs(hostId, base, token)
        assertTrue("expected at least one tab from $url", first.isNotEmpty())

        // 2. The key is recorded only now that a request has come back over it.
        val pin = c.pin(hostId, base)
        assertNotNull(
            "a successful https call must record the key it was made over. " +
                "origin=${base.origin} recorded=$pin " +
                "prefs=" + context.getSharedPreferences("tabatelier_host_pins", Context.MODE_PRIVATE).all,
            pin,
        )
        assertTrue(
            "the pin must be in the form CertificatePinner compares, was $pin",
            pin!!.startsWith("sha256/"),
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
        //
        // Which message arrives is OkHttp's choice, and it is not the one we
        // would pick. Two checks can refuse a changed key — the trust manager
        // (whose message says the certificate key was replaced, and what to do
        // about it) and the hostname verifier — and OkHttp reports a false
        // verifier answer as "Hostname <host> not verified", which is the one
        // that surfaces. Asserting on "pin" here would be asserting on wording we
        // do not control; making the better message surface is a separate job.
        assertTrue(
            "a changed key must be refused in a way that names the host, was: $messages",
            messages.any { it.contains(base.host) },
        )
    }

    /** Replaces every recorded pin with one no real key can match. */
    private fun tamperWithRecordedPins() {
        val prefs = context.getSharedPreferences("tabatelier_host_pins", Context.MODE_PRIVATE)
        val keys = prefs.all.keys.filter { it.startsWith("pin_") }
        assertTrue("expected a recorded pin to tamper with", keys.isNotEmpty())
        val bogus = "sha256/" + Base64.encodeToString(ByteArray(32) { 0x42.toByte() }, Base64.NO_WRAP)
        prefs.edit().apply { keys.forEach { putString(it, bogus) } }.commit()
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
}
