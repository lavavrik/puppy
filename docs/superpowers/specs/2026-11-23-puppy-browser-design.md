# puppy — browser download pickup (design)

Status: approved in conversation 2026-11-23 ("yes, looking good").

## Intent

Every download started in the browser goes to puppy automatically instead of the
browser's own downloader, so puppy's segmented, resumable, rate-limited downloads and
its dashboard cover browser downloads too. This was item 5 of the follow-up list and is
named in the main spec (2026-11-19) as "browser download pickup (browser extension /
native messaging)", with `RequestContext` (headers, cookies, referer, user agent) already
in place for it.

## Goals

- **Chrome-family first** (Chrome, Chromium, Brave, Edge, Vivaldi), on Linux, macOS,
  Windows. **Firefox soon after, from the same extension**: one source tree written against
  the common WebExtensions API, Manifest V3, with a thin compatibility layer for the few
  differences, so Firefox later is mostly manifest keys plus a test pass.
- Every download is taken over automatically — no per-download prompt.
- Logged-in downloads work: the site's cookies, the referring page and the browser's user
  agent travel with the request.
- Nothing is ever lost: the browser download is cancelled only after puppy confirmed it
  accepted the download; if puppy is unreachable or refuses, the browser keeps it.
- Configurable extension.

## Non-goals

- Grabbing media streams from pages; right-click "download with puppy" (cheap later).
- Chrome Web Store / AMO publishing (later; this ships as an unpacked extension with a
  fixed ID).
- A localhost HTTP API on the daemon.

## Anti-goals

- No security hole: no network port any web page could hit; only the browser-launched
  native host talks to the daemon, over the existing local socket / named pipe.
- Cookies never written to disk, logs, `state.json`, `puppy info` or `--json` output
  (`RequestContext` is already excluded from persisted state; keep it so).
- Never break a download the browser could have done: `blob:`/`data:` URLs, POST-backed
  downloads, and anything puppy refuses stay with the browser.

## Approaches considered

1. **Native messaging (picked).** The browser launches a small puppy host process
   (`puppy native-host`, stdin/stdout length-prefixed JSON per the native messaging
   protocol); only the allowed extension ID can start it. The host forwards to the daemon
   over the existing client (autostarting it like the CLI does).
2. Localhost HTTP port on the daemon — rejected: any website could send it requests.
3. `puppy://` protocol handler — rejected: can't carry cookies; the browser confirms every
   time.

## Decided how

- **Extension takeover flow:** on a new download (`downloads.onCreated` and/or
  `onDeterminingFilename` — builder's call per browser), unless excluded by settings or
  unsupported: pause it in the browser, collect URL, final URL, suggested filename, referrer,
  cookies for that URL (`cookies` API), user agent; send to the native host; on puppy's
  "accepted" reply cancel and erase the browser download; on error or no reply in a short
  timeout, resume the browser download.
- **Unsupported, always left to the browser:** `blob:`, `data:`, `file:` URLs, POST-backed
  downloads where detectable, and anything else puppy refuses.
- **Where files go:** puppy's own download directory (same as every other puppy download);
  the browser's "ask where to save" choice is not mirrored.
- **Settings page (options):** enabled on/off; minimum file size (smaller stays in the
  browser; unknown size ⇒ take over); file types (extensions) to leave to the browser;
  sites (domains/patterns) to leave to the browser; a modifier key held while clicking
  (default Alt) lets the browser keep that one download; a status line showing whether
  puppy is reachable (native host installed + daemon answers). Settings stored in
  `storage.sync`.
- **Toolbar popup:** on/off toggle and the last few handed-over downloads (name, ok/error).
- **Native host:** a `puppy native-host` subcommand of the main binary (no separate
  binary to ship); hidden from normal help.
- **Install:** `puppy browser install` writes the native messaging host manifest for each
  Chrome-family browser found (Linux/macOS: per-user NativeMessagingHosts directories;
  Windows: per-user registry key under `HKCU\Software\<vendor>\...\NativeMessagingHosts`
  pointing to a manifest file), with `allowed_origins` set to the fixed extension ID;
  `puppy browser uninstall` removes them; `puppy browser status` reports what's installed.
  Firefox's manifest form (`allowed_extensions`) is designed in but not installed yet.
- **Fixed extension ID:** the manifest carries a `key` so the unpacked extension always
  gets the same ID; the host manifest allows only that ID.
- **Extension lives in the repo:** `extension/` (plain JS/TS, no heavy build chain; builder
  decides between plain JS and a tiny TypeScript build), with a README section on loading it
  unpacked (`chrome://extensions` → Developer mode → Load unpacked).

## Known limits (accepted)

- A download behind a login that is interrupted by a daemon restart can't resume once its
  cookies expire or are gone (they are never persisted); it shows the existing
  needs-attention / failed state.
- Downloads that require a form submit (POST) or page-generated data stay in the browser.

## Left to the builder

- Exact message schema between extension and native host (versioned, small).
- Which `downloads` events to hook per browser to avoid a visible flash of the browser's
  download bar; how to detect POST-backed downloads.
- Timeout before falling back to the browser (suggest ~3 s).
- Testing: Rust side unit/integration tests of the native host framing and install
  (temp HOME, no real browser); extension logic unit-tested with mocked `chrome.*` APIs
  (e.g. node's built-in test runner, no npm dependencies if possible); a manual Chrome
  check documented.
