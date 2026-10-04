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

// New file for Tab Atelier Remote, not part of upstream ConnectBot: one tab of
// a tab-atelier daemon, and the parser for `GET {base}/tabs`. The protocol's
// own constants live in org.connectbot.transport.TabAtelier. See
// overlay/README.md.

package org.connectbot.tabatelier

import org.json.JSONArray
import org.json.JSONObject

/**
 * One tab of a tab-atelier daemon, as reported by `GET {base}/tabs`.
 *
 * Every field is optional on the wire: the daemon is a separate program on
 * another machine and may add, rename or drop fields. Each one is therefore
 * read independently and a missing or unexpected value falls back to a default
 * rather than failing the whole list.
 */
data class TabAtelierTab(
    /** Daemon-assigned tab uuid; empty when the daemon did not send one. */
    val id: String = "",
    val name: String = "",
    val badge: String? = null,
    /** Last non-empty output line, already ANSI-stripped by the daemon. */
    val preview: String? = null,
    val active: Boolean = false,
    val agentState: String? = null,
    val agentKind: String? = null,
    val locked: Boolean = false,
    val viewers: Int = 0,
    /** Unix milliseconds; 0 when unknown, which sorts last. */
    val lastUsedAt: Long = 0L,
)

/**
 * Parse a `GET {base}/tabs` response body.
 *
 * The result is sorted by [TabAtelierTab.lastUsedAt] descending: most recently
 * used first. Presenting the daemon's own ordering is the point of the list,
 * so the sort happens here rather than in the UI.
 *
 * @throws org.json.JSONException if the body is not a JSON object.
 */
fun parseTabAtelierTabs(body: String): List<TabAtelierTab> {
    val root = JSONObject(body)
    val tabs = root.optJSONArray("tabs") ?: JSONArray()
    return (0 until tabs.length())
        .mapNotNull { index -> tabs.optJSONObject(index)?.let(::toTab) }
        .sortedByDescending { it.lastUsedAt }
}

private fun toTab(json: JSONObject): TabAtelierTab = TabAtelierTab(
    id = json.optText("id") ?: "",
    name = json.optText("name") ?: "",
    badge = json.optText("badge"),
    preview = json.optText("preview"),
    active = json.optBoolean("active", false),
    agentState = json.optText("agent_state"),
    agentKind = json.optText("agent_kind"),
    locked = json.optBoolean("locked", false),
    viewers = json.optInt("viewers", 0),
    lastUsedAt = json.optLong("last_used_at", 0L),
)

/**
 * Read a string field, treating null/JSONObject.NULL/blank as absent. A number
 * where a string was expected is stringified rather than dropped: the tab still
 * shows up, just with a value we did not expect.
 */
private fun JSONObject.optText(name: String): String? {
    if (!has(name) || isNull(name)) return null
    val value = opt(name)
    val text = when (value) {
        is String -> value
        is Number, is Boolean -> value.toString()
        else -> null
    }
    return text?.takeIf { it.isNotEmpty() }
}
