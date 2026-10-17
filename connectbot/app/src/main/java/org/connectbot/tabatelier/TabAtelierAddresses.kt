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

/**
 * The addresses one tab-atelier server can be reached at, in the order they should be
 * tried *with nothing known about the network*.
 *
 * A single daemon is normally reachable two ways — a LAN address and a remote one — and
 * which is correct depends on where the phone is, not on anything the user wants to think
 * about. So the host row carries a primary address and any number of fallbacks, and this
 * type is the ordered set of them plus the rule for reordering it.
 *
 * [withLocalFirst] is where the LAN preference lives, and it is a pure function of the
 * set and one boolean so it can be tested without a socket, a network or a phone.
 *
 * Addresses are kept as the strings the user typed, not as parsed URLs, because the edit
 * screen has to show them back unchanged and a round trip through a parser would rewrite
 * them (trailing slashes, default ports, case in the host). Parsing happens per dial, in
 * [TabAtelierBase.parse], which is where a bad one has to be rejected anyway.
 */
data class TabAtelierAddresses(
    /** The address in [org.connectbot.data.entity.Host.tabAtelierUrl]. */
    val primary: String?,
    /** The addresses in [org.connectbot.data.entity.Host.tabAtelierUrls], in typed order. */
    val fallbacks: List<String>,
    /**
     * The local addresses among them, as things the user marked or that
     * [isLocalAddress] recognised when they were entered.
     *
     * Held rather than recomputed on every dial so that the answer cannot change under a
     * reconnect: a DNS name that resolved to a LAN address a moment ago is still the
     * address this session was established on, and re-classifying it mid-session would
     * move the session between networks for no reason the user asked for.
     */
    val local: Set<String> = emptySet(),
) {
    /** Everything, in the order it should be tried with nothing known about the network. */
    val all: List<String> get() = listOfNotNull(primary?.takeIf { it.isNotBlank() }) + fallbacks

    /**
     * The addresses to try, local ones first when the phone is on the same network as one
     * of them.
     *
     * **This is the LAN part, and it is an ordering rather than a filter on purpose.**
     * Being on a LAN address that has since stopped answering must fall through to the
     * remote one, so a local address is never the only thing tried; it is only tried
     * first. That makes the whole thing one rule instead of two, and means "the LAN is
     * down" needs no case of its own — it is the same code path as "the remote is down",
     * one step further along.
     *
     * Order within each group is the order the user typed, so an explicit preference
     * between two local addresses is respected rather than re-sorted arbitrarily.
     */
    fun candidates(onLocalNetwork: Boolean): List<String> =
        if (!onLocalNetwork) {
            all
        } else {
            all.filter { it in local } + all.filter { it !in local }
        }

    companion object {
        /**
         * Splits the stored fallback column into addresses.
         *
         * Comma-separated, and blank entries dropped so a trailing comma or a stray space
         * cannot become an address that is tried and fails. The order is preserved
         * exactly — see [candidates] for why that matters.
         */
        fun parseFallbacks(stored: String?): List<String> =
            stored.orEmpty().split(',').map { it.trim() }.filter { it.isNotEmpty() }

        /**
         * Whether an address looks like it is on a local network.
         *
         * Deliberately a *hint*, used only to decide what to try first, never to decide
         * whether to connect: a wrong answer here costs a few hundred milliseconds at
         * connect time, and no answer at all is possible for a name like
         * `nas.home.example` that resolves differently depending on where the phone is.
         *
         * Covers the two families' private ranges, loopback, link-local and `.local`
         * mDNS names — everything a home or office LAN plausibly uses — and nothing else,
         * so a public address is never mistaken for a LAN one and put first.
         */
        fun isLocalAddress(address: String): Boolean {
            val host = hostOf(address) ?: return false

            // mDNS: `.local` is resolved by the network you are on, by definition.
            if (host.endsWith(".local", ignoreCase = true)) return true
            if (host.equals("localhost", ignoreCase = true)) return true

            val octets = host.split('.')
            if (octets.size != 4) return false
            val parts = octets.map { it.toIntOrNull() ?: return false }
            if (parts.any { it !in 0..255 }) return false
            val (a, b) = parts

            return when {
                a == 10 -> true
                // 172.16.0.0/12
                a == 172 && b in 16..31 -> true
                a == 192 && b == 168 -> true
                // Link-local, which is what a phone gets when DHCP has not answered.
                a == 169 && b == 254 -> true
                a == 127 -> true
                else -> false
            }
        }

        /**
         * The host part of a URL-ish address, lower-cased, with any IPv6 brackets kept out
         * of the way.
         *
         * Written rather than parsed on purpose: this has to answer for half-typed
         * addresses in the editor too, where the string is not yet a URL.
         */
        private fun hostOf(address: String): String? {
            val withoutScheme = address.trim()
                .substringAfter("://", address.trim())
                .substringBefore('/')
                .substringBefore('?')
                .substringBefore('#')

            if (withoutScheme.isEmpty()) return null

            // IPv6 in brackets: [fe80::1]:8443
            if (withoutScheme.startsWith("[")) {
                val host = withoutScheme.substringAfter('[').substringBefore(']')
                return host.ifEmpty { null }?.lowercase()
            }

            // A bare IPv6 address has several colons and no port; anything with exactly
            // one colon could have a port, so that one is trimmed.
            val colonCount = withoutScheme.count { it == ':' }
            val host = if (colonCount == 1) withoutScheme.substringBefore(':') else withoutScheme
            return host.ifEmpty { null }?.lowercase()
        }
    }
}
