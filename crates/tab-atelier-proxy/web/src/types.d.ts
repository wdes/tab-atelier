// SPDX-License-Identifier: MPL-2.0
//
// The wire shapes, declared once.
//
// These mirror what `crates/tab-atelier-proxy/src/server.rs` serialises. They
// are hand-written rather than generated because there are a dozen of them and
// a generator would be a second build step for the sake of a file that changes
// when the API does — which is exactly when you want to be made to look at it.
//
// Ambient (`.d.ts`, no import/export) because the compiled output is two plain
// <script> tags sharing one global scope. `"module": "none"` in tsconfig makes
// an accidental `import` a compile error rather than a runtime one.

/** A key belongs to a place, not a person — see docs/proxy.md. */
interface ApiKey {
    id: string;
    name: string;
    created_at: number;
    first_used_at: number | null;
    last_used_at: number | null;
    last_used_ip: string | null;
    disabled: boolean;
}

interface ApiUser {
    id: string;
    first_name: string;
    last_name: string;
    email: string;
    created_at: number;
    disabled: boolean;
    /** The scheduler's word for priority; the UI shows a tier name instead. */
    weight: number;
    keys: ApiKey[];
    last_used_at: number | null;
    has_key: boolean;
    /** The provider this account's work is pinned to, if any. */
    provider: string | null;
    /**
     * This account's compaction level. Per ACCOUNT, not per provider: routing
     * picks the provider per request, so a level filed under one would quietly
     * mean something else the moment traffic stopped going there.
     */
    compact: string;
    /**
     * This account's tool policy. Per ACCOUNT for the same reason `compact`
     * above is: routing picks the hop per request.
     */
    tools: ToolsPolicy;
}

/**
 * Which of a request's tool definitions are sent on.
 *
 * `mode` decides the base set and `disable` subtracts from it, but neither is
 * the last word: a name the conversation has already called, or that
 * `tool_choice` forces, is kept whatever the policy says. That rule is the
 * server's (`tools.rs`), not this file's — the UI must not re-derive it, or
 * the two will disagree about what a policy does.
 */
interface ToolsPolicy {
    mode: "all" | "referenced" | "allow" | "none";
    /** Names removed, in every mode. */
    disable: string[];
    /** The complete set, read only by `mode: "allow"`. */
    allow: string[];
    /** Definitions injected when the client did not send them. */
    add: unknown[];
}

interface TokenTotals {
    input: number;
    output: number;
    cache_read: number;
    cache_write: number;
    total: number;
}

interface UsageWindow {
    calls: number;
    errors: number;
    tokens: TokenTotals;
}

/** One hour of one account's usage. */
interface UsageBucket {
    hour: number;
    calls: number;
    errors: number;
    input: number;
    output: number;
    cache_read: number;
    cache_write: number;
}

/**
 * One account's calls-per-hour, as a line of its own.
 *
 * `slot` is the categorical colour, assigned by the CALLER from a stable order
 * rather than by the chart. Colour follows the entity, never its rank: if the
 * chart picked slots by size, a busy hour would repaint every line on the page,
 * and focusing one account would recolour all the others. `-1` is the folded
 * "Other" group, which gets muted ink because it is not an entity.
 */
interface UserSeries {
    id: string;
    name: string;
    slot: number;
    points: UsageBucket[];
}

interface AccountUsage {
    user: ApiUser;
    last_24h: UsageWindow;
    last_7d: UsageWindow;
    all_time: UsageWindow;
    series_hourly: UsageBucket[];
    /**
     * The requested window, as the server resolved it.
     *
     * Fixed windows sit beside it rather than inside it: `last_24h` and
     * `last_7d` are what the account table reads and must mean the same thing
     * whatever the graph is zoomed to, while this is the graph's own range.
     */
    window: UsageSpan;
}

interface UsageSpan {
    start: string;
    end: string;
    hours: number;
}

/**
 * The dashboard's usage response.
 *
 * Distinct from `AccountUsage` because the window the server actually served
 * is a property of the response, not of any one account.
 */
interface UsageResponse {
    /** The canonical token — not necessarily the one that was sent. */
    window: string;
    hours: number;
    window_start: string;
    window_end: string;
    users: AccountUsage[];
}

/**
 * One reading of the shared plan.
 *
 * `seven_day_resets` is upstream's claim about the weekly window and is
 * deliberately not treated as the moment fresh allocation arrives — see
 * `Sample::seven_day_resets` in account.rs.
 */
interface PlanSample {
    ts: string;
    http?: number;
    five_hour?: number | null;
    five_hour_resets?: string | null;
    seven_day?: number | null;
    seven_day_resets?: string | null;
    seven_day_sonnet?: number | null;
    error?: string;
}

interface PlanHealth {
    stale: boolean;
    last_ok_ts?: string;
    last_ok_age_secs?: number;
    consecutive_failures?: number;
}

