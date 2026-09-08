# Vendored web dependencies

## `bootstrap.min.css` — NOT here

Debian packages it (`libjs-bootstrap5`), so the `.deb` depends on that and
`postinst` symlinks `/usr/share/javascript/bootstrap5/css/bootstrap.min.css`
into this directory. Security updates then reach the UI without anyone
re-vendoring anything.

Running from a source checkout on a Debian box, make the same link yourself:

```sh
ln -sf /usr/share/javascript/bootstrap5/css/bootstrap.min.css \
       crates/tab-atelier-proxy/assets/vendor/bootstrap.min.css
```

## `vue.global.prod.js` — here, because Debian has no Vue package

Fetch it once, pinned, and commit it:

```sh
scripts/fetch-vue.sh
```

Served locally rather than from a CDN on purpose. A credential proxy is exactly
the sort of thing that runs on a network with no outbound access, where a CDN
script tag means the admin UI simply does not load — and it would hand a third
party a script tag on the page where the admin token is typed.
