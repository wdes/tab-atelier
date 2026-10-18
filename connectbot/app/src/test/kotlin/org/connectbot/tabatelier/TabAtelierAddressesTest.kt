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

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Covers the two pure rules behind fallback addresses: which order to try them in, and
 * which ones count as being on a local network.
 *
 * Both are the sort of rule that is easy to write plausibly and impossible to check by
 * looking at it — "local addresses go first, unless it's not local, in which case the
 * remote one goes first, unless the local one failed, in which case…" — so they are
 * asserted directly. Nothing here opens a socket.
 */
class TabAtelierAddressesTest {

    private val remote = "https://home.example.com:8443"
    private val lan = "https://192.168.1.10:8443"
    private val otherLan = "http://10.0.0.5:8443"

    /**
     * The LAN preference: on a local network, the local address is tried first.
     *
     * This is the whole point of the feature. Without it, opening a tab at home waits for
     * a connect timeout against the remote address before falling back to a LAN address
     * that was reachable all along.
     */
    @Test
    fun onALocalNetwork_theLocalAddressGoesFirst() {
        val addresses = TabAtelierAddresses(
            primary = remote,
            fallbacks = listOf(lan, otherLan),
            local = setOf(lan, otherLan),
        )

        assertEquals(listOf(lan, otherLan, remote), addresses.candidates(onLocalNetwork = true))
    }

    /**
     * Off a local network, the configured order is kept.
     *
     * A LAN address is unreachable from a train, so trying it first would cost a timeout
     * for nothing — but it is still *tried*, because the phone's idea of "local" is a hint
     * and the network's is the truth.
     */
    @Test
    fun offALocalNetwork_theConfiguredOrderIsKept() {
        val addresses = TabAtelierAddresses(
            primary = remote,
            fallbacks = listOf(lan, otherLan),
            local = setOf(lan, otherLan),
        )

        assertEquals(
            listOf(remote, lan, otherLan),
            addresses.candidates(onLocalNetwork = false),
        )
    }

    /**
     * A local address that has stopped answering falls through to the remote one.
     *
     * Ordering and not filtering, and this is the assertion that says so: being on a LAN
     * whose daemon is down must still reach the server, so the remote address has to be
     * in the list even when a local one is preferred.
     */
    @Test
    fun aPreferredLocalAddress_doesNotHideTheRemoteOne() {
        val addresses = TabAtelierAddresses(
            primary = remote,
            fallbacks = listOf(lan),
            local = setOf(lan),
        )

        val candidates = addresses.candidates(onLocalNetwork = true)

        assertEquals(listOf(lan, remote), candidates)
        assertTrue("the remote address must still be tried", candidates.contains(remote))
    }

    /**
     * The user's order is kept within each group.
     *
     * Two LAN addresses with a preference between them must not be re-sorted by anything
     * here — the only thing that reorders is the local/remote split.
     */
    @Test
    fun theTypedOrderIsKeptWithinEachGroup() {
        val addresses = TabAtelierAddresses(
            primary = remote,
            fallbacks = listOf(otherLan, lan),
            local = setOf(lan, otherLan),
        )

        assertEquals(
            listOf(otherLan, lan, remote),
            addresses.candidates(onLocalNetwork = true),
        )
    }

    /**
     * Blank entries are dropped, so a trailing comma cannot become an address that is
     * dialled and fails.
     */
    @Test
    fun blankFallbacksAreDropped() {
        assertEquals(
            listOf(remote, lan),
            TabAtelierAddresses.parseFallbacks("  $remote ,, $lan , "),
        )
        assertEquals(emptyList<String>(), TabAtelierAddresses.parseFallbacks(null))
        assertEquals(emptyList<String>(), TabAtelierAddresses.parseFallbacks("   "))
    }

    /**
     * A blank primary is not an address, even though it is stored as one.
     *
     * Every host row has a tabatelier_url column, and a blank one means "not configured"
     * rather than "the empty address" — it must not appear in the list and be dialled.
     */
    @Test
    fun aBlankPrimaryIsNotAnAddress() {
        val addresses = TabAtelierAddresses(primary = "  ", fallbacks = listOf(lan))

        assertEquals(listOf(lan), addresses.all)
    }

