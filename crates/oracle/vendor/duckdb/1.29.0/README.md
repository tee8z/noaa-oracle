# DuckDB browser module

This directory contains the pinned JavaScript module for the Oracle Raw data page.
Cargo embeds `duckdb.js` under a content-hashed URL. Other pages do not import it.

The bundle replaces jsDelivr's generated `+esm` modules.
WebKit resolves their relative `Link: ...; rel="modulepreload"` headers against the document origin, causing three unnecessary 404 requests.
The worker and WebAssembly still load from the existing DuckDB 1.29.0 jsDelivr paths.

| Package | Version | Source |
| --- | --- | --- |
| DuckDB-WASM | 1.29.0 | [npm package](https://registry.npmjs.org/@duckdb/duckdb-wasm/1.29.0) |
| Apache Arrow | 17.0.0 | [npm package](https://registry.npmjs.org/apache-arrow/17.0.0) |
| FlatBuffers | 24.3.25 | [npm package](https://registry.npmjs.org/flatbuffers/24.3.25) |
| tslib | 2.6.3 | [npm package](https://registry.npmjs.org/tslib/2.6.3) |
| esbuild (build only) | 0.25.10 | [npm package](https://registry.npmjs.org/esbuild/0.25.10) |

`package-lock.json` pins npm tarballs and integrity hashes. `build.cjs` bundles the browser entry point without external JavaScript imports.
The bundle includes the license notices from `licenses/`.
Those files come from the npm packages, except DuckDB's license, which its npm tarball omits.
The [DuckDB license](https://github.com/duckdb/duckdb-wasm/blob/5cf0ddc70dbb4a4c7273d106b117974217c2aed0/LICENSE) comes from the package's npm `gitHead` commit.

Rebuild from this directory:

```sh
npm ci --ignore-scripts --no-audit --no-fund
npm run build
sha256sum duckdb.js
```

No npm tooling or network access is required during the Rust build.

Expected SHA-256:

```text
d8ddcfd340aaa84e6c77c6a4d06132c5b0ec842bdfa643d1fbe61ce556304ce9  duckdb.js
```