interface WeeklyDrop {
    ts: string;
    from: number;
    to: number;
}

interface Plan {
    utilization: number | null;
    latest: PlanSample | null;
    health: PlanHealth;
    weekly_last_drop: WeeklyDrop | null;
    history: PlanSample[];
}

/**
 * The QoS scheduler's snapshot.
 *
 * NOT called `Scheduler`: the DOM lib already declares a global interface of
 * that name, and an ambient `interface Scheduler` here MERGED with it rather
 * than shadowing it — so the type silently gained `postTask` and `yield` and
 * stopped matching anything the API returns. Declaration merging is a feature;
 * in a global .d.ts beside `lib.dom` it is a trap.
 */
interface PlanScheduler {
    backoff_for?: number;
    rejected?: number;
}

interface Pressure {
    plan: Plan;
    scheduler: PlanScheduler;
    degrade_above?: number;
    floor_above?: number;
    strained_above?: number;
}

/** One captured request, as sent to Anthropic. Credentials are already gone. */
interface CaptureTokens {
    input: number;
    output: number;
    cache_read: number;
    cache_write: number;
}

interface Capture {
    ts: string;
    account_id: string;
    account_email: string;
    method: string;
    path: string;
    provider: string;
    /** The model actually requested upstream, not necessarily the one asked for. */
    model?: string;
    /**
     * Which client session sent this, unpacked from the body's `metadata`.
     *
     * Client-supplied and unauthenticated: it groups and explains, and must
     * never be read as a fact about who is calling. Absent when the client sent
     * nothing usable — most often `account_uuid: ""`, which is omitted rather
     * than rendered as blank.
     */
    client?: CaptureClient;
    /** How the request arrived, as the proxy saw it. */
    origin?: CaptureOrigin;
    /**
     * `"work"` or `"classifier"`. Always present — the server defaults it,
     * so a capture written before the field existed still reads as work.
     */
    kind: string;
    request_headers: [string, string][];
    request_body: string;
    request_truncated: boolean;
    status?: number;
    tokens?: CaptureTokens;
    /** What compaction removed, when a level was in force for this request. */
    compaction?: CaptureCompaction;
    response_excerpt?: string;
}

/**
 * What the client said about itself, from the body's `metadata.user_id`.
 *
 * That field is a JSON document stored inside a JSON string, because every
 * `metadata` value has to be a string. All three members are optional on the
 * wire and absent when empty: Claude Code sends `account_uuid: ""` for a
 * session that is not logged in, and the server reports that as missing rather
 * than as a value.
 *
 * Unauthenticated — anything with a valid key can put any string here. Display
 * it to group and explain; never to decide.
 */
interface CaptureClient {
    /** Stable per install. Answers "same machine, or two?". */
    device_id?: string;
    /** Per conversation — what tells one tab from four others on one key. */
    session_id?: string;
    /** The CLIENT's Anthropic account, frequently empty. Not this proxy's. */
    account_uuid?: string;
}

/**
 * How a request arrived, as the proxy saw it.
 *
 * Exists to settle "which IP is it using" from data rather than from a
 * reading of the code: it records the socket peer AND the forwarding headers
 * as received, beside the one the proxy actually resolved.
 */
interface CaptureOrigin {
    peer: string;
    /** False explains a result that ignored a header the operator can see. */
    peer_trusted: boolean;
    /** What the proxy resolved, and therefore logged against the key. */
    client_ip: string;
    x_real_ip?: string;
    x_forwarded_for?: string;
}

/**
 * What one compaction pass removed.
 *
 * Both byte counts rather than a percentage: the ratio is arithmetic the
 * reader can do, and a stale percentage is harder to notice than two sizes
 * that do not look right.
 */
interface CaptureCompaction {
    level: string;
    bytes_before: number;
    bytes_after: number;
    tool_results_elided: number;
    tool_results_kept_for_error: number;
    thinking_dropped: number;
    /**
     * Layer D: file bodies stubbed inside old `Write`/`Edit` inputs.
     *
     * Optional because `inspect.jsonl` holds captures written before the
     * field existed, and a file that fails to parse costs the whole history.
     */
    writes_elided?: number;
    banners_dropped: number;
}

/** One model a provider lists. */
interface ProviderModel {
    id: string;
    class: string;
    relative_cost: number;
    cost_now: number;
    deprecated: boolean;
    note?: string;
}

interface ProviderView {
    id: string;
    base_url: string;
    preference: number;
    enabled: boolean;
    peak_now: boolean;
    peak?: { multiplier_percent: number; windows: unknown[] };
    ready: boolean;
    auth: string;
    /**
     * Why compaction through THIS hop would only cost money, or null when it
     * would not.
     *
     * Level-independent: the harm is a property of the provider, so any
     * non-`none` level meets it equally. The level itself is per ACCOUNT — see
     * `ApiUser.compact` — and the server raises this refusal against every
     * destination an account could reach when its level is set. Kept here as
     * well so the reason is inspectable per hop without a save attempt.
     */
    compact_refusal: string | null;
    models: ProviderModel[];
}

