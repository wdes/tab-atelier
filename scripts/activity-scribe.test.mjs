// Deterministic self-check for the activity SCRIBE (docs/dashboard-increment-5.md
// S3). The scribe aggregates recent agent activity into
// `$ACTIVITY_STATE_DIR/activity.json`, which the Rust route `/dashboard/activity`
// (S2) serves and the "Dernières heures" panel (S4) renders.
//
// Run:  node scripts/activity-scribe.test.mjs   (exits non-zero on any failure)
// No framework — node:assert only. Drives the REAL scribe as a subprocess against
// tiny committed JSONL fixtures + a throwaway git repo, with a fixed ACTIVITY_NOW
// so every number is exact. RED until `scripts/activity-scribe` exists — the
// scribe builder makes it green. Builder: scribe.
//
// Contract the scribe MUST honour (env-driven so it is testable AND its feature
// whitelist stays DYNAMIC — never a hardcoded repo list):
//   ACTIVITY_PROJECTS_DIR  dir of *.jsonl transcripts to aggregate
//   ACTIVITY_STATE_DIR     dir holding swamp.jsonl (if any) + where activity.json is WRITTEN
//   ACTIVITY_ORCH_REPOS    comma-separated repo PATHS = the orchestrator whitelist
//                          (in prod: derived from live tabs role=orchestrator -> their cwd)
//   ACTIVITY_NOW           ISO-8601 "now" override (deterministic durations)
//   ACTIVITY_WINDOW_HOURS  aggregation window (default 24)

// Fixtures include a `sessionC.jsonl` of automated ticks dispatched AS typed
// (a `RESTART mx imminent` broadcast, a `Watcher restart`, and two `Ronde …`
// rounds — one at 14:59) plus a `system`-source round. NONE are human: they must
// NOT count toward human_prompts NOR shrink minutes_since_last / autonomy. They
// carry no assistant records, so the token totals below are unchanged — which is
// why human_prompts===3 / minutes===60 / autonomy===270 double as the cron-
// exclusion proof (they would be 5 / 1 / small if the ticks leaked in).
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const SCRIBE = join(HERE, "activity-scribe"); // stable extensionless entrypoint (shebang inside)
const FIXTURES = join(HERE, "activity-scribe.fixtures", "projects");
const NOW = "2026-08-23T15:00:00.000Z";

// --- a throwaway git repo carrying 2 in-window `feat(` commits, 1 `fix(`, and 1
//     out-of-window `feat(` (dated before the 24h window) to prove windowing. ---
function makeFeatRepo() {
  const repo = mkdtempSync(join(tmpdir(), "scribe-repo-"));
  const git = (args, date) =>
    execFileSync("git", ["-C", repo, ...args], {
      env: {
        ...process.env,
        GIT_AUTHOR_NAME: "t", GIT_AUTHOR_EMAIL: "t@t",
        GIT_COMMITTER_NAME: "t", GIT_COMMITTER_EMAIL: "t@t",
        ...(date ? { GIT_AUTHOR_DATE: date, GIT_COMMITTER_DATE: date } : {}),
      },
      stdio: "pipe",
    });
  git(["init", "-q", "-b", "main"]);
  const commit = (subject, date) => {
    writeFileSync(join(repo, "f"), subject);
    git(["add", "f"]);
    git(["commit", "-q", "-m", subject], date);
  };
  commit("feat(a): first thing", "2026-08-23T10:00:00");   // in window
  commit("fix(b): not a feature", "2026-08-23T11:30:00");   // not feat(
  commit("feat(c): second thing", "2026-08-23T12:00:00");   // in window
  commit("feat(old): before the window", "2026-08-20T10:00:00"); // out of window
  return repo;
}

