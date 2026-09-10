// SPDX-License-Identifier: MPL-2.0
//
// Account management UI. Vue 3 global build — no bundler, no build step: the
// proxy serves these two files and the vendored libraries as they are, so
// there is nothing to compile before the .deb can be built and nothing to
// re-run after editing them.

const { createApp } = Vue;

createApp({
  // Read off the namespace rather than destructured into consts: these files
  // are plain <script> tags sharing one scope, so a top-level `const
  // CallsChart` here collides with the one in charts.js and the page dies with
  // a redeclaration SyntaxError before Vue ever mounts.
  components: {
    CallsChart: window.TaCharts.CallsChart,
    TokensChart: window.TaCharts.TokensChart,
    PressureChart: window.TaCharts.PressureChart,
  },
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
      // Per-account usage, keyed by id, as returned by /api/usage.
      usage: {},
      hours: 168,
      // null = everyone; an account id = just them. Clicking a row's token
      // total drills in, which is the only question the summed charts cannot
      // answer ("who is that spike?").
      focus: null,
      // The shared plan, as Anthropic reports it. Real data even when the
      // per-person numbers are seeded — this one is not ours to invent.
      pressure: null,
      pressureTimer: null,
      // "Weight" is the scheduler's word for this and means nothing to anyone
      // reading a table of people. The stored value is still a weight — the
      // API and the QoS maths are unchanged — but the UI names what it does.
      //
      // Normal sits in the MIDDLE so a CI account or a backlog fleet can yield
      // without everyone else needing a promotion. Roughly geometric, so each
      // step is a real difference: Background gets a tenth of what Normal
      // does, Critical ten times.
      tiers: [
        { weight: 1, label: "Background" },
        { weight: 2, label: "Low" },
        { weight: 5, label: "Normal" },
        { weight: 10, label: "Elevated" },
        { weight: 20, label: "High" },
        { weight: 50, label: "Critical" },
      ],
    };
  },
  computed: {
    plan() {
      return this.pressure?.plan || {};
    },
    sched() {
      return this.pressure?.scheduler || {};
    },
    degradeAbove() {
      return this.pressure?.degrade_above ?? 0.85;
    },
    planUtil() {
      return this.plan.utilization ?? null;
    },
    planHealth() {
      return this.plan.health || {};
    },
    // Whether the MONITOR is broken, as opposed to the plan being busy. The
    // two used to be indistinguishable on this page: a failing poll produced
    // a sample with no numbers, planSeries dropped it, and the chart simply
    // ended — which reads as "nothing happened since noon" rather than "we
    // stopped being able to look".
    monitorBroken() {
      return this.planHealth.stale === true || (this.planHealth.consecutive_failures ?? 0) > 0;
    },
    // What to say about it, in words, without making anyone read a log.
    monitorProblem() {
      if (!this.monitorBroken) return "";
      const h = this.planHealth;
      const since = h.last_ok_ts ? `last good reading ${this.whenLocal(h.last_ok_ts)}` : "no good reading yet";
      const n = h.consecutive_failures ?? 0;
      const failures = n === 1 ? "1 failed poll" : `${n} failed polls`;
      return `${since} · ${failures}`;
    },
    // Spelled out beside the number, so the state never rests on colour.
    planState() {
      // Checked before the utilisation branches: an unknown plan must not be
      // described as a healthy one, and this is the state routing is in too.
      if (this.planHealth.stale) return "plan unknown — the reading is too old to act on";
      if (this.sched.backoff_for) return "upstream is rate-limiting — everything is waiting";
      if (this.planUtil == null) return "no reading yet";
      if (this.planUtil >= (this.pressure?.floor_above ?? 0.95)) return "nearly exhausted — falling back to the cheapest model";
      if (this.planUtil >= this.degradeAbove) return "tight — large models are being downgraded";
      return "healthy — nothing is being throttled";
    },
    planClass() {
      if (this.planHealth.stale) return "text-body-secondary";
      if (this.sched.backoff_for || (this.planUtil ?? 0) >= (this.pressure?.floor_above ?? 0.95)) return "text-danger";
      if ((this.planUtil ?? 0) >= this.degradeAbove) return "text-warning";
      return "";
    },
    planSeries() {
      return (this.plan.history || [])
        .filter((s) => s.five_hour != null || s.seven_day != null)
        .map((s) => ({
          util: s.five_hour ?? s.seven_day,
          seven_day: s.seven_day,
          label: this.whenLocal(s.ts),
        }));
    },
    scopeLabel() {
      if (!this.focus) return "everyone";
      const u = this.users.find((x) => x.id === this.focus);
      return u ? `${u.first_name} ${u.last_name}` : "unknown";
    },
    // The series the charts draw: one account's, or every account's summed
    // hour by hour. Summing here rather than server-side keeps /api/usage a
    // plain per-account dump that the drill-down can reuse without refetching.
    series() {
      const all = Object.values(this.usage);
      const chosen = this.focus ? all.filter((u) => u.user.id === this.focus) : all;
      if (!chosen.length) return [];
      const base = chosen[0].series_hourly.map((b) => ({ ...b }));
      for (const acct of chosen.slice(1)) {
        acct.series_hourly.forEach((b, i) => {
          const t = base[i];
          if (!t || t.hour !== b.hour) return;
          t.calls += b.calls;
          t.errors += b.errors;
          t.input += b.input;
          t.output += b.output;
          t.cache_read += b.cache_read;
          t.cache_write += b.cache_write;
        });
      }
      return base;
    },
    tiles() {
      const sum = (pick) => this.series.reduce((a, b) => a + pick(b), 0);
      const calls = sum((b) => b.calls);
      const errors = sum((b) => b.errors);
      const input = sum((b) => b.input);
      const output = sum((b) => b.output);
      const cache = sum((b) => b.cache_read);
      return [
        { label: "API calls", value: this.fmt(calls), sub: errors ? `${errors} failed` : "none failed" },
        { label: "Tokens", value: this.fmt(input + output + cache), sub: "input + output + cache" },
        { label: "Input", value: this.fmt(input), sub: cache ? `${this.fmt(cache)} from cache` : "no cache hits" },
        { label: "Output", value: this.fmt(output), sub: this.scopeLabel },
      ];
    },
    meUrl() {
      return `${this.origin}/me/usage`;
    },
    // Both recipes carry the real key. It is readable exactly once, so a
    // `<key>` placeholder here means hand-copying 68 characters out of the
    // field above — and the commonest way that goes wrong is a truncated
    // paste, which fails as "no active account has that key" and reads like a
    // proxy fault rather than a typo.
    relayRecipe() {
      const key = this.freshKey ? this.freshKey.key : "<key>";
      // `--label` is a named flag. This block used to say `remote add proxy
      // --url …`, which the CLI rejects outright as an unknown argument.
      return [
        `tab-atelier remote add --label proxy --url ${this.origin} --relay-token ${key}`,
        "tab-atelier relay via proxy",
        "tab-atelier relay on",
      ].join("\n");
    },
    // The proxy's Anthropic path lives under /relay/anthropic; the client
    // appends /v1/messages itself.
    envRecipe() {
      const key = this.freshKey ? this.freshKey.key : "<key>";
      return [
        `export ANTHROPIC_BASE_URL=${this.origin}/relay/anthropic`,
        `export ANTHROPIC_API_KEY=${key}`,
      ].join("\n");
    },
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
          // Sent twice on purpose: Authorization is the header reverse
          // proxies and auth modules are most likely to consume before it
          // reaches a backend, and when that happens the server sees no
          // credential at all — indistinguishable from a wrong token. A
          // plainly-named custom header survives those arrangements.
          "X-Admin-Token": this.token,
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
        // Trim: a token pasted from a terminal usually brings a newline, and
        // the comparison is exact.
        this.token = (this.token || "").trim();
        const data = await this.api("GET", "/api/users");
        this.users = data.users;
        this.authed = true;
        sessionStorage.setItem("ta-proxy-admin", this.token);
        await this.loadUsage();
        await this.loadPressure();
        // The plan moves on its own, independently of anything done here.
        this.pressureTimer ??= setInterval(() => this.loadPressure(), 30_000);
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
      this.usage = {};
      this.focus = null;
      this.pressure = null;
      clearInterval(this.pressureTimer);
      this.pressureTimer = null;
      this.freshKey = null;
    },
    async refresh() {
      this.users = (await this.api("GET", "/api/users")).users;
      await this.loadUsage();
    },
    async loadPressure() {
      try {
        this.pressure = await this.api("GET", "/api/pressure");
      } catch {
        // A pressure read failing must not blank the accounts page it sits on.
      }
    },
    // A weight set through the API need not be on the ladder, so the select
    // has to be able to show it rather than silently snapping it to a tier.
    tierLabel(weight) {
      const t = this.tiers.find((x) => x.weight === weight);
      return t ? t.label : `Custom (${weight})`;
    },
    tiersFor(u) {
      const w = u.weight || 1;
      return this.tiers.some((t) => t.weight === w)
        ? this.tiers
        : [...this.tiers, { weight: w, label: this.tierLabel(w) }].sort((a, b) => a.weight - b.weight);
    },
    // What this priority actually works out to, if everyone were busy at once.
    // A number like "3" is meaningless on its own; "≈60% when everyone is
    // busy" is the decision being made.
    shareOf(u) {
      const active = this.users.filter((x) => !x.disabled);
      const total = active.reduce((a, x) => a + (x.weight || 1), 0);
      if (u.disabled) return "disabled";
      if (!total || active.length < 2) return "all of it when alone";
      return `≈${Math.round(((u.weight || 1) / total) * 100)}% when all busy`;
    },
    setWeight(u, value) {
      const weight = Math.max(1, Math.min(100, Number(value) || 1));
      return this.act(() => this.api("POST", `/api/users/${u.id}/weight`, { weight }));
    },
    async loadUsage() {
      const data = await this.api("GET", `/api/usage?hours=${this.hours}`);
      const next = {};
      for (const u of data.users) next[u.user.id] = u;
      this.usage = next;
      // Drilling into someone who has since been deleted would show an empty
      // chart with their name on it.
      if (this.focus && !next[this.focus]) this.focus = null;
    },
    // Rows render before the first usage load returns, and for an account
    // that has never called anything there is simply no entry.
    usageOf(id) {
      return (
        this.usage[id] || {
          last_7d: { calls: 0, errors: 0, tokens: { total: 0 } },
          all_time: { calls: 0, errors: 0, tokens: { total: 0 } },
        }
      );
    },
    fmt(n) {
      if (n >= 1e9) return `${(n / 1e9).toFixed(1)}B`;
      if (n >= 1e6) return `${(n / 1e6).toFixed(1)}M`;
      if (n >= 1e3) return `${(n / 1e3).toFixed(1)}k`;
      return String(n ?? 0);
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
    // Adding a key never disturbs the existing ones, which is what makes
    // moving a machine across safe: add, deploy, then delete the old one.
    addKey(u) {
      const name = prompt(`Name for ${u.first_name}'s new key (laptop, ci, fleet…)`, "");
      if (!name) return;
      return this.act(async () => {
        const data = await this.api("POST", `/api/users/${u.id}/keys`, { name });
        this.showKey({ user: u, key: data.secret, name: data.key.name });
      });
    },
    removeKey(u, k) {
      if (!confirm(`Delete ${u.first_name}'s key "${k.name}"? Anything using it stops working. Their other keys are unaffected.`))
        return;
      return this.act(() => this.api("DELETE", `/api/users/${u.id}/keys/${k.id}`));
    },
    toggleKey(u, k) {
      return this.act(() => this.api("POST", `/api/users/${u.id}/keys/${k.id}/disabled`, { disabled: !k.disabled }));
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
        name: data.name ? `"${data.name}"` : "",
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
    // Every timestamp the proxy emits carries a zone — epoch seconds, or ISO
    // 8601 with an offset. Rendering is the browser's job, in the zone of
    // whoever is looking: chopping the offset off the string and showing the
    // rest, which is what this used to do, silently displays UTC as if it were
    // local. An hour wrong is worse than no timestamp.
    whenLocal(iso) {
      const d = new Date(iso);
      if (Number.isNaN(d.getTime())) return String(iso ?? "");
      return d.toLocaleString([], { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });
    },
    // "resets 09:00" is only useful if you already know what time it is there.
    // What anybody actually wants is how long they have.
    untilLocal(iso) {
      const d = new Date(iso);
      if (Number.isNaN(d.getTime())) return "";
      const mins = Math.round((d.getTime() - Date.now()) / 60000);
      if (mins <= 0) return "any moment";
      if (mins < 60) return `in ${mins} min`;
      const h = Math.floor(mins / 60);
      const m = mins % 60;
      return m ? `in ${h} h ${m} min` : `in ${h} h`;
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
