# Vendored JavaScript

| file         | origin                                            | licence   |
| ------------ | ------------------------------------------------- | --------- |
| `solver.js`  | [yt-dlp/ejs](https://github.com/yt-dlp/ejs)       | Unlicense |
| `meriyah.js` | [meriyah](https://github.com/meriyah/meriyah)     | ISC       |
| `astring.js` | [astring](https://github.com/davidbonnet/astring) | MIT       |
| `prelude.js` | this repo                                         | —         |

`solver.js` is `dist/yt.solver.core.js` built from yt-dlp/ejs at commit `c47fb1b`. It asks for
`meriyah` and `astring` through `require`, which `prelude.js` answers with the globals of that
name.
