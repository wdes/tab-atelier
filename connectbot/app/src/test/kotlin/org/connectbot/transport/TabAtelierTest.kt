/**
 * New file for Tab Atelier Remote (Apache-2.0 section 4(b)), ours — upstream has
 * no tab-atelier transport, so there is nothing to preserve here.
 *
 * Covers the two things about this transport that are decided by a rule rather than
 * carried out by a socket: the frame decoder, which turns the daemon's stream into
 * what the terminal renders, and the reconnection policy, which decides what a lost
 * socket means and when to dial again.
 *
 * Both are pure functions of their arguments, so both are tested directly rather than
 * through a session — no server, no WebSocket, no coroutine. That is not only
 * convenience: the reconnection rule is the substance of the fix it belongs to, and
 * a rule that can only be exercised by reproducing a real network drop is a rule that
 * will not be exercised.
 *
 * The tag table it checks against is written out here rather than read from the
 * transport: this is the test's own statement of the daemon's protocol, so a
 * change to one side that the other does not match fails here instead of quietly
 * agreeing with itself.
 *
 * Robolectric is only needed for `org.json`, which the Android platform provides
 * and a plain JVM unit test does not.
 */
package org.connectbot.transport

import okio.ByteString.Companion.encodeUtf8
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import java.io.ByteArrayOutputStream
import java.util.zip.GZIPOutputStream

@RunWith(RobolectricTestRunner::class)
class TabAtelierTest {

    private val transport = TabAtelier()

    private fun frame(tag: Int, payload: String): ByteArray =
        byteArrayOf(tag.toByte()) + payload.toByteArray(Charsets.UTF_8)

    private fun gzip(payload: String): ByteArray {
        val out = ByteArrayOutputStream()
        GZIPOutputStream(out).use { it.write(payload.toByteArray(Charsets.UTF_8)) }
        return out.toByteArray()
    }

    @Test
    fun outputFrames_areHandedToTheTerminal() {
        val decoded = transport.decodeFrame(frame(0x02, "hello"))
        assertEquals(TabAtelier.Frame.Output("hello".encodeUtf8()), decoded)
    }

    /**
     * Gzipped output is *gzip*, not raw deflate. The tags look interchangeable and
     * are not: reading one as the other does not throw here, it paints garbage.
     */
    @Test
    fun gzippedOutputFrames_areExpanded() {
        val decoded = transport.decodeFrame(byteArrayOf(0x0A) + gzip("hello"))
        assertEquals("hello".encodeUtf8(), (decoded as TabAtelier.Frame.Output).bytes)
    }

    /**
     * The grid size, which is the whole reason this decoder exists.
     *
     * A tab is a real workstation terminal — the one this was built against is 193
     * columns — and the daemon refuses to resize it, so mirroring this is the only
     * way to render the tab's replay in the right places. Rendering at the phone's
     * own width is what made the session look like several terminals at once.
     */
    @Test
    fun metaFrames_carryTheGridToMirror() {
        val decoded = transport.decodeFrame(frame(0x03, """{"rows":48,"cols":193}"""))
        assertEquals(
            TabAtelier.Frame.Meta(name = null, grid = TabAtelier.Frame.Meta.Grid(rows = 48, cols = 193), bgColor = null),
            decoded,
        )
    }

    /**
     * The tab's name, which the console title shows beside the server's.
     *
     * It comes from the daemon rather than from the row that was tapped, so this
     * is the frame that has to carry it — and it is the same frame as the grid, so
     * one test each for "only a name", "only a grid" and "both".
     */
    @Test
    fun metaFrames_carryTheTabName() {
        assertEquals(
            "a name alone must still be a usable meta frame",
            TabAtelier.Frame.Meta(name = "build-2", grid = null, bgColor = null),
            transport.decodeFrame(frame(0x03, """{"name":"build-2"}""")),
        )
        assertEquals(
            "both, when the daemon sends both",
            TabAtelier.Frame.Meta(name = "build-2", grid = TabAtelier.Frame.Meta.Grid(rows = 48, cols = 193), bgColor = null),
            transport.decodeFrame(frame(0x03, """{"name":"build-2","rows":48,"cols":193}""")),
        )
    }

