# Templates

Server-rendered HTML using [Maud](https://maud.lambda.xyz/) and
[htmx](https://htmx.org/). A component's CSS, and JavaScript if it needs
any, sits beside the Rust that renders it.

## Structure

```
templates/
├── assets.rs           # Serves the embedded CSS, JS and images
├── styles.css          # Base styles, bundled first
├── static/             # Images, served as they are
├── layouts/            # Page shell
├── components/         # Pieces shared by pages
├── fragments/          # Parts htmx fetches on their own
└── pages/              # One module (or folder) per page
```

A template with styles or a script is a folder: `mod.rs` plus `name.css`
and/or `name.js`.

## Build

`build.rs` runs at compile time and writes only to Cargo's `OUT_DIR`:

1. Minifies every `.css` file below `templates/` (`styles.css` first) with
   lightningcss into one stylesheet, `site.css`.
2. Minifies each `.js` file with oxc as a classic script, so top-level names
   stay global, and joins them into bundles: `layouts/head.js` alone runs in
   `<head>` before the page paints; `pages/raw_data/` scripts load only on
   the raw data page; everything else is `site.js`. A script that does not
   parse fails the build.
3. Takes `static/` files as they are: htmx 1.9.10 as published
   (`dist/htmx.min.js`) and the map.
4. Generates `assets.rs` with a content-hashed URL, the bytes and a gzipped
   copy of each file.

The binary embeds the files (`include_bytes!`) and serves them at
`/assets/<name>.<hash>.<ext>` with a one-year immutable cache header, gzipped
when the browser accepts it. A changed file gets a new URL, so nothing needs
to be installed next to the binary. Templates link to them through the
constants:

```rust
link rel="stylesheet" href=(assets::SITE_CSS.url);
script defer src=(assets::SITE_JS.url) {}
```

## Scripts and the Content-Security-Policy

Pages send `script-src 'self'`, so every script is a file served from
`/assets/`: no inline `<script>`, no `onclick=` attributes and no `hx-on`.
htmx is configured (`HTMX_CONFIG` in the layout) not to evaluate code, which
also rules out trigger filters such as `every 30s [cond]`; put that logic in
a script listening for `htmx:confirm` instead. The raw data page's policy
also allows DuckDB-WASM from jsdelivr.
