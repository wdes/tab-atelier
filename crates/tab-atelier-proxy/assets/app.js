// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Account management UI. Vue 3 global build — no bundler, no build step: the
// proxy serves these two files and the vendored libraries as they are, so
// there is nothing to compile before the .deb can be built and nothing to
// re-run after editing them.

const { createApp } = Vue;

createApp({
  data() {
    return {
      // sessionStorage, not localStorage: the admin token should not outlive
      // the tab it was typed into.
      token: sessionStorage.getItem("ta-proxy-admin") || "",
      authed: false,
      users: [],
      form: { first_name: "", last_name: "", email: "" },
      freshKey: null,
      copied: false,
      error: "",
      busy: false,
      origin: window.location.origin,
    };
  },
  mounted() {
    // A token already in this session means a reload should land straight back
    // on the list rather than asking again.
    if (this.token) this.signIn();
  },
  methods: {
    async api(method, path, body) {
      const resp = await fetch(path, {
        method,
        headers: {
          Authorization: "Bearer " + this.token,
          ...(body ? { "Content-Type": "application/json" } : {}),
        },
        body: body ? JSON.stringify(body) : undefined,
      });
      const text = await resp.text();
      let data = {};
      try {
        data = text ? JSON.parse(text) : {};
      } catch {
        // A non-JSON body here means something in front of the proxy answered
        // (a gateway, a captive portal). Say that rather than "unexpected
        // token < in JSON", which sends people looking in the wrong place.
        throw new Error(`${resp.status}: ${text.slice(0, 120) || "empty response"}`);
      }
      if (!resp.ok) throw new Error(data.error || `${resp.status}`);
      return data;
    },
    async signIn() {
      this.busy = true;
      this.error = "";
      try {
        const data = await this.api("GET", "/api/users");
        this.users = data.users;
        this.authed = true;
        sessionStorage.setItem("ta-proxy-admin", this.token);
      } catch (e) {
        this.error = String(e.message || e);
        this.authed = false;
        sessionStorage.removeItem("ta-proxy-admin");
      } finally {
        this.busy = false;
      }
    },
    signOut() {
      sessionStorage.removeItem("ta-proxy-admin");
      this.token = "";
      this.authed = false;
      this.users = [];
      this.freshKey = null;
    },
    async refresh() {
      this.users = (await this.api("GET", "/api/users")).users;
    },
    // Every mutation funnels through here so a failure always lands in the
    // banner instead of the console, and the list can never drift from the
    // server's state after a partial success.
    async act(fn) {
      this.busy = true;
      this.error = "";
      try {
        await fn();
        await this.refresh();
      } catch (e) {
        this.error = String(e.message || e);
      } finally {
        this.busy = false;
      }
    },
    add() {
      return this.act(async () => {
        const data = await this.api("POST", "/api/users", this.form);
        this.showKey(data);
        this.form = { first_name: "", last_name: "", email: "" };
      });
    },
    rotate(u) {
      if (!confirm(`Replace ${u.first_name} ${u.last_name}'s key? The current one stops working at once.`)) return;
      return this.act(async () => this.showKey(await this.api("POST", `/api/users/${u.id}/rotate`)));
    },
    setDisabled(u, disabled) {
      return this.act(() => this.api("POST", `/api/users/${u.id}/disabled`, { disabled }));
    },
    remove(u) {
      if (!confirm(`Delete ${u.first_name} ${u.last_name} <${u.email}>? Their key stops working.`)) return;
      return this.act(() => this.api("DELETE", `/api/users/${u.id}`));
    },
    showKey(data) {
      this.copied = false;
      this.freshKey = {
        who: `${data.user.first_name} ${data.user.last_name} <${data.user.email}>`,
        key: data.key,
      };
    },
    async copy(text) {
      try {
        await navigator.clipboard.writeText(text);
        this.copied = true;
      } catch {
        // Clipboard access needs a secure context; over plain http on a LAN
        // address it simply is not there. The field is selectable, so say what
        // to do instead of failing silently.
        this.error = "Clipboard unavailable (needs HTTPS) — select the key and copy it manually.";
      }
    },
    ago(secs) {
      if (!secs) return "never";
      const d = Math.floor(Date.now() / 1000) - secs;
      if (d < 90) return "just now";
      if (d < 5400) return `${Math.floor(d / 60)} min ago`;
      if (d < 172800) return `${Math.floor(d / 3600)} h ago`;
      return `${Math.floor(d / 86400)} d ago`;
    },
  },
}).mount("#app");
