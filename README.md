# Documents

A document viewer plugin for Ice Commander. PDF is the only format so far.

## What it opens

`.pdf`, by extension only: the application picks a viewer by the file's
extension and does not look inside the file. The plugin registers the viewer
`pdf-view` at priority 0, so opening a `.pdf` for viewing lands here.

The window is a declarative document every frontend draws: the page, and under
it back and forward buttons (also on `Left` and `Right`, greyed out on the
first and last page) with the file's name and *page / pages*. The page keeps its
proportions, fits the window and can be zoomed by the frontend.

## A page at a time

Only the page on screen is rendered. The window's document names it —
`part:page/3` — instead of carrying it, and the host asks the plugin for those
pixels when it draws. The plugin keeps only the last page it rendered.

Turning a page answers `{"redescribe":true}`; the new document names the new
page.

Pages are rendered 1600 pixels wide, on white, as PNG.

## Where the file comes from

The plugin first asks the host for a local path (`fs_local_path`); with one,
pdfium reads the file itself. Without one, the whole file is read through
`fs_open` and kept in memory while the window is open.

The document is opened for every render and closed again rather than held:
pdfium keeps the file open, and an open window must not keep a lock on it.

## libpdfium

pdfium is bound at run time, from the first of:

1. beside the running executable (the application's, not the plugin's);
2. `../Libs` from there, the macOS bundle layout;
3. on Linux, `/usr/lib/ice-commander/libpdfium.so`, then `/usr/lib/libpdfium.so`;
4. the system's library search path.

Without it no page can be counted, so the plugin refuses the file at open and
the application reports that the viewer could not open it.

## Not in a terminal

A console host has nothing to draw a page onto, so the plugin refuses to load
there rather than claiming `.pdf` and showing nothing.

## Known limitations

- Neither this repository nor the application's packages ship libpdfium; it has
  to be put in one of the places above by hand.
- One page at a time: no continuous scrolling, no jump to a page, no text
  search or selection, no printing.
- Pages are always rendered 1600 pixels wide, whatever the window size or zoom.
- Documents that need a password to open are refused; there is no prompt for
  one.
- The page count is held as a 16-bit number, so a document of more than 65 535
  pages is miscounted.
- The tests cover page turning and part lookup without pdfium; rendering itself
  is not tested.

## Building

```sh
./build.sh          # release build; the library is copied into bin/
./test.sh           # cargo test --workspace
./deploy-local.sh   # copies bin/ into the plugin folder
```

`build.sh` produces `libic_pdf_view.dylib`, `libic_pdf_view.so` or
`ic_pdf_view.dll`. `ic-plugin-api` is fetched from
`github.com/ice-commander/plugin-api` (branch `main`).

`deploy-local.sh` copies into `~/Library/Application Support/ice-commander/plugins`
on macOS, `%APPDATA%\ice-commander\plugins` on Windows and
`${XDG_DATA_HOME:-~/.local/share}/ice-commander/plugins` elsewhere;
`IC_PLUGIN_DIR` overrides it. Then switch the plugin on in
**Settings → Plugins** and restart.

`version.rs` is generated from `package.json` by `node builder/gen-version.js`
(`npm run gen-version`) and is the version the plugin reports. `build.sh` does
not regenerate it.

## Licence

MIT or Apache-2.0, at your option. Contributions are taken under the
[DCO](DCO); sign off with `git commit -s`.
