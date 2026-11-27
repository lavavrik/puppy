# Browser downloads

An extension for Chrome, Chromium, Brave, Edge and Vivaldi hands the browser's
downloads to puppy. It talks to puppy through the browser's native messaging,
so no network port is opened. Firefox is not supported yet.

1. **Install the native host.** `puppy browser install` registers it for every
   browser found; `--browser chrome|chromium|brave|edge|vivaldi` picks one, and
   `--force` with it registers that browser even if it is not found.
   `puppy browser status` shows what is registered, and `puppy browser
   uninstall` removes it. Re-run `install` after moving the puppy binary
   (`status` then says "stale").
2. **Load the extension.** Open `chrome://extensions`, turn on Developer mode,
   choose Load unpacked and pick the `extension/` folder from the release
   archive (or from this repo). Keep the folder where it is: the browser loads
   it from there every time. The card must
   show the ID `cbfmabianfipnmdcdipmcpaggpejlaeg`.
3. **What happens.** Every download goes to puppy's download folder, sent with
   the browser's cookies, referrer and user agent. The browser's own copy is
   cancelled only after puppy accepted the download, and the browser keeps it
   if puppy does not answer within 10 s. If puppy accepts late, or the browser
   finishes first, puppy removes its own copy and keeps the browser's.
4. **Bypass and settings.** The toolbar popup has a "Let the browser take the
   next download" button; Alt+Shift+B does the same. It covers one download
   within 60 s. The options page sets a minimum size and the file types and
   sites to leave to the browser, and shows the connection status.
5. **Privacy.** A browser download's cookies are kept in puppy's private state
   file (mode 0600) and cleared when the download completes or is removed. They
   never reach logs, `puppy info`, `--json` output or the resume sidecar. An
   interrupted logged-in download resumes after a restart.

## Known limits

- `blob:`, `data:` and `file:` downloads stay in the browser.
- Incognito downloads and downloads started by other extensions stay in the
  browser.
- POST-backed (form) downloads can't be detected, and may fail in puppy. Use
  the bypass (toolbar button or Alt+Shift+B) for them.
- Cookies are read for the original URL only. If that URL redirects to another
  origin (a different scheme, host or port), the target gets what a browser
  would send it: no cookies, credentials or page headers other than `Accept`,
  `Accept-Language` and `User-Agent`, and the page's referer cut down to its
  origin (none from HTTPS to plain HTTP). See
  [Command line](cli.md) for the whole rule. A download whose redirect
  target needs a login or a header of its own fails in puppy. The extension
  never adds mirrors; a mirror added otherwise gets none of a download's
  cookies unless it is on the same origin.
- A metalink the browser hands over is downloaded as a file, not read: add
  it with `puppy add` (or the dashboard) to get its files.
- CHIPS partitioned cookies are not read below Chrome 119.
- "Ask where to save each file" is not mirrored: files go to puppy's folder.
- Re-downloading a completed download after a post-completion recheck failure
  goes without cookies.
- Flatpak and Snap browsers are not supported.
- On Windows, Vivaldi shares Chrome's native-messaging registration, and Brave
  is registered both under its own key and under Chrome's. Brave's location
  is unverified.
- The Windows registry code has been compile-checked only.

## Manual check (Chrome)

1. `cargo build --release -p puppy && ./target/release/puppy browser install && ./target/release/puppy browser status`: chrome shows `ok`.
2. Load the extension unpacked. The ID matches, and the options page status reads "Ready: ...".
3. Download a large public file. It leaves the browser's list, and `puppy ls` shows it under the same name.
4. Download something behind a login (for example a private GitHub release asset). It completes in puppy.
5. Press Alt+Shift+B, then download. It stays in the browser.
6. Run `puppy browser uninstall`, then download. It stays in the browser, and the popup lists it as failed with "not reachable".
7. Start a `blob:` download (for example "export" in a web app). It stays in the browser.
8. Add `pdf` to the file types. A PDF stays in the browser.
9. Midway through a logged-in download, run `puppy stop` then `puppy resume <id>`. It resumes.
10. Pausing during filename determination: start a download from a link that sets its name in a header (`Content-Disposition`). The browser shows no "Save as" and no partial file, and puppy gets the final name. With puppy unreachable the same download still completes in the browser.
11. Erase removes the entry: after puppy took a download, it is gone from `chrome://downloads` and the toolbar download bubble, not left as a cancelled entry.
12. Turn on "Ask where to save each file" in the browser settings and download. No save dialog flashes, and puppy gets the file.
13. Late accept: suspend the daemon (`kill -STOP <pid>`) so the browser's 10 s wait runs out, and download. The browser finishes the file itself. Resume the daemon (`kill -CONT <pid>`): puppy removes its own copy, keeping the browser's file.
14. Browser finished first: download a tiny file. The browser's copy completes and puppy ends up with no second copy of it (if puppy took one, it removes it).
