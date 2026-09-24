# htmx 4.0.0

`htmx.min.js` is `dist/htmx.min.js` from the npm package `htmx.org@4.0.0`,
unmodified. `build.rs` embeds it and serves it at a content-hashed URL.

- Tarball: https://registry.npmjs.org/htmx.org/-/htmx.org-4.0.0.tgz
  (`sha512-T/171FUY93Kdfp8t+DnHdk45QvKRiBhVhhrwSzrXgUi4pHKvhp77dUA/qg8FAjsFWPIHNbmUuIdCrcVHuiZWng==`)
- `htmx.min.js`: `sha384-BvJpBiO8Kh31EqtJe5DRIeWrHWnCGkwytKs9NKFi86Hhw96dEqdEMzZDeK9iEGTc`

To check or replace it:

```sh
curl -sL "$(curl -s https://registry.npmjs.org/htmx.org/4.0.0 | jq -r .dist.tarball)" | tar xz
openssl dgst -sha384 -binary package/dist/htmx.min.js | openssl base64 -A
```