    /**
     * A name that is not a name must not reach the title.
     *
     * The console renders "server - name", so an empty or absent name would leave
     * a trailing separator with nothing after it, which reads as a rendering bug
     * rather than as a tab without a name. A blank one is therefore not a name
     * here, and the title falls back to the server's.
     */
    /**
     * The tab's own background, which the daemon uses to tell one tab's viewer from
     * another's.
     *
     * It matters that this is read and not dropped: the daemon keeps the terminal's
     * own colours — part of the tab — separate from this, so a viewer shows the tab
     * in its own colour without losing what the terminal renders with.
     */
    @Test
    fun metaFrames_carryTheTabBackground() {
        assertEquals(
            TabAtelier.Frame.Meta(
                name = null,
                grid = null,
                bgColor = "#002451",
            ),
            transport.decodeFrame(frame(0x03, """{"bg_color":"#002451"}""")),
        )
        assertEquals(
            "all three fields arrive in one frame, so all three must survive it",
            TabAtelier.Frame.Meta(
                name = "build-2",
                grid = TabAtelier.Frame.Meta.Grid(rows = 48, cols = 193),
                bgColor = "#451c2e",
            ),
            transport.decodeFrame(
                frame(0x03, """{"name":"build-2","rows":48,"cols":193,"bg_color":"#451c2e"}"""),
            ),
        )
    }

    /**
     * A background this client cannot use must leave the tab's usual one alone,
     * rather than be passed on to be rendered.
     *
     * The daemon's own browser client validates the same shape before applying one,
     * so a value that is not `#rrggbb` is a value neither end should act on — and
     * the failure mode of guessing is an unreadable terminal, which is worse than
     * ignoring a cosmetic field.
     */
    @Test
    fun anUnusableTabBackground_isDroppedNotRendered() {
        for (bad in listOf("#GGGGGG", "red", "#12345", "#1234567", "002451", "")) {
            val decoded = transport.decodeFrame(
                frame(0x03, """{"name":"tab","bg_color":"$bad"}"""),
            ) as TabAtelier.Frame.Meta
            assertEquals("a background of \"$bad\" is not a colour", null, decoded.bgColor)
            assertEquals("the rest of the frame must still be usable", "tab", decoded.name)
        }
        // JSON null, the daemon's sentinel for "none", is not the string "null".
        val nulled = transport.decodeFrame(frame(0x03, """{"name":"tab","bg_color":null}""")) as TabAtelier.Frame.Meta
        assertEquals(null, nulled.bgColor)
    }

    @Test
    fun metaFrames_withoutAUsableName_carryNoName() {
        // Nothing usable in the frame at all is Ignored, not an empty Meta: a
        // meta frame that says nothing this client can use has nothing to say.
        assertEquals(
            TabAtelier.Frame.Ignored,
            transport.decodeFrame(frame(0x03, """{"name":""}""")),
        )
        assertEquals(
            TabAtelier.Frame.Ignored,
            transport.decodeFrame(frame(0x03, """{"name":"   "}""")),
        )
        // The daemon's JSON null for "unknown" must not become the string "null".
        assertEquals(
            TabAtelier.Frame.Ignored,
            transport.decodeFrame(frame(0x03, """{"name":null}""")),
        )
    }

    @Test
    fun metaFrames_withoutAGrid_carryNoGrid() {
        // Zero and the daemon's JSON null for "unknown" are both "not a size", and
        // neither is an error: the name from the same frame is still usable.
        assertEquals(
            TabAtelier.Frame.Meta(name = "tab", grid = null, bgColor = null),
            transport.decodeFrame(frame(0x03, """{"name":"tab","agent_kind":"claude"}""")),
        )
        assertEquals(
            TabAtelier.Frame.Ignored,
            transport.decodeFrame(frame(0x03, """{"rows":null,"cols":null}""")),
        )
        assertEquals(
            TabAtelier.Frame.Ignored,
            transport.decodeFrame(frame(0x03, """{"rows":0,"cols":193}""")),
        )
    }

    @Test
    fun malformedMeta_doesNotThrow() {
        assertEquals(TabAtelier.Frame.Ignored, transport.decodeFrame(frame(0x03, "not json at all")))
        assertEquals(TabAtelier.Frame.Ignored, transport.decodeFrame(byteArrayOf(0x03)))
    }

    /**
     * The preview paint is not rendered, and this is the regression guard for it.
     *
     * Tag 0x0c carries the last screen as terminal output, sent ahead of the full
     * replay so a viewer paints something immediately. Feeding it to the emulator
     * concatenates it with the replay that follows and draws the same screen twice
     * — visible as a jumbled session. It is a hint, not a stream.
     */
    @Test
    fun previewFrames_areNotRendered() {
        assertEquals(
            TabAtelier.Frame.Ignored,
            transport.decodeFrame(frame(0x0C, "last screen as text")),
        )
    }

    @Test
    fun unknownTags_areIgnoredRatherThanFatal() {
        // A daemon that grows a frame must not be able to break an older client by
        // sending one it has not learned yet.
        assertEquals(TabAtelier.Frame.Ignored, transport.decodeFrame(frame(0x7F, "future")))
        assertEquals(TabAtelier.Frame.Ignored, transport.decodeFrame(ByteArray(0)))
    }