/** One compaction level, keyed and labelled by the server so the wording
 *  lives beside the enum that routing actually reads. */
interface CompactLevel {
    value: string;
    label: string;
}

interface PresetView {
    id: string;
    label: string;
    base_url: string;
    configured: boolean;
}

interface MappingView {
    from: string;
    to: string;
    note?: string;
}

interface ProvidersResponse {
    providers: ProviderView[];
    presets: PresetView[];
    mappings: MappingView[];
    compact_levels: CompactLevel[];
}

interface InspectState {
    armed: boolean;
    armed_until: number;
    seconds_left: number;
    max_arm_minutes: number;
    captures: Capture[];
}

/**
 * Anything the shared hover/label mixins can plot.
 *
 * The mixins genuinely depend on their host providing `points`, so they
 * declare the prop themselves rather than reaching for one they hope exists.
 * Each chart re-declares it with its own concrete element type.
 */
type ChartPoint = UsageBucket | PressurePoint;

/** A point on the pressure chart. */
interface PressurePoint {
    util: number | null;
    seven_day?: number | null;
    label: string;
}

/** What `POST /api/users/<id>/keys` returns: the row, plus the secret once. */
interface NewKey {
    key: ApiKey;
    secret: string;
}

/** A freshly minted key, readable exactly once. */
interface FreshKey {
    name: string;
    who: string;
    key: string;
}

/** A priority rung, as the UI names it. */
interface Tier {
    weight: number;
    label: string;
}

/**
 * The admin app's reactive state.
 *
 * Named and annotated on `data()` rather than left to inference. Vue's Options
 * API infers `this` from the data literal, the computed block and the methods
 * block at once; with a literal this size it gives up silently and hands back
 * `any` in methods and `{}` in computed, which reads as "fully typed" while
 * checking nothing at all. An explicit return type is what keeps the rest of
 * the file honest.
 */
interface AppState {
    token: string;
    authed: boolean;
    users: ApiUser[];
    form: { first_name: string; last_name: string; email: string };
    freshKey: FreshKey | null;
    copied: boolean;
    error: string;
    busy: boolean;
    /**
     * Per-chart, rather than the page-wide `busy`: reloading a chart is no
     * reason to grey out the account table. Calls and tokens share one because
     * they share a fetch — when either is reloaded, both really are.
     */
    usageBusy: boolean;
    pressureBusy: boolean;
    providersBusy: boolean;
    inspectBusy: boolean;
    origin: string;
    usage: Record<string, AccountUsage>;
    /**
     * The graph's time range, as a `usage::Window` token.
     *
     * A string rather than an hour count: "this week" is the current calendar
     * week, so no number expresses it. The server canonicalises what comes back.
     */
    usageWindow: string;
    focus: string | null;
    pressure: Pressure | null;
    pressureTimer: ReturnType<typeof setInterval> | null;
    tiers: Tier[];
    inspect: InspectState | null;
    inspectOpen: Capture | null;
    providers: ProvidersResponse | null;
    newMapping: { from: string; to: string; note: string };
    /**
     * The tool policy being edited, or null when no account has its editor
     * open — one at a time, because two half-written policies on screen is how
     * the wrong one gets saved.
     *
     * The lists are held as TEXT, not as `string[]`, because they are typed
     * into a field and a half-typed `Bash, Ed` is a legitimate intermediate
     * state — a split-on-keystroke would fight the typist. The split happens
     * once, on save, and the wire shape sent is the stored shape exactly.
     */
    tools: ToolsDraft | null;
}

interface ToolsDraft {
    id: string;
    mode: ToolsPolicy["mode"];
    disable: string;
    allow: string;
    /** JSON, as typed. Parsed in `saveTools`, so a syntax error is reported
     *  against the field the typist is looking at. */
    add: string;
}

/**
 * The vendored Vue 3 global build.
 *
 * `typeof import("vue")` is a TYPE-level import inside an ambient declaration:
 * it pulls Vue's own types in — so `defineComponent` infers `this` across data,
 * computed and methods — while emitting nothing and leaving this file global.
 * That is what lets the output stay two plain <script> tags with no module
 * loader, no bundler and no runtime dependency on the npm package, which is
 * present only as a devDependency for these types.
 */
declare const Vue: typeof import("vue");

/**
 * What `charts.js` hands to `app.js` across the global scope.
 *
 * `Component` rather than `ReturnType<typeof defineComponent>`: the latter is
 * an enormous inferred type, and naming it inside the options object defeated
 * inference for the WHOLE component — `this.usage` came back as `{}` and forty
 * downstream annotations went implicit-any.
 */
interface Window {
    TaCharts: {
        CallsChart: import("vue").Component;
        TokensChart: import("vue").Component;
        PressureChart: import("vue").Component;
    };
}
