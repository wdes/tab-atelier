"use strict";
// SPDX-License-Identifier: MPL-2.0
//
// Every HTTP call the admin UI makes, in one class.
//
// SOURCE. The proxy serves ../assets/api.js, compiled from here — edit this
// one, the same way as app.ts. `bun run build` in this directory, and the
// `web_assets` test fails if the committed output is stale.
//
// Why a class rather than the `api(method, path, body)` helper this replaces:
//
//  * a path is written once. `/api/users/${id}/keys/${key}/disabled` appeared
//    in the call sites as an interpolated string, so a typo was a 404 at
//    runtime and nothing could see it. Here it is one line with one name.
//  * the response type is stated where the path is, not at each caller, so
//    `app.ts` reads as UI and this file reads as the wire.
//  * there is one place that knows the base URL, the error shape and the
//    headers. That is what made the credential bug below a one-line fix
//    instead of several.
//
// This file is a script, not a module: like charts.ts it has no top-level
// import or export, so `tsc` emits a plain `<script>` and the classes are
// reachable as globals. Names are put on `window.TaApi` rather than left as
// top-level consts, because every script tag here shares one scope and a
// collision is a redeclaration SyntaxError that blanks the page before Vue
// mounts — the reason charts.ts is wrapped in an IIFE.
/** A refusal from the proxy.
 *
 * The API answers every error with `{"error": "..."}` and a status, so that is
 * all this carries. Mounch's client also exposes per-field messages for
 * Laravel's 422; this API does not send any, and inventing an empty map would
 * be a field nobody could ever populate. */
class ApiError extends Error {
    status;
    /** The parsed body, for a caller that wants more than the message. */
    body;
    constructor(status, body, message) {
        super(message);
        this.name = "ApiError";
        this.status = status;
        this.body = body;
    }
}
/**
 * Every call the dashboard makes.
 *
 * One `request` underneath and a namespace per resource on top, so the base
 * URL, the headers and the error handling exist once.
 */
class ApiClient {
    /** Scheme, host and port — no path, and no userinfo. */
    baseUrl;
    users;
    keys;
    providers;
    mappings;
    usage;
    pressure;
    inspect;
    constructor(options = {}) {
        this.baseUrl = options.baseUrl ?? window.location.origin;
        // Each namespace closes over `this`, so a method can be handed around and
        // still sends through the same client.
        this.users = {
            list: () => this.request("GET", "/api/users").then((r) => r.users),
            add: (form) => this.request("POST", "/api/users", form).then((r) => r.user),
            remove: (id) => this.request("DELETE", `/api/users/${seg(id)}`).then((r) => r.removed),
            setDisabled: (id, disabled) => this.request("POST", `/api/users/${seg(id)}/disabled`, { disabled }),
            setWeight: (id, weight) => this.request("POST", `/api/users/${seg(id)}/weight`, { weight }),
            setProvider: (id, provider) => this.request("POST", `/api/users/${seg(id)}/provider`, { provider }),
            setModel: (id, model) => this.request("POST", `/api/users/${seg(id)}/model`, { model }),
            setCompact: (id, compact) => this.request("POST", `/api/users/${seg(id)}/compact`, { compact }),
            setTools: (id, policy) => this.request("POST", `/api/users/${seg(id)}/tools`, policy),
        };
        this.keys = {
            add: (id, name) => this.request("POST", `/api/users/${seg(id)}/keys`, { name }),
            remove: (id, keyId) => this.request("DELETE", `/api/users/${seg(id)}/keys/${seg(keyId)}`),
            setDisabled: (id, keyId, disabled) => this.request("POST", `/api/users/${seg(id)}/keys/${seg(keyId)}/disabled`, { disabled }),
        };
        this.providers = {
            list: () => this.request("GET", "/api/providers"),
            save: (body) => this.request("POST", "/api/providers", body),
            rotateKey: (id, key) => this.request("POST", `/api/providers/${seg(id)}/key`, { key }),
            remove: (id) => this.request("DELETE", `/api/providers/${seg(id)}`),
        };
        this.mappings = {
            add: (mapping) => this.request("POST", "/api/mappings", mapping),
            remove: (from) => this.request("DELETE", `/api/mappings/${seg(from)}`),
        };
        this.usage = {
            report: (window) => this.request("GET", `/api/usage?window=${encodeURIComponent(window)}`),
        };
        this.pressure = { get: () => this.request("GET", "/api/pressure") };
        this.inspect = {
            get: () => this.request("GET", "/api/inspect"),
            arm: (minutes) => this.request("POST", "/api/inspect", { minutes }),
            disarm: () => this.request("DELETE", "/api/inspect"),
        };
    }
    /**
     * The absolute URL for a path.
     *
     * Absolute rather than relative on purpose. A relative URL resolves against
     * the document's, so a page opened as `http://admin:tap_…@host/` carried
     * those credentials into every request — Firefox warns about it in the
     * console, and the secret ends up in the log of anything in front of the
     * proxy. `origin` is scheme + host + port with no userinfo, and the browser
     * attaches the credential it holds anyway.
     */
    resolveUrl(path) {
        const base = this.baseUrl.replace(/\/+$/, "");
        return base + (path.startsWith("/") ? path : `/${path}`);
    }
    /**
     * One request, with the status checked and the error unwrapped.
     *
     * A 401 is called out in words because the browser owns the credential now:
     * the page only loaded because it was answered, so a 401 here means that
     * answer has since expired or the token was rotated — neither of which the
     * operator can see from a bare status.
     */
    // `object` rather than `JsonValue`: every body this API takes is a request
    // struct, and a TypeScript interface has no implicit index signature, so it
    // is not assignable to `JsonValue`'s object branch. What is checked against
    // what is the facade above — `providers.save` still demands a
    // `SaveProviderBody`, which is where a wrong field name is caught. `object`
    // is here to rule out the mistakes that matter at this layer: undefined, a
    // bare string, a number.
    async request(method, path, body) {
        const response = await fetch(this.resolveUrl(path), {
            method,
            headers: body === undefined ? {} : { "Content-Type": "application/json" },
            body: body === undefined ? undefined : JSON.stringify(body),
        });
        if (!response.ok) {
            throw await this.failure(response);
        }
        // A 204, or a 200 with nothing in it. `json()` would throw on an empty
        // body, and there is nothing to parse.
        const text = await response.text();
        return (text ? JSON.parse(text) : undefined);
    }
    /** Turn a failed response into an `ApiError`, reading the server's wording. */
    async failure(response) {
        const raw = await response.text();
        let message = `${response.status} ${response.statusText}`.trim();
        let body = raw;
        try {
            const parsed = JSON.parse(raw);
            if (parsed !== null &&
                typeof parsed === "object" &&
                !Array.isArray(parsed) &&
                typeof parsed.error === "string") {
                message = parsed.error;
            }
            body = parsed;
        }
        catch {
            // Not JSON — an HTML error page from something in front of the proxy,
            // or an empty body. The status line is then the only truth available.
            if (raw.trim())
                message = `${message}: ${raw.slice(0, 200)}`;
        }
        if (response.status === 401) {
            message = `the browser's sign-in for this proxy was refused (${message}). Reload the page to get a fresh prompt.`;
        }
        return new ApiError(response.status, body, message);
    }
}
/** A path segment, escaped.
 *
 * Ids and key names reach the URL verbatim, and a `+` or a space in one would
 * otherwise change the path rather than the value. */
function seg(value) {
    return encodeURIComponent(value);
}
/** The one client the page uses, and the classes, for the tests. */
window.TaApi = { ApiClient, ApiError, api: new ApiClient() };