    /**
     * `setDimensions` must not tell the daemon anything.
     *
     * The daemon refuses to resize a tab on purpose — a phone viewer must not
     * reflow a shared PTY under an agent's TUI — and this protocol's reference
     * client mirrors the server's grid rather than driving it. A resize frame here
     * is what the daemon's own documentation calls a no-op it drops, so sending one
     * is at best pointless; asserting it is safe to call is the honest limit of
     * what a unit test can say about a deliberate no-op.
     */
    @Test
    fun setDimensions_isSafeAndSilent() {
        transport.setDimensions(columns = 45, rows = 20, width = 1080, height = 1920)
    }

    /**
     * A live session that loses its socket is reported *and* retried.
     *
     * This is the bug in one assertion. A dropped socket used to do neither: the
     * failure was swallowed, so the host list went on reading "connected" from
     * `disconnected == false`, and nothing dialled again. Getting either half of
     * this wrong is a different bug — retrying silently leaves a user staring at a
     * frozen terminal wondering why, and reporting without retrying is the
     * force-disconnect the user was stuck with.
     */
    @Test
    fun aLostSocketOnALiveSession_isReportedAndRetried() {
        assertEquals(
            TabAtelier.Reconnect.Action.RETRY_AND_REPORT,
            TabAtelier.Reconnect.decide(established = true, closed = false),
        )
    }

    /**
     * A session that never got a socket is retried without being announced.
     *
     * Before the first socket opens there is no session for the bridge to hear
     * about — it has not been given its `onConnected` yet — and the terminal is
     * already saying it is connecting. Reporting a failure there would be reporting
     * a session the app does not know it has.
     */
    @Test
    fun aLostSocketBeforeAnyOpened_isRetriedSilently() {
        assertEquals(
            TabAtelier.Reconnect.Action.RETRY_SILENTLY,
            TabAtelier.Reconnect.decide(established = false, closed = false),
        )
    }

    /**
     * A socket lost after the session was closed is not retried at all.
     *
     * `close()` detaches the socket before closing it, so its callback is normally
     * ignored by identity — but the callback and the close run on different threads,
     * and one that loses that race must still not redial. A retry here would
     * re-attach a viewer to the user's tab for a session the app has already ended:
     * a connection the phone has no record of, on the workstation's screen.
     */
    @Test
    fun aLostSocketAfterTheSessionEnded_isNotRetried() {
        assertEquals(
            TabAtelier.Reconnect.Action.GIVE_UP,
            TabAtelier.Reconnect.decide(established = false, closed = true),
        )
        assertEquals(
            TabAtelier.Reconnect.Action.GIVE_UP,
            TabAtelier.Reconnect.decide(established = true, closed = true),
        )
    }

    /**
     * The redial backs off, and then stops backing off.
     *
     * Two things have to be true and they are not the same: the wait must grow, so a
     * server that is down is not hammered; and it must stop growing, so a session
     * left on a dead server does not end up waiting hours. Both halves are asserted
     * because a schedule that only escalates passes a "is it backing off" test and
     * fails the user.
     */
    @Test
    fun theRedialBacksOffAndThenCaps() {
        val first = TabAtelier.Reconnect.delayMillis(0)
        val second = TabAtelier.Reconnect.delayMillis(1)
        val third = TabAtelier.Reconnect.delayMillis(2)

        assertTrue("the wait must grow: $first then $second", second > first)
        assertTrue("the wait must keep growing: $second then $third", third > second)

        // Past the end of the schedule the last value repeats, so a long outage settles
        // at a fixed interval rather than growing without bound.
        val atCap = TabAtelier.Reconnect.delayMillis(4)
        assertEquals(atCap, TabAtelier.Reconnect.delayMillis(5))
        assertEquals(atCap, TabAtelier.Reconnect.delayMillis(1_000))
        assertTrue("a settled interval must stay useful: $atCap", atCap <= 60_000L)
    }

    /**
     * The backoff index is clamped, not wrapped or thrown on.
     *
     * `delayMillis` is called with a counter that only ever climbs, so the argument
     * is a number the caller does not bound. An index into a fixed array has to
     * survive that: a negative index would come from a counter reset, and past-the-end
     * is the normal case after a few minutes of a dead server.
     */
    @Test
    fun theBackoffIndexIsClampedNotThrownOn() {
        assertTrue(TabAtelier.Reconnect.delayMillis(-1) >= 0)
        assertTrue(TabAtelier.Reconnect.delayMillis(Int.MAX_VALUE) > 0)
    }
}
