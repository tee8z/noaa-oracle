const fs = require("node:fs");
const path = require("node:path");
const esbuild = require("esbuild");

const licenses = fs.readdirSync(path.join(__dirname, "licenses")).sort()
  .map(name => `${name}\n${fs.readFileSync(path.join(__dirname, "licenses", name), "utf8")}`)
  .join("\n\n");

esbuild.buildSync({
  absWorkingDir: __dirname,
  entryPoints: ["node_modules/@duckdb/duckdb-wasm/dist/duckdb-browser.mjs"],
  outfile: "duckdb.js",
  bundle: true,
  format: "esm",
  platform: "browser",
  target: "es2020",
  minify: true,
  legalComments: "inline",
  banner: { js: licenses.split("\n").map(line => `// ${line}`).join("\n") },
});
