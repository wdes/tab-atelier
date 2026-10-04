/**
 * New file for Tab Atelier Remote (Apache-2.0 section 4(b)), ours — upstream has
 * no tab-atelier transport, so there is nothing to preserve here.
 *
 * Covers the frame decoder, which is where the daemon's stream is turned into
 * what the terminal renders. It is a pure function of a frame, so it is tested
 * directly rather than through a session.
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
            TabAtelier.Frame.Meta(name = null, grid = TabAtelier.Frame.Meta.Grid(rows = 48, cols = 193)),
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
            TabAtelier.Frame.Meta(name = "build-2", grid = null),
            transport.decodeFrame(frame(0x03, """{"name":"build-2"}""")),
        )
        assertEquals(
            "both, when the daemon sends both",
            TabAtelier.Frame.Meta(name = "build-2", grid = TabAtelier.Frame.Meta.Grid(rows = 48, cols = 193)),
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
            TabAtelier.Frame.Meta(name = "tab", grid = null),
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
}