    /**
     * The private ranges are recognised, and public addresses are not.
     *
     * Both halves matter, and the second more: putting a public address first because it
     * "looked local" would delay every single connect from outside the LAN.
     */
    @Test
    fun thePrivateRangesAreLocal() {
        listOf(
            "http://10.0.0.5:8443",
            "https://192.168.1.10:8443",
            "https://172.16.0.1:8443",
            "https://172.31.255.254",
            "http://127.0.0.1:8443",
            "http://169.254.10.1",
            "http://nas.local:8443",
            "https://LOCALHOST:8443",
            "192.168.1.10",
            "https://192.168.1.10/some/path",
        ).forEach { address ->
            assertTrue("$address should be treated as local", TabAtelierAddresses.isLocalAddress(address))
        }

        listOf(
            "https://home.example.com:8443",
            "https://8.8.8.8",
            "https://172.15.0.1",
            "https://172.32.0.1",
            "https://11.0.0.1",
            "https://193.168.1.10",
            "",
            "https://",
            "not a url",
            "https://256.1.1.1",
        ).forEach { address ->
            assertFalse("$address should not be treated as local", TabAtelierAddresses.isLocalAddress(address))
        }
    }

    /**
     * An address that normalises differently from how it was typed is still keyed by what was
     * typed — and nothing throws.
     *
     * This is a regression test for a crash, not a style assertion. An earlier version looked
     * a parsed address up with `bases.first { it.origin == candidate }`, comparing a
     * *normalised* origin against the *raw* string the user typed. Any host written with a
     * trailing slash, an uppercase scheme or a default port made that `first` throw
     * `NoSuchElementException`, and the connection died with it — so the app was unusable for
     * exactly the addresses that are most natural to type.
     *
     * Asserted on the keys, because the keys are what the ordering code walks.
     */
    @Test
    fun addressesAreKeyedByWhatWasTypedEvenWhenTheyNormalise() {
        val typed = listOf(
            "https://Home.Example:8443/",
            "HTTPS://192.168.1.10:8443/",
            "http://nas.local:80",
        )
        val addresses = TabAtelierAddresses(primary = typed[0], fallbacks = typed.drop(1))

        val parsed = addresses.parsed()

        assertEquals(typed, parsed.keys.toList())
        // And every one of them parsed, so none is silently dropped from the ordering.
        assertEquals(typed.size, parsed.size)
        // The parsed forms are normalised, which is precisely why they were the wrong thing
        // to compare the typed strings against.
        assertTrue(parsed.values.first().origin != typed[0])
    }

    /**
     * An address that cannot be parsed is dropped from the pairing but does not throw.
     *
     * The editor refuses to save one, so this is the case of a row written by an older
     * version or edited behind the app's back: skipping it is right, crashing is not.
     */
    @Test
    fun unparseableAddressesAreSkippedWhenPairing() {
        val addresses = TabAtelierAddresses(
            primary = "https://192.168.1.10:8443",
            fallbacks = listOf("not a url", "https://home.example:8443"),
        )

        val parsed = addresses.parsed()

        assertEquals(listOf("https://192.168.1.10:8443", "https://home.example:8443"), parsed.keys.toList())
    }

    /**
     * A hostname is never guessed to be local, because it cannot be known.
     *
     * `nas.example` resolves to whatever the current network says it does, so the honest
     * answer is "no opinion" and the configured order stands. Guessing here would be worse
     * than not guessing: a name that resolves publicly would be pinned to the front of the
     * list everywhere.
     */
    @Test
    fun aPlainHostnameIsNotAssumedLocal() {
        assertFalse(TabAtelierAddresses.isLocalAddress("https://nas.example:8443"))
        assertFalse(TabAtelierAddresses.isLocalAddress("https://nas.example.com:8443"))
    }
}
