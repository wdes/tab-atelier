// SPDX-License-Identifier: MPL-2.0

/// The intentions pane ships as browser JS, and its one piece of real logic — the column split —
/// is invisible to `cargo test` unless it is exercised here. The failure it guards is concrete: a
/// divider dragged to either end leaves a 0 % column, which the grid renders as a strip of nothing
/// and the reader cannot recover from because the grip is inside the collapsed column.
///
/// The test asserts the property, not the implementation: whatever the rule becomes, no reachable
/// value may collapse a column.
#[test]
fn the_intent_split_never_collapses_a_column() {
    let script = r#"
const src = require("fs").readFileSync(process.argv[1], "utf8");
// Pull the two pure helpers out of the asset. Evaluating the whole file would need a DOM; these
// two are self-contained by design so the rule stays testable without one.
const clampSrc = src.match(/function clampIntentSplit[\s\S]*?\n}/);
if (!clampSrc) throw new Error("clampIntentSplit not found — was it renamed?");
const MIN_MATCH = src.match(/const INTENT_SPLIT_MIN = (\d+);/);
const MAX_MATCH = src.match(/const INTENT_SPLIT_MAX = (\d+);/);
const DEF_MATCH = src.match(/const INTENT_SPLIT_DEFAULT = (\d+);/);
if (!MIN_MATCH || !MAX_MATCH || !DEF_MATCH) throw new Error("split bounds not found");
const INTENT_SPLIT_MIN = Number(MIN_MATCH[1]);
const INTENT_SPLIT_MAX = Number(MAX_MATCH[1]);
// The default constant too: the function refers to it, so an eval without it is a ReferenceError —
// which is exactly how this test failed the first time.
const INTENT_SPLIT_DEFAULT = Number(DEF_MATCH[1]);
const DEF_MATCH_N = INTENT_SPLIT_DEFAULT;
eval(clampSrc[0]);

const cases = [
  [0, INTENT_SPLIT_MIN, "dragged fully left"],
  [100, INTENT_SPLIT_MAX, "dragged fully right"],
  [-40, INTENT_SPLIT_MIN, "dragged past the left edge"],
  [400, INTENT_SPLIT_MAX, "dragged past the right edge"],
  [null, DEF_MATCH_N, "nothing stored yet"],
  [undefined, DEF_MATCH_N, "nothing stored yet"],
  ["abc", DEF_MATCH_N, "a corrupted stored value"],
  [NaN, DEF_MATCH_N, "a corrupted stored value"],
  [50, 50, "a sensible value is kept as-is"],
  [38.4, 38, "a fraction is rounded, not truncated toward the bound"],
];
for (const [input, want, why] of cases) {
  const got = clampIntentSplit(input);
  if (got !== want) throw new Error(`${why}: clampIntentSplit(${input}) = ${got}, wanted ${want}`);
  if (got < INTENT_SPLIT_MIN || got > INTENT_SPLIT_MAX) {
    throw new Error(`${why}: ${got} is outside [${INTENT_SPLIT_MIN}, ${INTENT_SPLIT_MAX}]`);
  }
  if (!Number.isInteger(got)) throw new Error(`${why}: ${got} is not a whole percent`);
}
// The bounds themselves have to leave both columns usable, or the clamp would be clamping to a
// value that is already broken.
if (INTENT_SPLIT_MIN < 15 || INTENT_SPLIT_MAX > 85) {
  throw new Error(`bounds ${INTENT_SPLIT_MIN}/${INTENT_SPLIT_MAX} leave one column unusable`);
}
"#;
    let asset = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/kiosk.js");
    let out = std::process::Command::new("node")
        .arg("-e")
        .arg(script)
        .arg(asset)
        .output()
        .expect("node must be available to check the pane's split logic");
    assert!(
        out.status.success(),
        "kiosk.js split rule failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The pane has to be reachable from the tab shell: a function nobody calls ships as dead weight and
/// reads as a feature that does not work.
#[test]
fn the_intent_pane_is_wired_into_the_tab_shell() {
    let js = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/kiosk.js")).expect("read kiosk.js");
    assert!(
        js.contains(r#"panel("intent", intentPaneHtml())"#),
        "the intent tab must render the pane, not the bare form"
    );
    for fn_name in [
        "loadIntentions",
        "openIntention",
        "wireIntent",
        "sendIntentMessage",
        "intentAction",
    ] {
        assert!(
            js.contains(&format!("function {fn_name}")),
            "{fn_name} is missing — the pane would render but do nothing"
        );
    }
    // Idempotence matters: the panel element survives a re-render, so a listener bound twice would
    // fire a promote twice and create a second plan.
    assert!(
        js.contains(r#"el.dataset.intentWired === "1""#),
        "wireIntent must be idempotent or every re-render stacks a listener"
    );
    // Every fetch the pane makes has to address a route that exists.
    for (needle, why) in [
        ("/intent/list", "the list"),
        ("/intent/${encodeURIComponent(slug)}`", "the detail"),
        ("/ask`", "asking the worker"),
        ("/launch`", "launching the worker"),
        ("/kiosk/remotes", "finding a remote to launch on"),
    ] {
        assert!(js.contains(needle), "the pane never fetches {why} ({needle})");
    }
}
