# Bulma 1.0.4

`bulma.min.css` is `css/bulma.min.css` from the npm package `bulma@1.0.4`,
unmodified, with the package's `LICENSE` (MIT) beside it. `build.rs` embeds
it and serves it at a content-hashed URL, so pages no longer load it from a
CDN.

- Tarball: https://registry.npmjs.org/bulma/-/bulma-1.0.4.tgz
  (`sha512-Ffb6YGXDiZYX3cqvSbHWqQ8+LkX6tVoTcZuVB3lm93sbAVXlO0D6QlOTMnV6g18gILpAXqkG2z9hf9z4hCjz2g==`)
- `bulma.min.css`: `sha384-DCY3M8xLkMu6c9IKcKbe+jHKMjelnwC0p+SBaxfHxoBYZWdJF2X400UdBCgATtAB`

To check or replace it:

```sh
curl -sL "$(curl -s https://registry.npmjs.org/bulma/1.0.4 | jq -r .dist.tarball)" | tar xz
openssl dgst -sha384 -binary package/css/bulma.min.css | openssl base64 -A
```
