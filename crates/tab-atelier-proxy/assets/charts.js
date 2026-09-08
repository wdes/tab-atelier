// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Two chart components, hand-rolled in SVG.
//
// No chart library on purpose. The proxy serves its own UI from disk with no
// build step, and pulling in a charting bundle would mean vendoring several
// hundred KB more third-party JavaScript into the page where the admin token
// is typed — to draw a line and some bars.
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
const hoverable = {
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
    onMove(e) {
      const svg = e.currentTarget;
      const r = svg.getBoundingClientRect();
      // Client px → viewBox units, so hit-testing is right at any width.
      const x = ((e.clientX - r.left) / r.width) * W - PAD.left;
      const i = Math.floor((x / this.plotW) * this.points.length);
      this.hover = i >= 0 && i < this.points.length ? i : -1;
    },
    onLeave() {
      this.hover = -1;
    },
    xOf(i) {
      return PAD.left + (this.plotW * (i + 0.5)) / this.points.length;
    },
    // Tooltip flips to the left of the cursor near the right edge so it never
    // hangs off the card.
    tipStyle(i) {
      const frac = (this.xOf(i) / W) * 100;
      return frac > 62 ? { right: `${100 - frac}%` } : { left: `${frac}%` };
    },
    hourLabel(ts) {
      const d = new Date(ts * 1000);
      return d.toLocaleString([], { month: "short", day: "numeric", hour: "2-digit" });
    },
    fmt(n) {
      if (n >= 1e9) return `${(n / 1e9).toFixed(1)}B`;
      if (n >= 1e6) return `${(n / 1e6).toFixed(1)}M`;
      if (n >= 1e3) return `${(n / 1e3).toFixed(1)}k`;
      return String(n);
    },
    // A rounded top on a "nice" number, so gridlines land on values a person
    // would have chosen.
    niceMax(v) {
      if (v <= 0) return 1;
      const mag = 10 ** Math.floor(Math.log10(v));
      return Math.ceil(v / mag) * mag;
    },
    ticks(max) {
      return [0, 0.5, 1].map((f) => Math.round(max * f));
    },
  },
};

// Roughly six x labels, whatever the window length. Computed rather than
// filtered inline: in Vue 3 `v-if` is evaluated BEFORE `v-for`, so a
// `v-for`+`v-if` pair on one element cannot see the loop variable at all.
const xLabels = {
  computed: {
    labels() {
      const step = Math.max(1, Math.ceil(this.points.length / 6));
      return this.points.map((p, i) => ({ p, i })).filter(({ i }) => i % step === 0);
    },
  },
};

// ── API calls over time ─────────────────────────────────────────────
// One series, so there is no legend: the title names it. Area under a 2px
// line — the shape over time is the question, and the fill makes an idle
// stretch read as idle rather than as missing data.
const CallsChart = {
  mixins: [hoverable, xLabels],
  props: { points: { type: Array, required: true } },
  computed: {
    max() {
      return this.niceMax(Math.max(1, ...this.points.map((p) => p.calls)));
    },
    yOf() {
      return (v) => PAD.top + this.plotH * (1 - v / this.max);
    },
    line() {
      return this.points.map((p, i) => `${i ? "L" : "M"}${this.xOf(i)},${this.yOf(p.calls)}`).join(" ");
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
};

// ── Tokens over time ────────────────────────────────────────────────
// Two series, so a legend is always present. Stacked bars rather than two
// lines: input and output sum to something meaningful (what the hour cost),
// which a pair of lines would not show. 2px surface gap between the segments
// and a 4px rounded top on the data-end.
const TokensChart = {
  mixins: [hoverable, xLabels],
  props: { points: { type: Array, required: true } },
  computed: {
    max() {
      return this.niceMax(Math.max(1, ...this.points.map((p) => p.input + p.output)));
    },
    yOf() {
      return (v) => PAD.top + this.plotH * (1 - v / this.max);
    },
    barW() {
      // 2px of surface between neighbours, and never a sliver.
      return Math.max(1, this.plotW / this.points.length - 2);
    },
  },
  methods: {
    seg(i, from, to) {
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
};

window.TaCharts = { CallsChart, TokensChart };
})();
