# 04 — Branding, removed screens and strings

**Upstreamable: no.** These are the changes that make this a different app.

## The changes

- **Name:** `res/values/notrans.xml` sets `app_name` to `Tab Atelier Remote`.
- **Removed screen:** `ui/screens/contact/` (the upstream Contact screen) and its test
  `ContactScreenTest.kt`. The route and the navigation to it are removed in `NavGraph.kt`.
- **Help screen:** `ui/screens/help/HelpScreen.kt` carries `Changed for Tab Atelier Remote`
  markers at two places. The change is to upstream's text and links there.
- **Deep links:** `AndroidManifest.xml` adds the `tabatelier` scheme to two intent filters.
- **Strings:** upstream strings that name ConnectBot or link to its community were rewritten in
  `res/values/strings.xml`. Strings the fork adds live in `strings_tabatelier.xml`.

## Why none of it goes upstream

Upstream's identity, its community links and its contact route are upstream's decisions. The
licence (Apache-2.0, section 4(b)) requires these changes to be marked, which is why each has a
`Changed for Tab Atelier Remote` comment. Keep the markers, so a sync can find them.

## What a sync must watch

`res/values/strings.xml` changed 26 times upstream since the base, and every localised
`values-*/strings.xml` changed too. A rewritten string is an edit in the middle of upstream's
file, so each one is a likely conflict. The fork's rule is to keep rewritten strings few, and
to prefer adding a new string in `strings_tabatelier.xml` over editing an upstream one.
