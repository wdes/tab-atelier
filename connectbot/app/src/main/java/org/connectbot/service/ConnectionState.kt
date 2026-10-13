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

package org.connectbot.service

/**
 * How a host's connection is doing.
 *
 * Moved here from the host list's view model for Tab Atelier Remote (Apache-2.0
 * section 4(b)): a transport has to be able to report its own state, and the host
 * list has to be able to draw it, so the vocabulary belongs beside the transports
 * rather than inside one screen's view model. Same name and same constants as
 * upstream so nothing else has to move.
 *
 * [RECONNECTING] is ours. It is what stops a dropped connection being drawn as a
 * dead one: for a tab-atelier server the daemon is holding the tab regardless, and
 * the app is only the viewer, so "the socket is down and I am re-dialling it" is a
 * different situation from "this session is over" — and the one that used to look
 * identical to a healthy connection.
 */
enum class ConnectionState {
    /** Nothing has been attempted, or nothing is known about this host. */
    UNKNOWN,

    /** The connection is up. */
    CONNECTED,

    /**
     * The connection dropped and is being re-established, by itself.
     *
     * Distinct from [DISCONNECTED] on purpose: a user seeing this should wait,
     * and a user seeing [DISCONNECTED] should not.
     */
    RECONNECTING,

    /** The connection is down and nothing is trying to bring it back. */
    DISCONNECTED,
}
