<!-- SPDX-License-Identifier: MPL-2.0 -->

# Admin UI sources

TypeScript, compiled to the committed `../assets/*.js` that the proxy serves.

## The one rule

**Edit `src/*.ts`, never `assets/*.js`.** The output is generated, and
`ui_build_is_current` in `tests/web_assets.rs` fails if it is stale.

```sh
cd crates/tab-atelier-proxy/web
bun install          # once
bun run build        # emit ../assets/{app,charts}.js
bun run check        # type-check without emitting
```

## Why the output is committed

Because `cargo deb`, CI and anyone doing `cargo build` must not need Node. The
package is built from a checkout, and a checkout that required a JavaScript
toolchain before it could produce a `.deb` would be a new dependency for every
consumer of this crate — to compile 800 lines that change a few times a year.
So the compiler runs on a developer's machine and the result is reviewed like
any other source.

That trade has one failure mode — output drifting from source — and the test
above is what closes it.

## No bundler, and no modules in the output

The two files are plain `<script>` tags sharing one global scope, exactly as
before: `charts.js` assigns `window.TaCharts`, `app.js` reads it. Neither `.ts`
file has a top-level `import` or `export`, so `tsc` emits them unwrapped.

Vue's types arrive through a type-level `import("vue")` inside an ambient
`.d.ts`, which erases completely. The npm `vue` package is a devDependency for
typing only — the runtime is still the vendored `assets/vendor/vue.global.prod.js`.

## Two things that bite

**Explicit return types on computed properties, and on `data()`.** Vue's
Options API infers `this` from `data`, `computed` and `methods` together. With
an object this size it gives up *silently* and hands back `any` in methods and
`{}` in computed — the file type-checks while checking nothing. `data(): AppState`
and a return type on every computed is what keeps that honest.

**Do not name an interface after a DOM global.** An ambient `interface Scheduler`
here MERGED with `lib.dom`'s rather than shadowing it, so the type quietly
gained `postTask` and `yield` and matched nothing the API returns. Hence
`PlanScheduler`.
