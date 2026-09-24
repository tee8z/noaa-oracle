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

1. Collects every `.css` file below `templates/` (`styles.css` first) and
   minifies them with lightningcss into one stylesheet.
2. Collects every `.js` file and minifies them with better-minify-js into one
   classic script, so top-level names are shared across files.
3. Copies `static/` files as they are.
4. Generates `assets.rs` with a content-hashed URL and the bytes of each file.

The binary embeds the files (`include_bytes!`) and `assets.rs` serves them at
`/assets/<name>.<hash>.<ext>` with a one-year immutable cache header. A
changed file gets a new URL, so nothing needs to be installed next to the
binary. Templates link to them through the constants:

```rust
link rel="stylesheet" href=(assets::CSS_URL);
script defer src=(assets::JS_URL) {}
```

Large browser libraries (DuckDB-WASM on the raw data page) are imported
lazily by the script that needs them rather than on every page.
