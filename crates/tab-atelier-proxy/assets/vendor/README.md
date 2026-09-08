# Vendored web dependencies

## `bootstrap.min.css` — NOT here, and nothing to do

Debian packages it (`libjs-bootstrap5`), so the `.deb` depends on that and the
server serves `/vendor/bootstrap.min.css` straight from
`/usr/share/javascript/bootstrap5/` when it is not in this directory. Security
updates reach the UI through `apt upgrade`, with nothing re-vendored and no
symlink to create — running from a source checkout works as-is.

Drop a file here with that name and it wins, if you ever need to pin a
different build.

## `vue.global.prod.js` — here, because Debian has no Vue package

Fetch it once, pinned, and commit it:

```sh
scripts/fetch-vue.sh
```

Served locally rather than from a CDN on purpose. A credential proxy is exactly
the sort of thing that runs on a network with no outbound access, where a CDN
script tag means the admin UI simply does not load — and it would hand a third
party a script tag on the page where the admin token is typed.
