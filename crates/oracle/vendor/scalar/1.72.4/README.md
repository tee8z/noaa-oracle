# Scalar API reference 1.72.4

`standalone.js` is `dist/browser/standalone.js` from the npm package
`@scalar/api-reference@1.72.4` (MIT), unmodified. It is the bundle jsDelivr
served for the unpinned `@scalar/api-reference` URL that `/docs` used
before. `build.rs` embeds it and serves it at a content-hashed URL; `/docs`
turns off its web fonts and telemetry, so the page loads nothing from other
sites.

- Tarball: https://registry.npmjs.org/@scalar/api-reference/-/api-reference-1.72.4.tgz
  (`sha512-HKsUqCJbXhmz/5j6ETceXVgnlZKOX1uidF7jz8elSuzKE6FJjJYhK1G7WqxHk4zLabbSb13QHQ9yKUikf8XLBQ==`)
- `standalone.js`: `sha384-omTRdD9MbjA1vm12DqRUVvqJlr3VzSixvAdF1Jruu9AJOiJKyTKraIB6DyX+m10M`

To check or replace it:

```sh
curl -sL "$(curl -s https://registry.npmjs.org/@scalar/api-reference/1.72.4 | jq -r .dist.tarball)" | tar xz
openssl dgst -sha384 -binary package/dist/browser/standalone.js | openssl base64 -A
```
