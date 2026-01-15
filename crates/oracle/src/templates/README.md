# Templates

Server-rendered HTML using [Maud](https://maud.lambda.xyz/) with co-located JavaScript and CSS.

## Structure

```
templates/
├── layouts/
│   ├── mod.rs
│   └── base.rs         # Base page layout
├── pages/
│   ├── mod.rs
│   └── home/           # Folder because it has JS
│       ├── mod.rs      # Maud template
│       ├── home.js     # Client-side behavior
│       └── home.css    # Component styles (optional)
└── mod.rs
```

## Conventions

**Simple templates** (no JS/CSS): single `foo.rs` file

**Templates with JS/CSS**: folder with `mod.rs` + `foo.js` and/or `foo.css`

## Build Process

`build.rs` runs at compile time:

**JavaScript:**
1. Scans `templates/` for `.js` files
2. Concatenates and minifies into `static/app.min.js`
3. Generates content hash for cache busting

**CSS:**
1. Starts with `static/styles.css` (base/global styles)
2. Appends any `.css` files found in `templates/`
3. Minifies into `static/styles.min.css`

## Loader Pattern

External dependencies (WASM, large libraries) go in `static/loader.js`:

```javascript
// static/loader.js
import * as duckdb from 'https://cdn.jsdelivr.net/npm/@duckdb/duckdb-wasm@1.27.1/+esm';
window.duckdb = duckdb;

// Load app after dependencies ready
const script = document.createElement('script');
script.type = 'module';
script.src = '/static/app.min.js';
document.head.appendChild(script);
```

Then in your template JS, access via `window`:
```javascript
const duckdb = window.duckdb;
```

**Why?**
- Keeps external URLs out of bundled code
- Single place to manage versions
- App code stays clean and testable

## Adding JavaScript

1. Create folder: `pages/mypage/`
2. Add `mod.rs` (template) and `mypage.js`
3. Update `pages/mod.rs`
4. Rebuild - JS automatically bundled

## Adding CSS

**Global styles:** Edit `static/styles.css`

**Component styles:** Add `mycomponent.css` next to `mod.rs`:
```
components/
└── widget/
    ├── mod.rs
    ├── widget.js
    └── widget.css   # Styles specific to this component
```

Reference in layout:
```rust
link rel="stylesheet" href="/static/styles.min.css";
```

## Key Files

| File | Purpose |
|------|---------|
| `build.rs` | JS/CSS bundling at compile time |
| `static/loader.js` | WASM/external deps, loads app bundle |
| `static/styles.css` | Base/global styles (manual) |
| `static/app.min.js` | Generated JS bundle (gitignored) |
| `static/styles.min.css` | Generated CSS bundle (gitignored) |
| `layouts/base.rs` | Page wrapper, loads loader.js |