// --- run the scribe once with a given env overlay, return parsed activity.json ---
function runScribe(stateDir, projectsDir, overlay = {}) {
  execFileSync(SCRIBE, [], {
    env: {
      ...process.env,
      ACTIVITY_PROJECTS_DIR: projectsDir,
      ACTIVITY_STATE_DIR: stateDir,
      ACTIVITY_NOW: NOW,
      ACTIVITY_WINDOW_HOURS: "24",
      ...overlay,
    },
    stdio: "pipe",
  });
  return JSON.parse(readFileSync(join(stateDir, "activity.json"), "utf8"));
}

const repo = makeFeatRepo();
const stateDir = mkdtempSync(join(tmpdir(), "scribe-state-"));
let failures = 0;
const ok = (label, cond, detail = "") => {
  if (cond) { console.log(`  ✓ ${label}`); }
  else { failures++; console.log(`  ✗ ${label}${detail ? ` -- ${detail}` : ""}`); }
};

try {
  // ---- Run 1: no swamp.jsonl present (aligator absent -> 0), whitelist = our repo.
  const a = runScribe(stateDir, FIXTURES, { ACTIVITY_ORCH_REPOS: repo });
  const t = a.totals || {};
  const tk = t.tokens_total || {};

  // Schema: the six documented totals + generated_at/window + per_day + summary_lines.
  ok("S3: generated_at present", typeof a.generated_at === "string");
  ok("S3: window_hours = 24", a.window_hours === 24, `${a.window_hours}`);
  ok("S3: summary_lines is a non-empty array", Array.isArray(a.summary_lines) && a.summary_lines.length >= 1);

  // Tokens summed across all assistant records (input/output/cache_*).
  ok("S3: tokens input summed", tk.input === 3500, `${tk.input}`);
  ok("S3: tokens output summed", tk.output === 700, `${tk.output}`);
  ok("S3: tokens cache_creation summed", tk.cache_creation === 150, `${tk.cache_creation}`);
  ok("S3: tokens cache_read summed", tk.cache_read === 35, `${tk.cache_read}`);

  // Human prompts = user records that are BOTH promptSource=="typed" AND a string
  // content (the tool_result list + the non-typed hook injection are excluded).
  ok("S3: human_prompts = 3 (typed strings only)", t.human_prompts === 3, `${t.human_prompts}`);

  // Duration since the last human prompt = NOW(15:00) - last typed(14:00) = 60 min.
  ok("S3: minutes_since_last_human_prompt = 60", t.minutes_since_last_human_prompt === 60, `${t.minutes_since_last_human_prompt}`);

  // Features = `feat(` commits from the whitelist WITHIN the window (out-of-window
  // feat excluded, fix excluded) -> 2.
  ok("S3: features_implemented = 2 (in-window feat commits)", t.features_implemented === 2, `${t.features_implemented}`);

  // tokens_per_feature = round((input+output) / features) — window ratio (ponytail).
  ok("S3: tokens_per_feature = round((in+out)/features)",
     t.tokens_per_feature === Math.round((tk.input + tk.output) / t.features_implemented),
     `${t.tokens_per_feature}`);

  // Aligator: swamp.jsonl absent -> 0 (graceful).
  ok("S3: aligator_calls = 0 when swamp.jsonl absent", t.aligator_calls === 0, `${t.aligator_calls}`);

  // per_day: one entry for 2026-08-23 with autonomy = longest gap between two
  // consecutive human prompts that day = max(90, 270) = 270 min, features = 2.
  const day = (a.per_day || []).find((d) => d.date === "2026-08-23");
  ok("S3: per_day has the 2026-08-23 entry", !!day, JSON.stringify(a.per_day));
  ok("S3: autonomy_minutes_max = 270 (longest inter-prompt gap)", day && day.autonomy_minutes_max === 270, day && `${day.autonomy_minutes_max}`);
  ok("S3: per_day features = 2", day && day.features === 2, day && `${day.features}`);

  // The PO benchmark to display alongside the panel: 'go-build ~6h'.
  ok("S3: a `record` benchmark is emitted (go-build ~6h)",
     a.record !== undefined && /go-build/i.test(JSON.stringify(a.record)),
     JSON.stringify(a.record));

  // Cron/watcher/round ticks dispatched AS typed (sessionC, incl. one at 14:59)
  // are NOT human intervention — explicit guard on top of the ===3/===60 asserts.
  ok("S3: cron/watcher/round ticks excluded from human prompts (typed but automated)",
     t.human_prompts === 3 && t.minutes_since_last_human_prompt === 60,
     `${t.human_prompts}/${t.minutes_since_last_human_prompt}`);

  // ---- Fix 1: whitelist derived from live tabs (ACTIVITY_TABS_JSON) must resolve
  //      each orchestrator to its PROJECT (assignment `<project>:` override >
  //      basename cwd) and scan `<DEV_ROOT>/<project>` — NOT the raw cwd. An
  //      orchestrator sitting in a work-root cwd but overriding to a project must
  //      credit THAT project's feat( commits; a non-orchestrator tab is ignored.
  {
    const devRoot = mkdtempSync(join(tmpdir(), "scribe-devroot-"));
    const proj = "overridden-proj";
    const projRepo = join(devRoot, proj);
    mkdirSync(projRepo);
    const g = (a2, date) => execFileSync("git", ["-C", projRepo, ...a2], {
      env: { ...process.env,
        GIT_AUTHOR_NAME: "t", GIT_AUTHOR_EMAIL: "t@t",
        GIT_COMMITTER_NAME: "t", GIT_COMMITTER_EMAIL: "t@t",
        ...(date ? { GIT_AUTHOR_DATE: date, GIT_COMMITTER_DATE: date } : {}) },
      stdio: "pipe",
    });
    g(["init", "-q", "-b", "main"]);
    const c = (subj, date) => { writeFileSync(join(projRepo, "f"), subj); g(["add", "f"]); g(["commit", "-q", "-m", subj], date); };
    c("feat(x): one", "2026-08-23T09:00:00");
    c("feat(y): two", "2026-08-23T10:00:00");
    c("feat(z): three", "2026-08-23T11:00:00");
    c("feat(old): out of window", "2026-08-20T09:00:00");
    const tabsFile = join(devRoot, "tabs.json");
    writeFileSync(tabsFile, JSON.stringify([
      { assignment: `${proj}:build/orchestrator`, cwd: "/home/mox2/Dev" }, // project via OVERRIDE, cwd is a work root
      { assignment: "build/worker", cwd: projRepo },                        // in the repo but NOT an orchestrator -> ignored
    ]));
    const e = runScribe(stateDir, FIXTURES, { ACTIVITY_ORCH_REPOS: "", ACTIVITY_TABS_JSON: tabsFile, ACTIVITY_DEV_ROOT: devRoot });
    ok("S3: whitelist resolves orchestrator PROJECT via override, not raw cwd -> features = 3",
       (e.totals || {}).features_implemented === 3, `${(e.totals || {}).features_implemented}`);
    rmSync(devRoot, { recursive: true, force: true });
  }

  // ---- No hardcoded whitelist: empty ACTIVITY_ORCH_REPOS -> 0 features.
  const b = runScribe(stateDir, FIXTURES, { ACTIVITY_ORCH_REPOS: "" });
  ok("S3: features_implemented = 0 when whitelist empty (no hardcoded repos)",
     (b.totals || {}).features_implemented === 0, `${(b.totals || {}).features_implemented}`);

  // ---- Aligator present: 3 swamp lines -> aligator_calls = 3.
  writeFileSync(join(stateDir, "swamp.jsonl"),
    [
      '{"ts":1755950400,"tab":"tab-a","input":"go","submit":true}',
      '{"ts":1755950460,"tab":"tab-a","input":"more","submit":true,"from":"aligator"}',
      '{"ts":1755950520,"tab":"tab-b","input":"yet more","submit":true}',
    ].join("\n") + "\n");
  const c = runScribe(stateDir, FIXTURES, { ACTIVITY_ORCH_REPOS: repo });
  ok("S3: aligator_calls = 3 (swamp lines counted)", (c.totals || {}).aligator_calls === 3, `${(c.totals || {}).aligator_calls}`);

  // ---- Idempotent: same window -> identical output modulo generated_at.
  const d1 = runScribe(stateDir, FIXTURES, { ACTIVITY_ORCH_REPOS: repo });
  const d2 = runScribe(stateDir, FIXTURES, { ACTIVITY_ORCH_REPOS: repo });
  delete d1.generated_at; delete d2.generated_at;
  ok("S3: idempotent (re-run same window -> same output, generated_at aside)",
     JSON.stringify(d1) === JSON.stringify(d2));

  // =====================================================================
  // Increment 6 — REFINER red tests. Builder: scribe (S5, S7).
  // =====================================================================

  // ---- S7 (FIX): full 3-layer human-vs-injected classification + autonomy_record.
  //      A `queued` prompt is a bufferized HUMAN prompt (must count); a `system`
  //      source, a `tool_result` array, a nudge ("continue") and a cron tick are
  //      ALL injected (must NOT count nor stretch autonomy). This is THE slice
  //      where the red protects the key figure. RED until the scribe promotes
  //      `queued` to human and demotes nudges.
  {
    const pj = mkdtempSync(join(tmpdir(), "scribe-s7-proj-"));
    const sd = mkdtempSync(join(tmpdir(), "scribe-s7-state-"));
    const rec = (o) => JSON.stringify(o);
    const day = "2026-08-23";
    // humans (typed + queued) at 10:00, 11:00, 12:40, 13:00 -> gaps 60/100/20 max 100.
    writeFileSync(join(pj, "s7.jsonl"), [
      rec({ type: "user", promptSource: "typed", sessionId: "s7", timestamp: `${day}T10:00:00.000Z`, message: { role: "user", content: "real work A" } }),
      rec({ type: "user", promptSource: "queued", sessionId: "s7", timestamp: `${day}T11:00:00.000Z`, message: { role: "user", content: "buffered but human-typed prompt" } }),
      rec({ type: "user", promptSource: "typed", sessionId: "s7", timestamp: `${day}T11:30:00.000Z`, message: { role: "user", content: "continue" } }),          // nudge -> injected
      rec({ type: "user", promptSource: "system", sessionId: "s7", timestamp: `${day}T12:00:00.000Z`, message: { role: "user", content: "<task-notification>tick</task-notification>" } }), // injected
      rec({ type: "user", sessionId: "s7", timestamp: `${day}T12:10:00.000Z`, message: { role: "user", content: [{ type: "tool_result", content: "ok" }] } }),   // array -> injected
      rec({ type: "user", promptSource: "typed", sessionId: "s7", timestamp: `${day}T12:20:00.000Z`, message: { role: "user", content: "Ronde du soir : refresh roster" } }), // cron -> injected
      rec({ type: "user", promptSource: "queued", sessionId: "s7", timestamp: `${day}T12:40:00.000Z`, message: { role: "user", content: "another human buffered prompt" } }),
      rec({ type: "user", promptSource: "typed", sessionId: "s7", timestamp: `${day}T13:00:00.000Z`, message: { role: "user", content: "real work B" } }),
    ].join("\n") + "\n");
    const s7 = runScribe(sd, pj, { ACTIVITY_ORCH_REPOS: "" });
    const t7 = s7.totals || {};
    ok("S7: queued counts as human, nudge/cron/system/array excluded -> human_prompts = 4",
       t7.human_prompts === 4, `${t7.human_prompts}`);
    const d7 = (s7.per_day || []).find((d) => d.date === day) || {};
    ok("S7: autonomy computed on HUMAN prompts only -> max gap = 100",
       d7.autonomy_minutes_max === 100, `${d7.autonomy_minutes_max}`);
    ok("S7: an autonomy_record (all-time high-water) is tracked, >= current max",
       typeof t7.autonomy_record === "number" && t7.autonomy_record >= 100, `${t7.autonomy_record}`);
    rmSync(pj, { recursive: true, force: true });
    rmSync(sd, { recursive: true, force: true });
  }

  // ---- S5: SEPARATE deliverable counters (features NOT divided) + verdict.
  //      A toy repo with 2 feat(, 3 fix(, 1 tooling-add (scripts/) -> features=2,
  //      fixes=3, self_tooling>=1, all DISTINCT fields; issues_* numeric (0 offline);
  //      self_improvement_verdict is a shaped classifier. RED until the counters exist.
  {
    const r = mkdtempSync(join(tmpdir(), "scribe-s5-repo-"));
    const sd = mkdtempSync(join(tmpdir(), "scribe-s5-state-"));
    const g = (args, date) => execFileSync("git", ["-C", r, ...args], {
      env: { ...process.env, GIT_AUTHOR_NAME: "t", GIT_AUTHOR_EMAIL: "t@t", GIT_COMMITTER_NAME: "t", GIT_COMMITTER_EMAIL: "t@t",
             ...(date ? { GIT_AUTHOR_DATE: date, GIT_COMMITTER_DATE: date } : {}) },
      stdio: "pipe",
    });
    g(["init", "-q", "-b", "main"]);
    const commit = (subject, date, file = "f") => { mkdirSync(dirname(join(r, file)), { recursive: true }); writeFileSync(join(r, file), subject); g(["add", "-A"]); g(["commit", "-q", "-m", subject], date); };
    commit("feat(a): feature one", "2026-08-23T09:00:00");
    commit("feat(b): feature two", "2026-08-23T09:30:00");
    commit("fix(x): bug one", "2026-08-23T10:00:00");
    commit("fix(y): bug two", "2026-08-23T10:30:00");
    commit("fix(z): bug three", "2026-08-23T11:00:00");
    commit("chore(tooling): add helper script", "2026-08-23T11:30:00", "scripts/newtool.sh"); // ADDS a tooling file
    // All commit-derived counters (features/fixes/self_tooling) scan the SAME repo
    // whitelist as features — no separate env, just more counters over the same set.
    const s5 = runScribe(sd, FIXTURES, { ACTIVITY_ORCH_REPOS: r });
    const t5 = s5.totals || {};
    ok("S5: features NOT divided -> features = 2 (feat commits only)", t5.features_implemented === 2, `${t5.features_implemented}`);
    ok("S5: fixes counted SEPARATELY -> fixes = 3", t5.fixes === 3, `${t5.fixes}`);
    ok("S5: self_tooling counted SEPARATELY -> >= 1 (scripts/ add)", t5.self_tooling >= 1, `${t5.self_tooling}`);
    ok("S5: the three counters are distinct fields", t5.features_implemented !== undefined && t5.fixes !== undefined && t5.self_tooling !== undefined);
    ok("S5: issues_opened / issues_closed are numeric (0 when offline)",
       typeof t5.issues_opened === "number" && typeof t5.issues_closed === "number", `${t5.issues_opened}/${t5.issues_closed}`);
    const v = s5.self_improvement_verdict || {};
    ok("S5: self_improvement_verdict is a shaped classifier (verdict + trend + tooling_rate + evidence)",
       ["maturité", "croissance", "indéterminé"].includes(v.verdict) && "autonomy_trend" in v && "tooling_rate" in v && Array.isArray(v.evidence),
       JSON.stringify(v));
    rmSync(r, { recursive: true, force: true });
    rmSync(sd, { recursive: true, force: true });
  }
} catch (e) {
  failures++;
  console.log(`  ✗ scribe run crashed (RED until scripts/activity-scribe exists): ${e.message}`);
} finally {
  rmSync(repo, { recursive: true, force: true });
  rmSync(stateDir, { recursive: true, force: true });
}

console.log(`\n${failures ? `FAIL: ${failures} check(s) failed` : "OK: activity-scribe self-check passed"}`);
process.exit(failures ? 1 : 0);
