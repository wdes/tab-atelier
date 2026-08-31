# #39 MERGE handoff — origin/main → feat/harness-dashboard

Repo: /home/mox2/Dev/tab-atelier. Base HEAD 0c02f74. origin/main=280cbdd.
Safety: pre-rebase-40 + backup/pre-merge-40 @0c02f74; backup/harness-dashboard-preauthor @0de7326.
NO PUSH. Merge commit only. Verify a-e before ping MAS.

## Plan / progress (update as you go)
- [ ] `git merge --no-ff --no-commit origin/main` started
- [ ] brain.rs → `git checkout --theirs` + re-add bump_usage (4 lines in tick() after
      `.send(pick.trigger.action()...)?;` before `if deferred > 0`):
      `crate::cli::share_link::bump_usage(&ep, &pick.tab_id);`
- [ ] headless.rs → 1 conflict (SnapshotTab build): keep BOTH (card fields + tab_env).
      Also dedup last_used_at (auto-merge kept it twice at ~1741 in a TabState build).
- [ ] api/mod.rs → `git checkout --ours` (keep split), then PORT env feature:
      - types.rs SnapshotTab: + `pub tab_env: BTreeMap<String,String>` (after tokens)
      - mod.rs: + `mask_env_value` + `mask_env_map` fns (pub(in crate::api))
      - mod.rs: + `const VENDOR_XTERM_UNICODE11_JS = include_str!(".../addon-unicode11.js")`
      - asset route arm (server.rs handle_connection): + `/assets/xterm-unicode11-6.0.0.js` case
      - handlers/admin.rs env_get: mask via mask_env_map(&tab_env_global())
      - NEW route GET /tabs/{id}/env (dispatch + handler): mask_env_map(&snap.tabs[idx].tab_env)
      - 5 tests: relay_passes_upstream_error_status_through, env_list_per_tab_masks*,
        env_list_global_masks*, mask_env_value_covers*, env_list_per_tab_unknown_tab_404
      - test_snapshot_tab + all SnapshotTab literals: + tab_env (Default::default())
- [ ] lib.rs → dedup last_used_at (TabState field decl ~1315 + Default ~1587 — both dup of the
      upstream mru one at ~1173). REMOVE the duplicates (keep upstream's).
- [ ] app.rs (modify/delete DU) → PORT 148-line upstream delta hunk-by-hunk into app/*:
      upstream hunks (git diff 28bb05e origin/main -- src/app.rs):
      * Tab::new boot-active last_used_at stamp (~266)
      * wipe render cache on resume / alt-screen (~356)
      * TabSwitcher.filter field (~456)
      * release_render_caches() on window focus-gain (~1450)
      * mru: fold viewer_attached_at_millis into tab.last_used_at (~2059)
      * SnapshotTab build: + tab_env: tab.tab_env.clone() (~2142) → app/persist.rs
      * switcher_filtered() fn (~4560) + Ctrl+P switcher render filter (~4604)
      * Render alt-screen scroll/resume (~6160, 6188) → app/render/* or app/mod.rs
      Each hunk MUST land in app/* (zero loss — check a). Then `git rm src/app.rs`.
- [ ] runtime Tab.tab_env (app/mod.rs Tab struct + restore + default) for GUI.
- [ ] cargo test --lib headless + gui green.
- [ ] a-e: no upstream loss / split intact (no monolith) / brain=upstream+bump_usage /
      gui+headless green / williamdes authorship intact.
- [ ] commit merge (NO push), ping MAS.

## Notes / gotchas found
- Auto-merge kept last_used_at TWICE (both sides added it) → E0124/E0062. Dedup.
- git rename-detected api.rs→api/mod.rs (5 huge conflicts, up to 825 lines) but NOT
  app.rs→app/mod.rs (DU). So app.rs port is fully manual.
- headless lib compiled after tab_env + last_used_at dedup last time.
