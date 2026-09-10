// SPDX-License-Identifier: MPL-2.0
//
// Two chart components, hand-rolled in SVG.
//
// SOURCE. The file the proxy serves is ../assets/charts.js, compiled from
// here — edit this one, and see web/README.md.
//
// No chart library on purpose. Pulling in a charting bundle would mean
// vendoring several hundred KB more third-party JavaScript into the page where
// the admin token is typed — to draw a line and some bars.
//
// Colours come from CSS custom properties (see the <style> block in
// index.html), not from hex literals here, so light and dark are chosen from
// the same ramps in one place. The pair in use — blue #2a78d6 / orange #eb6834
// in light, #3987e5 / #d95926 in dark — was checked with the palette validator
// in both modes: lightness band, chroma floor, CVD separation (ΔE 24.7 protan
// worst case), normal-vision separation and contrast all pass.

(function () {
"use strict";

const PAD = { top: 10, right: 10, bottom: 22, left: 48 };
const W = 720;
const H = 200;

// Shared by both charts: mouse position → bucket index, the crosshair, and the
// tooltip. Hover is not optional decoration — an hourly series is unreadable
// without a way to ask "which hour is that, exactly".
const hoverable = Vue.defineComponent({
  // Declared here because the mixin uses it: `onMove` maps a pixel to an index
  // in this array, and `xOf` divides by its length. A mixin that silently
  // assumed a host property would be a runtime crash waiting for the one host
  // that forgot.
  props: { points: { type: Array as () => readonly ChartPoint[], required: true } },
  data() {
    return { hover: -1 };
  },
  computed: {
    plotW() {
      return W - PAD.left - PAD.right;
    },
    plotH() {
      return H - PAD.top - PAD.bottom;
    },
  },
  methods: {
    onMove(e: MouseEvent) {
      // Bound only to the <svg>, which is what makes this cast safe.
      const svg = e.currentTarget as SVGSVGElement;
      const r = svg.getBoundingClientRect();
      // Client px → viewBox units, so hit-testing is right at any width.
      const x = ((e.clientX - r.left) / r.width) * W - PAD.left;
      const i = Math.floor((x / this.plotW) * this.points.length);
      this.hover = i >= 0 && i < this.points.length ? i : -1;
    },
    onLeave() {
      this.hover = -1;
    },
    xOf(i: number): number {
      return PAD.left + (this.plotW * (i + 0.5)) / this.points.length;
    },
    // Tooltip flips to the left of the cursor near the right edge so it never
    // hangs off the card.
    tipStyle(i: number): Record<string, string> {
      const frac = (this.xOf(i) / W) * 100;
      return frac > 62 ? { right: `${100 - frac}%` } : { left: `${frac}%` };
    },
    hourLabel(ts: number): string {
      const d = new Date(ts * 1000);
      return d.toLocaleString([], { month: "short", day: "numeric", hour: "2-digit" });
    },
    fmt(n: number): string {
      if (n >= 1e9) return `${(n / 1e9).toFixed(1)}B`;
      if (n >= 1e6) return `${(n / 1e6).toFixed(1)}M`;
      if (n >= 1e3) return `${(n / 1e3).toFixed(1)}k`;
      return String(n);
    },
    // A rounded top on a "nice" number, so gridlines land on values a person
    // would have chosen.
    niceMax(v: number): number {
      if (v <= 0) return 1;
      const mag = 10 ** Math.floor(Math.log10(v));
      return Math.ceil(v / mag) * mag;
    },
    ticks(max: number): number[] {
      return [0, 0.5, 1].map((f) => Math.round(max * f));
    },
  },
});

// Roughly six x labels, whatever the window length. Computed rather than
// filtered inline: in Vue 3 `v-if` is evaluated BEFORE `v-for`, so a
// `v-for`+`v-if` pair on one element cannot see the loop variable at all.
const xLabels = Vue.defineComponent({
  props: { points: { type: Array as () => readonly ChartPoint[], required: true } },
  computed: {
    labels() {
      const step = Math.max(1, Math.ceil(this.points.length / 6));
      return this.points.map((p, i) => ({ p, i })).filter(({ i }) => i % step === 0);
    },
  },
});

// ── API calls over time ─────────────────────────────────────────────
// One series, so there is no legend: the title names it. Area under a 2px
// line — the shape over time is the question, and the fill makes an idle
// stretch read as idle rather than as missing data.
const CallsChart = Vue.defineComponent({
  mixins: [hoverable, xLabels],
  props: { points: { type: Array as () => readonly UsageBucket[], required: true } },
  computed: {
    // The mixins declare `points` as the union they can plot, and Vue
    // merges their props ahead of this component's. `pts` undoes that
    // widening once, here, rather than casting at every use.
    pts(): readonly UsageBucket[] {
      return this.points as readonly UsageBucket[];
    },
    max() {
      return this.niceMax(Math.max(1, ...this.pts.map((p) => p.calls)));
    },
    yOf() {
      return (v: number) => PAD.top + this.plotH * (1 - v / this.max);
    },
    line() {
      return this.pts.map((p, i) => `${i ? "L" : "M"}${this.xOf(i)},${this.yOf(p.calls)}`).join(" ");
    },
    area() {
      if (!this.points.length) return "";
      const base = PAD.top + this.plotH;
      return `${this.line} L${this.xOf(this.points.length - 1)},${base} L${this.xOf(0)},${base} Z`;
    },
  },
  template: `
    <div class="ta-chart">
      <svg :viewBox="'0 0 ' + ${W} + ' ' + ${H}" @mousemove="onMove" @mouseleave="onLeave" role="img"
           aria-label="API calls per hour">
        <g class="ta-grid">
          <line v-for="t in ticks(max)" :key="'g'+t"
                :x1="${PAD.left}" :x2="${W - PAD.right}" :y1="yOf(t)" :y2="yOf(t)" />
          <text v-for="t in ticks(max)" :key="'l'+t" :x="${PAD.left - 8}" :y="yOf(t) + 4"
                text-anchor="end">{{ fmt(t) }}</text>
        </g>
        <path :d="area" class="ta-area-1" />
        <path :d="line" class="ta-line-1" />
        <g v-if="hover >= 0">
          <line class="ta-crosshair" :x1="xOf(hover)" :x2="xOf(hover)" :y1="${PAD.top}" :y2="${H - PAD.bottom}" />
          <circle :cx="xOf(hover)" :cy="yOf(points[hover].calls)" r="5" class="ta-dot-1" />
        </g>
        <g class="ta-axis">
          <text v-for="l in labels" :key="'x'+l.i"
                :x="xOf(l.i)" :y="${H - 6}" text-anchor="middle">{{ hourLabel(l.p.hour) }}</text>
        </g>
      </svg>
      <div v-if="hover >= 0" class="ta-tip" :style="tipStyle(hover)">
        <div class="ta-tip-h">{{ hourLabel(points[hover].hour) }}</div>
        <div><span class="ta-key ta-bg-1"></span>{{ points[hover].calls }} calls</div>
        <div v-if="points[hover].errors" class="ta-tip-err">{{ points[hover].errors }} failed</div>
      </div>
    </div>`,
});

// ── Tokens over time ────────────────────────────────────────────────
// Two series, so a legend is always present. Stacked bars rather than two
// lines: input and output sum to something meaningful (what the hour cost),
// which a pair of lines would not show. 2px surface gap between the segments
// and a 4px rounded top on the data-end.
const TokensChart = Vue.defineComponent({
  mixins: [hoverable, xLabels],
  props: { points: { type: Array as () => readonly UsageBucket[], required: true } },
  computed: {
    // The mixins declare `points` as the union they can plot, and Vue
    // merges their props ahead of this component's. `pts` undoes that
    // widening once, here, rather than casting at every use.
    pts(): readonly UsageBucket[] {
      return this.points as readonly UsageBucket[];
    },
    max() {
      return this.niceMax(Math.max(1, ...this.pts.map((p) => p.input + p.output)));
    },
    yOf() {
      return (v: number) => PAD.top + this.plotH * (1 - v / this.max);
    },
    barW() {
      // 2px of surface between neighbours, and never a sliver.
      return Math.max(1, this.plotW / this.points.length - 2);
    },
  },
  methods: {
    seg(i: number, from: number, to: number) {
      const y = this.yOf(to);
      const h = Math.max(0, this.yOf(from) - y);
      return { x: this.xOf(i) - this.barW / 2, y, width: this.barW, height: h };
    },
  },
  template: `
    <div class="ta-chart">
      <svg :viewBox="'0 0 ' + ${W} + ' ' + ${H}" @mousemove="onMove" @mouseleave="onLeave" role="img"
           aria-label="Input and output tokens per hour">
        <g class="ta-grid">
          <line v-for="t in ticks(max)" :key="'g'+t"
                :x1="${PAD.left}" :x2="${W - PAD.right}" :y1="yOf(t)" :y2="yOf(t)" />
          <text v-for="t in ticks(max)" :key="'l'+t" :x="${PAD.left - 8}" :y="yOf(t) + 4"
                text-anchor="end">{{ fmt(t) }}</text>
        </g>
        <g v-for="(p, i) in points" :key="'b'+i">
          <!-- output sits on top, so the rounded data-end belongs to it -->
          <rect v-if="p.input" v-bind="seg(i, 0, p.input)" class="ta-bar-1" />
          <rect v-if="p.output" v-bind="seg(i, p.input + 2 * max / ${H}, p.input + p.output)"
                class="ta-bar-2" rx="4" />
        </g>
        <line v-if="hover >= 0" class="ta-crosshair" :x1="xOf(hover)" :x2="xOf(hover)"
              :y1="${PAD.top}" :y2="${H - PAD.bottom}" />
        <g class="ta-axis">
          <text v-for="l in labels" :key="'x'+l.i"
                :x="xOf(l.i)" :y="${H - 6}" text-anchor="middle">{{ hourLabel(l.p.hour) }}</text>
        </g>
      </svg>
      <div v-if="hover >= 0" class="ta-tip" :style="tipStyle(hover)">
        <div class="ta-tip-h">{{ hourLabel(points[hover].hour) }}</div>
        <div><span class="ta-key ta-bg-1"></span>{{ fmt(points[hover].input) }} in</div>
        <div><span class="ta-key ta-bg-2"></span>{{ fmt(points[hover].output) }} out</div>
        <div v-if="points[hover].cache_read" class="ta-tip-sub">
          {{ fmt(points[hover].cache_read) }} cache read
        </div>
      </div>
    </div>`,
});

// ── Plan pressure over time ─────────────────────────────────────────
// One series (utilisation), so no legend — the title names it. The thing that
// makes this chart worth drawing is the THRESHOLD rule: a number on its own
// does not tell you whether you are about to be degraded, and the distance to
// that line is the actual question.
// Below this the five-hour window counts as reset. See `sessionResets`.
const RESET_EPS = 0.01;

const PressureChart = Vue.defineComponent({
  mixins: [hoverable, xLabels],
  props: {
    points: { type: Array as () => readonly PressurePoint[], required: true },
    threshold: { type: Number, default: 0.85 },
  },
  computed: {
    // The mixins declare `points` as the union they can plot, and Vue
    // merges their props ahead of this component's. `pts` undoes that
    // widening once, here, rather than casting at every use.
    pts(): readonly PressurePoint[] {
      return this.points as readonly PressurePoint[];
    },
    // Always full scale: utilisation is a fraction of a fixed thing, and
    // rescaling to the data would make 12% look alarming.
    max() {
      return 1;
    },
    yOf() {
      return (v: number) => PAD.top + this.plotH * (1 - Math.min(v, 1) / this.max);
    },
    line() {
      return this.pts
        .map((p, i) => `${i ? "L" : "M"}${this.xOf(i)},${this.yOf(p.util ?? 0)}`)
        .join(" ");
    },
    area() {
      if (!this.points.length) return "";
      const base = PAD.top + this.plotH;
      return `${this.line} L${this.xOf(this.points.length - 1)},${base} L${this.xOf(0)},${base} Z`;
    },
    // The weekly window, drawn behind the five-hour one. Same axis, because
    // both are a fraction of their OWN window — a second y-scale here would be
    // the classic dual-axis lie, inviting comparison of two numbers that share
    // no denominator.
    //
    // Gaps are breaks, not zeroes: a failed poll carries no weekly figure, and
    // joining across it would draw a decline that never happened.
    weeklySegments() {
      const out: string[] = [];
      let run: string[] = [];
      this.pts.forEach((p, i) => {
        if (p.seven_day == null) {
          if (run.length > 1) out.push(run.join(" "));
          run = [];
          return;
        }
        run.push(`${run.length ? "L" : "M"}${this.xOf(i)},${this.yOf(p.seven_day)}`);
      });
      if (run.length > 1) out.push(run.join(" "));
      return out;
    },
    hasWeekly() {
      return this.pts.some((p) => p.seven_day != null);
    },
    // Where the five-hour window rolled over: utilisation fell from above 1%
    // to at or below it between two readings. Without these the line is one
    // continuous sawtooth with no way to see where a session window ends and
    // the next begins — and that window is the unit the limit is enforced in.
    //
    // A threshold rather than `=== 0` because the window is only visible
    // through five-minute polls: the odds of sampling the instant it reads
    // exactly zero are poor, and a reset caught at 0.4% is still a reset.
    sessionResets() {
      const out = [];
      for (let i = 1; i < this.points.length; i++) {
        const before = this.pts[i - 1]!.util;
        const now = this.pts[i]!.util;
        if (before == null || now == null) continue;
        if (before > RESET_EPS && now <= RESET_EPS) out.push(i);
      }
      return out;
    },
  },
  methods: {
    pct(v: number | null | undefined): string {
      return `${Math.round((v ?? 0) * 100)}%`;
    },
  },
  template: `
    <div class="ta-chart">
      <svg :viewBox="'0 0 ' + ${W} + ' ' + ${H}" @mousemove="onMove" @mouseleave="onLeave" role="img"
           aria-label="Share of the plan used over time">
        <g class="ta-grid">
          <line v-for="t in [0, 0.5, 1]" :key="'g'+t"
                :x1="${PAD.left}" :x2="${W - PAD.right}" :y1="yOf(t)" :y2="yOf(t)" />
          <text v-for="t in [0, 0.5, 1]" :key="'l'+t" :x="${PAD.left - 8}" :y="yOf(t) + 4"
                text-anchor="end">{{ pct(t) }}</text>
        </g>
        <!-- Session-window boundaries: annotation, so they sit behind the data. -->
        <g class="ta-reset">
          <line v-for="i in sessionResets" :key="'r'+i"
                :x1="xOf(i)" :x2="xOf(i)" :y1="${PAD.top}" :y2="${H - PAD.bottom}" />
        </g>
        <!-- The weekly window, behind the main line and dashed. The dash is a
             second channel carrying the same identity as the hue, so the two
             series stay apart in greyscale, in print and for a CVD reader. -->
        <path v-for="(d, i) in weeklySegments" :key="'w'+i" :d="d" class="ta-line-2-bg" />
        <path :d="area" class="ta-area-1" />
        <path :d="line" class="ta-line-1" />
        <!-- Where fallback begins. Labelled, not just coloured. -->
        <g class="ta-threshold">
          <line :x1="${PAD.left}" :x2="${W - PAD.right}" :y1="yOf(threshold)" :y2="yOf(threshold)" />
          <text :x="${W - PAD.right}" :y="yOf(threshold) - 5" text-anchor="end">
            fallback above {{ pct(threshold) }}
          </text>
        </g>
        <g v-if="hover >= 0">
          <line class="ta-crosshair" :x1="xOf(hover)" :x2="xOf(hover)" :y1="${PAD.top}" :y2="${H - PAD.bottom}" />
          <circle v-if="points[hover].seven_day != null" :cx="xOf(hover)"
                  :cy="yOf(points[hover].seven_day)" r="4" class="ta-dot-2" />
          <circle :cx="xOf(hover)" :cy="yOf(points[hover].util ?? 0)" r="5" class="ta-dot-1" />
        </g>
        <g class="ta-axis">
          <text v-for="l in labels" :key="'x'+l.i"
                :x="xOf(l.i)" :y="${H - 6}" text-anchor="middle">{{ l.p.label }}</text>
        </g>
      </svg>
      <!-- Two series means a legend, always: identity must not rest on hue. -->
      <div class="small text-body-secondary mt-1">
        <span class="me-3"><span class="ta-key ta-bg-1"></span>session (5 h)</span>
        <span v-if="hasWeekly" class="me-3"><span class="ta-key ta-key-dash ta-bg-2"></span>weekly</span>
        <span v-if="sessionResets.length"><span class="ta-key ta-key-rule"></span>session reset</span>
      </div>
      <div v-if="hover >= 0" class="ta-tip" :style="tipStyle(hover)">
        <div class="ta-tip-h">{{ points[hover].label }}</div>
        <div><span class="ta-key ta-bg-1"></span>{{ pct(points[hover].util) }} session (5 h)</div>
        <div v-if="points[hover].seven_day != null">
          <span class="ta-key ta-key-dash ta-bg-2"></span>{{ pct(points[hover].seven_day) }} weekly
        </div>
      </div>
    </div>`,
});

window.TaCharts = { CallsChart, TokensChart, PressureChart };
})();
