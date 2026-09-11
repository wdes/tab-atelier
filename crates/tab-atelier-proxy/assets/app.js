"use strict";
// SPDX-License-Identifier: MPL-2.0
//
// Account management UI. Vue 3 global build, no bundler: the proxy serves the
// compiled output and the vendored libraries as they are.
//
// SOURCE. The file the proxy serves is ../assets/app.js, compiled from here —
// edit this one. `bun run build` in crates/tab-atelier-proxy/web, and the
// `web_assets` test fails if the committed output is stale.
const { createApp } = Vue;
/** A zeroed usage record, for an account that has not called anything yet. */
function emptyWindow() {
    return {
        calls: 0,
        errors: 0,
        tokens: { input: 0, output: 0, cache_read: 0, cache_write: 0, total: 0 },
    };
}
function emptyUsage(id) {
    return {
        user: {
            id,
            first_name: "",
            last_name: "",
            email: "",
            created_at: 0,
            disabled: false,
            weight: 1,
            keys: [],
            last_used_at: null,
            has_key: false,
            provider: null,
            // Off, matching the server's default and the `#[serde(default)]` that
            // gives every account already on disk the same value.
            compact: "none",
        },
        last_24h: emptyWindow(),
        last_7d: emptyWindow(),
        all_time: emptyWindow(),
        series_hourly: [],
    };
}
// Bound to a const before `createApp` sees it. Passing the literal inline
// leaves Vue unable to tie `data`, `computed` and `methods` together, and it
// fails open: `this` becomes `any` in methods and `{}` in computed, so the
// file type-checks while checking nothing.
const AdminApp = Vue.defineComponent({
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
            usageBusy: false,
            pressureBusy: false,
            origin: window.location.origin,
            // Per-account usage, keyed by id, as returned by /api/usage.
            usage: {},
            // 24 h, not the week: the question this page gets opened for is "what is
            // happening now" — a spike, a burst of errors, a key that just started
            // being used. A 7-day default flattens exactly that into the noise floor,
            // and the wider windows are one click away.
            hours: 24,
            // null = everyone; an account id = just them. Clicking a row's token
            // total drills in, which is the only question the summed charts cannot
            // answer ("who is that spike?").
            focus: null,
            // The shared plan, as Anthropic reports it. Real data even when the
            // per-person numbers are seeded — this one is not ours to invent.
            pressure: null,
            pressureTimer: null,
            // Inspection: null until the panel is opened, so nothing is fetched —
            // and nothing hints that captures exist — unless someone asks.
            inspect: null,
            inspectOpen: null,
            providers: null,
            newMapping: { from: "", to: "", note: "" },
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
            if (!this.monitorBroken)
                return "";
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
            if (this.planHealth.stale)
                return "plan unknown — the reading is too old to act on";
            if (this.sched.backoff_for)
                return "upstream is rate-limiting — everything is waiting";
            if (this.planUtil == null)
                return "no reading yet";
            if (this.planUtil >= (this.pressure?.floor_above ?? 0.95))
                return "nearly exhausted — falling back to the cheapest model";
            if (this.planUtil >= this.degradeAbove)
                return "tight — large models are being downgraded";
            return "healthy — nothing is being throttled";
        },
        planClass() {
            if (this.planHealth.stale)
                return "text-body-secondary";
            if (this.sched.backoff_for || (this.planUtil ?? 0) >= (this.pressure?.floor_above ?? 0.95))
                return "text-danger";
            if ((this.planUtil ?? 0) >= this.degradeAbove)
                return "text-warning";
            return "";
        },
        weeklyUtil() {
            return this.plan.latest?.seven_day ?? null;
        },
        // The weekly cap can be the binding one even while the session window
        // looks calm, so it gets the same warning treatment rather than staying
        // grey until someone happens to read the number.
        weeklyClass() {
            const w = this.weeklyUtil;
            if (w == null)
                return "";
            if (w >= 0.95)
                return "text-danger";
            if (w >= this.degradeAbove)
                return "text-warning";
            return "";
        },
        planSeries() {
            return (this.plan.history || [])
                .filter((s) => s.five_hour != null || s.seven_day != null)
                .map((s) => ({
                // `?? null` rather than passing the optional through: a missing
                // reading and an absent field are the same thing to the chart, and
                // `undefined` would slip past a `!= null` guard as a hole.
                util: s.five_hour ?? s.seven_day ?? null,
                seven_day: s.seven_day ?? null,
                label: this.whenLocal(s.ts),
            }));
        },
        scopeLabel() {
            if (!this.focus)
                return "everyone";
            const u = this.users.find((x) => x.id === this.focus);
            return u ? `${u.first_name} ${u.last_name}` : "unknown";
        },
        // The series the charts draw: one account's, or every account's summed
        // hour by hour. Summing here rather than server-side keeps /api/usage a
        // plain per-account dump that the drill-down can reuse without refetching.
        series() {
            const all = Object.values(this.usage);
            const chosen = this.focus ? all.filter((u) => u.user.id === this.focus) : all;
            if (!chosen.length)
                return [];
            const first = chosen[0];
            if (!first)
                return [];
            const base = first.series_hourly.map((b) => ({ ...b }));
            for (const acct of chosen.slice(1)) {
                acct.series_hourly.forEach((b, i) => {
                    const t = base[i];
                    if (!t || t.hour !== b.hour)
                        return;
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
        /**
         * One calls line per account, for the un-focused view.
         *
         * Slots come from the ACCOUNT LIST POSITION, not from size. Two reasons,
         * and the second is the one that matters:
         *
         *  - colour must follow the entity, never its rank, or an hour where
         *    somebody else is busiest repaints the whole chart;
         *  - clicking a row sets `focus`, and a size-ordered palette would
         *    recolour every remaining account at that moment. Stable slots mean
         *    the line you were following stays the same colour when you drill in.
         *
         * Past eight, the rest fold into a muted "Other" — a ninth hue is never
         * generated, because a generated one is not colourblind-safe and the
         * ordering is what makes the palette work at all.
         */
        callsByUser() {
            // Focused: the chart draws the single summed series instead, and an
            // empty list is how it is told to.
            if (this.focus)
                return [];
            const withUsage = this.users.filter((u) => this.usage[u.id]);
            if (withUsage.length < 2)
                return [];
            const grid = this.series.map((b) => b.hour);
            const series = withUsage.map((u, i) => {
                const acct = this.usage[u.id];
                const byHour = new Map((acct?.series_hourly ?? []).map((b) => [b.hour, b]));
                // Rebuilt on the SAME grid as `series`, so index alignment with the
                // x axis is guaranteed rather than assumed — the chart maps a series
                // point to an x position by index.
                const points = grid.map((hour) => {
                    const b = byHour.get(hour);
                    return {
                        hour,
                        calls: b?.calls ?? 0,
                        errors: b?.errors ?? 0,
                        input: b?.input ?? 0,
                        output: b?.output ?? 0,
                        cache_read: b?.cache_read ?? 0,
                        cache_write: b?.cache_write ?? 0,
                    };
                });
                return { id: u.id, name: u.first_name || u.email, slot: 0, points };
            });
            const calls = (s) => s.points.reduce((a, p) => a + p.calls, 0);
            const drawn = series.filter((s) => s.points.some((p) => p.calls > 0));
            if (drawn.length <= 8) {
                // Slots come from the position in THIS list — the account list, which
                // the server returns in a fixed order — so they are stable across
                // refreshes and across a focus change.
                return drawn.map((s, i) => ({ ...s, slot: i }));
            }
            // Past eight, the busiest eight keep a colour and the rest are summed.
            const busiest = new Set([...drawn]
                .sort((a, b) => calls(b) - calls(a))
                .slice(0, 8)
                .map((s) => s.id));
            // …but the kept eight take their slots from the STABLE order, not from
            // their rank by size. Numbering them by rank would repaint every line the
            // moment one account overtook another, and — as this was first written —
            // could hand out slot 8, 9 or 10 to the busiest accounts, which the
            // eight-slot palette maps to muted ink and so renders indistinguishably
            // from the folded group.
            const kept = drawn.filter((s) => busiest.has(s.id));
            const other = series.filter((s) => !busiest.has(s.id));
            const folded = {
                id: "__other__",
                name: `Other (${other.length})`,
                slot: -1,
                points: grid.map((hour, i) => ({
                    hour,
                    calls: other.reduce((a, s) => a + (s.points[i]?.calls ?? 0), 0),
                    errors: other.reduce((a, s) => a + (s.points[i]?.errors ?? 0), 0),
                    input: 0,
                    output: 0,
                    cache_read: 0,
                    cache_write: 0,
                })),
            };
            return [...kept.map((s, i) => ({ ...s, slot: i })), folded];
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
        if (this.token)
            this.signIn();
    },
    methods: {
        // Generic so each call site says what it expects back. The alternative is
        // an `any` that quietly spreads through every caller — which is what this
        // migration exists to stop.
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
            }
            catch {
                // A non-JSON body here means something in front of the proxy answered
                // (a gateway, a captive portal). Say that rather than "unexpected
                // token < in JSON", which sends people looking in the wrong place.
                throw new Error(`${resp.status}: ${text.slice(0, 120) || "empty response"}`);
            }
            if (!resp.ok)
                throw new Error(data.error || `${resp.status}`);
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
                // With the rest, not behind a button: the account table's "routed to"
                // dropdown reads this list, so leaving it unloaded gave every row an
                // empty picker and made a configured proxy look like it had no
                // providers at all.
                await this.loadProviders();
                // The plan moves on its own, independently of anything done here.
                this.pressureTimer ??= setInterval(() => this.loadPressure(), 30_000);
            }
            catch (e) {
                this.error = e instanceof Error ? e.message : String(e);
                this.authed = false;
                sessionStorage.removeItem("ta-proxy-admin");
            }
            finally {
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
            // Guarded: `clearInterval(null)` is legal JavaScript and a type error,
            // and the guard is free.
            if (this.pressureTimer !== null)
                clearInterval(this.pressureTimer);
            this.pressureTimer = null;
            this.freshKey = null;
        },
        async refresh() {
            this.users = (await this.api("GET", "/api/users")).users;
            await this.loadUsage();
            // Kept in step with the account list: a provider added or removed in
            // another tab would otherwise leave every row's picker showing stale
            // choices.
            if (this.providers)
                await this.loadProviders();
        },
        // Inspection. Nothing is fetched until the panel is opened: a page that
        // silently pulled captured prompts on every load would be collecting them
        // into a browser as well as a file.
        async loadProviders() {
            this.providers = await this.api("GET", "/api/providers");
        },
        async addPreset(p) {
            await this.api("POST", "/api/providers", { preset: p.id });
            await this.loadProviders();
        },
        async setKey(p) {
            const key = prompt(`API key for ${p.id}:`, "");
            if (!key)
                return;
            await this.api("POST", `/api/providers/${p.id}/key`, { key });
            await this.loadProviders();
        },
        async toggleProvider(p) {
            await this.api("POST", "/api/providers", {
                id: p.id,
                base_url: p.base_url,
                models: p.models.filter((m) => !m.deprecated).map((m) => `${m.id}:${m.class}:${m.relative_cost}`).join(","),
                preference: p.preference,
            });
            await this.loadProviders();
        },
        // Changing how much of an old conversation this provider is sent.
        //
        // Refused client-side with the SERVER's own explanation — `compact_refusal`
        // is the same rule the save path enforces, sent up rather than restated
        // here, because a second copy of "which provider is the subscription"
        // would eventually disagree with the first.
        //
        // Deliberately not via `act`: that clears `this.error` before running, so
        // the reason would be wiped by the very call it explains. The select is
        // bound with `:value`, not `v-model`, so a refused change snaps back to
        // what is actually stored.
        async setCompact(p, value) {
            if (value !== "none" && p.compact_refusal) {
                this.error = p.compact_refusal;
                return;
            }
            this.busy = true;
            try {
                await this.api("POST", "/api/providers", {
                    id: p.id,
                    base_url: p.base_url,
                    models: p.models.filter((m) => !m.deprecated).map((m) => `${m.id}:${m.class}:${m.relative_cost}`).join(","),
                    compact: value,
                });
                this.error = "";
                await this.loadProviders();
            }
            catch (e) {
                this.error = e instanceof Error ? e.message : String(e);
            }
            finally {
                this.busy = false;
            }
        },
        // The account's own compaction level. Bound with `:value`, not `v-model`,
        // like the provider select below was: a refused change must snap back to
        // what is actually stored, and the server is the one that refuses — it
        // knows every hop this account could reach, the browser does not.
        async setUserCompact(u, value) {
            this.busy = true;
            try {
                await this.api("POST", `/api/users/${u.id}/compact`, { compact: value });
                this.error = "";
                await this.refresh();
            }
            catch (e) {
                this.error = e instanceof Error ? e.message : String(e);
                await this.refresh();
            }
            finally {
                this.busy = false;
            }
        },
        async removeProvider(p) {
            if (!confirm(`Remove provider ${p.id}? Accounts pinned to it are unpinned.`))
                return;
            await this.api("DELETE", `/api/providers/${p.id}`);
            await this.loadProviders();
            await this.refresh();
        },
        async addMapping() {
            const m = this.newMapping;
            if (!m.from || !m.to)
                return;
            await this.api("POST", "/api/mappings", m);
            this.newMapping = { from: "", to: "", note: "" };
            await this.loadProviders();
        },
        async removeMapping(m) {
            await this.api("DELETE", `/api/mappings/${encodeURIComponent(m.from)}`);
            await this.loadProviders();
        },
        async setUserProvider(u, provider) {
            await this.api("POST", `/api/users/${u.id}/provider`, { provider });
            await this.refresh();
        },
        // The account's compaction level, as words rather than wire spelling.
        compactLabel(u) {
            const found = this.providers?.compact_levels.find((c) => c.value === u.compact);
            return found ? found.label : u.compact;
        },
        // "in 12 · out 340 · cache 1.2k" — the four numbers that answer "why was
        // that turn expensive", beside the request that produced them.
        tokenSummary(c) {
            const t = c.tokens;
            if (!t)
                return "—";
            const parts = [`in ${this.fmt(t.input)}`, `out ${this.fmt(t.output)}`];
            const cached = t.cache_read + t.cache_write;
            if (cached)
                parts.push(`cache ${this.fmt(cached)}`);
            return parts.join(" · ");
        },
        async loadInspect() {
            this.inspect = await this.api("GET", "/api/inspect");
        },
        async armInspect() {
            const raw = prompt(`Capture the JSON sent to Anthropic for how many minutes? (max ${this.inspect?.max_arm_minutes ?? 60})\n\n` +
                "Captures contain PROMPTS — whatever people are working on. Credentials are stripped, " +
                "the rest is not. Recording stops by itself when the time is up.", "15");
            if (!raw)
                return;
            const minutes = Number(raw);
            if (!Number.isFinite(minutes) || minutes < 1)
                return;
            await this.api("POST", "/api/inspect", { minutes });
            await this.loadInspect();
        },
        // One action, because "stop recording" and "delete what you recorded" are
        // the same intention in practice.
        async stopInspect() {
            if (!confirm("Stop capturing and delete every capture taken so far?"))
                return;
            await this.api("DELETE", "/api/inspect");
            this.inspectOpen = null;
            await this.loadInspect();
        },
        prettyJson(body) {
            try {
                return JSON.stringify(JSON.parse(body), null, 2);
            }
            catch {
                // Truncated captures are deliberately not valid JSON — say so rather
                // than showing an error the operator has to decode.
                return body;
            }
        },
        // Quiet by default because of the 30 s poll: a failing read must not blank
        // the accounts page it sits on, and a banner raised on every tick would be
        // unreadable. A reload someone pressed is not quiet — silence there is
        // indistinguishable from a button that does nothing.
        async loadPressure(quiet = true) {
            try {
                this.pressure = await this.api("GET", "/api/pressure");
            }
            catch (e) {
                if (!quiet)
                    this.error = e instanceof Error ? e.message : String(e);
            }
        },
        async reloadPressure() {
            this.pressureBusy = true;
            this.error = "";
            try {
                await this.loadPressure(false);
            }
            finally {
                this.pressureBusy = false;
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
            if (u.disabled)
                return "disabled";
            if (!total || active.length < 2)
                return "all of it when alone";
            return `≈${Math.round(((u.weight || 1) / total) * 100)}% when all busy`;
        },
        setWeight(u, value) {
            const weight = Math.max(1, Math.min(100, Number(value) || 1));
            return this.act(() => this.api("POST", `/api/users/${u.id}/weight`, { weight }));
        },
        async loadUsage() {
            const data = await this.api("GET", `/api/usage?hours=${this.hours}`);
            const next = {};
            for (const u of data.users)
                next[u.user.id] = u;
            this.usage = next;
            // Drilling into someone who has since been deleted would show an empty
            // chart with their name on it.
            if (this.focus && !next[this.focus])
                this.focus = null;
        },
        // The loader throws where its callers already catch (saving a weight, the
        // window picker's own handler); a reload pressed by hand has no such caller.
        async reloadUsage() {
            this.usageBusy = true;
            this.error = "";
            try {
                await this.loadUsage();
            }
            catch (e) {
                this.error = e instanceof Error ? e.message : String(e);
            }
            finally {
                this.usageBusy = false;
            }
        },
        // Rows render before the first usage load returns, and for an account
        // that has never called anything there is simply no entry.
        usageOf(id) {
            // The placeholder is a COMPLETE AccountUsage. It used to carry only
            // `last_7d` and `all_time`, which was fine while the table read nothing
            // else — and would have become `undefined.calls` the moment anything
            // touched `last_24h` or `series_hourly` on an account with no usage yet.
            // The type is what turned that from a future bug into a compile error.
            return this.usage[id] ?? emptyUsage(id);
        },
        // Mean tokens per call. `—` rather than 0 when nobody called: dividing by
        // no calls is not an average of zero, and showing one invites the reading
        // that this account is cheap when it is simply idle.
        avgPerCall(id) {
            const u = this.usageOf(id).last_7d;
            if (!u.calls)
                return "— avg/call";
            return `${this.fmt(Math.round(u.tokens.total / u.calls))} avg/call`;
        },
        fmt(n) {
            if (n >= 1e9)
                return `${(n / 1e9).toFixed(1)}B`;
            if (n >= 1e6)
                return `${(n / 1e6).toFixed(1)}M`;
            if (n >= 1e3)
                return `${(n / 1e3).toFixed(1)}k`;
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
            }
            catch (e) {
                this.error = e instanceof Error ? e.message : String(e);
            }
            finally {
                this.busy = false;
            }
        },
        // Creating someone mints no key. A key is named for the machine it lives
        // on — that is what makes losing a laptop one row to delete instead of a
        // re-key of everything that person runs — and a key handed out at signup
        // is the one that gets deployed with no name at all. So ask where this
        // first one goes, in the same breath.
        add() {
            return this.act(async () => {
                const data = await this.api("POST", "/api/users", this.form);
                const who = data.user;
                this.form = { first_name: "", last_name: "", email: "" };
                const name = prompt(`Account created. Where will ${who.first_name}'s first key be used? (laptop, ci, fleet…)`, "laptop");
                if (!name)
                    return;
                const k = await this.api("POST", `/api/users/${who.id}/keys`, { name });
                this.showKey({ user: who, key: k.secret, name: k.key.name });
            });
        },
        // Adding a key never disturbs the existing ones, which is what makes
        // moving a machine across safe: add, deploy, then delete the old one.
        addKey(u) {
            const name = prompt(`Name for ${u.first_name}'s new key (laptop, ci, fleet…)`, "");
            if (!name)
                return;
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
            if (!confirm(`Delete ${u.first_name} ${u.last_name} <${u.email}>? Their key stops working.`))
                return;
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
            }
            catch {
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
            if (iso === null || iso === undefined)
                return "";
            const d = new Date(iso);
            if (Number.isNaN(d.getTime()))
                return String(iso);
            return d.toLocaleString([], { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });
        },
        // "resets 09:00" is only useful if you already know what time it is there.
        // What anybody actually wants is how long they have.
        untilLocal(iso) {
            if (iso === null || iso === undefined)
                return "";
            const d = new Date(iso);
            if (Number.isNaN(d.getTime()))
                return "";
            const mins = Math.round((d.getTime() - Date.now()) / 60000);
            if (mins <= 0)
                return "any moment";
            if (mins < 60)
                return `in ${mins} min`;
            const h = Math.floor(mins / 60);
            const m = mins % 60;
            return m ? `in ${h} h ${m} min` : `in ${h} h`;
        },
        ago(secs) {
            if (!secs)
                return "never";
            const d = Math.floor(Date.now() / 1000) - secs;
            if (d < 90)
                return "just now";
            if (d < 5400)
                return `${Math.floor(d / 60)} min ago`;
            if (d < 172800)
                return `${Math.floor(d / 3600)} h ago`;
            return `${Math.floor(d / 86400)} d ago`;
        },
    },
});
createApp(AdminApp).mount("#app");
