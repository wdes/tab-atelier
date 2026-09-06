// Self-check for the PURE zoom math of mesh zoom/pan (Jules, view-lane).
// Run: node assets/dashboard.mesh-zoom.test.mjs  (also picked up by `node --test`).
// meshZoomAt(t, cx, cy, factor) -> new {s,tx,ty}: scales around the svg-space cursor
// (cx,cy) keeping the point UNDER the cursor fixed, scale clamped to [min,max].
import assert from "node:assert/strict";
import { meshZoomAt } from "./dashboard.js";

const near = (a, b, eps = 1e-9) => Math.abs(a - b) <= eps;
// where a model point p renders on screen under transform t: t.tx + t.s*p
const screenOf = (t, p) => t.tx + t.s * p;
// the model point currently under the cursor cx for transform t
const modelUnder = (t, cx) => (cx - t.tx) / t.s;

// identity: factor 1 changes nothing
{
  const t = { s: 1.5, tx: 20, ty: -30 };
  const t2 = meshZoomAt(t, 100, 100, 1);
  assert.deepEqual(t2, { s: 1.5, tx: 20, ty: -30 }, "factor 1 -> unchanged");
}

// zoom-in keeps the point under the cursor fixed (x AND y)
{
  const t = { s: 1, tx: 0, ty: 0 };
  const cx = 100, cy = 250, f = 2;
  const mx = modelUnder(t, cx), my = modelUnder({ s: t.s, tx: t.ty, ty: 0 }, cy); // (cy-ty)/s
  const t2 = meshZoomAt(t, cx, cy, f);
  assert.ok(near(t2.s, 2), "scale *2");
  assert.ok(near(screenOf(t2, mx), cx), "point under cursor stays put in x");
  assert.ok(near(t2.ty + t2.s * my, cy), "point under cursor stays put in y");
}

// zoom-out from a panned/zoomed state still fixes the cursor point
{
  const t = { s: 3, tx: -140, ty: 55 };
  const cx = 300, cy = 120, f = 0.5;
  const mx = (cx - t.tx) / t.s, my = (cy - t.ty) / t.s;
  const t2 = meshZoomAt(t, cx, cy, f);
  assert.ok(near(t2.s, 1.5), "scale *0.5 -> 1.5");
  assert.ok(near(t2.tx + t2.s * mx, cx) && near(t2.ty + t2.s * my, cy), "cursor point fixed after zoom-out");
}

// clamp: cannot exceed max nor drop below min
{
  const hi = meshZoomAt({ s: 7, tx: 0, ty: 0 }, 10, 10, 100, 0.2, 8);
  assert.ok(near(hi.s, 8), "scale clamped to max");
  const lo = meshZoomAt({ s: 0.3, tx: 0, ty: 0 }, 10, 10, 0.001, 0.2, 8);
  assert.ok(near(lo.s, 0.2), "scale clamped to min");
  // at the clamp the cursor point is STILL fixed (k = clampedS / s)
  const k = 8 / 7, cx = 10;
  assert.ok(near(hi.tx, cx - k * (cx - 0)), "clamped zoom still cursor-centered");
}

console.log("dashboard.mesh-zoom.test.mjs: OK");
