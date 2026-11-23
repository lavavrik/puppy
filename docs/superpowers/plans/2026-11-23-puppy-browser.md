# puppy browser download pickup Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Chrome-family browsers hand every download to puppy automatically, cookies, referer and user agent included, through a native messaging host. The browser download is cancelled only after puppy has accepted it.

**Architecture:** A hidden `puppy native-host` subcommand speaks the native messaging framing on stdin/stdout. It maps small versioned JSON messages (`hello`, `add`, `remove`) onto the existing daemon client, which autostarts the daemon like the CLI does. `puppy browser install|uninstall|status` writes the host manifests per browser and OS. A plain-JS MV3 extension under `extension/` takes downloads over: pause, send, then cancel and erase on accept, or resume. It has exclusion settings, a one-shot bypass, an options page and a popup. The daemon clears a download's stored request context once it completes or is removed.

**Tech Stack:** Rust 2024, tokio, clap, serde_json, `url`, `percent-encoding`, `winreg` (Windows only). The extension is Manifest V3 in plain ES modules with no build step and no npm dependencies. Its tests use node 24's built-in `node:test` with mocked `chrome.*`.

**Spec:** `docs/superpowers/specs/2026-11-23-puppy-browser-design.md` (binding, including "Controller calls").

## Global Constraints

- No network port: only the browser-launched `puppy native-host` talks to the daemon, over the existing local socket / named pipe (`Client`).
- Cookies (and auth, header values) never appear in logs, `puppy info`, `--json` output or the resume sidecar. They live in `state.json` (mode 0600) while the download is unfinished. They are cleared from the stored record once it completes or is removed.
- "Accepted" means the daemon's `Add` returned a download id. "Refused" means any `Add` error, or the host or daemon being unreachable. In both refused cases the browser keeps the download.
- The extension waits at most **10 s** (`TIMEOUT_MS = 10_000`) for the host. If there is no reply, it resumes the browser download. If an `accepted` arrives later, it sends `remove` with `delete_files: false`. Every request carries a client request id that the reply echoes.
- `blob:`, `data:` and `file:` URLs (and every other non-http(s) scheme), POST-backed downloads where detectable, and anything puppy refuses always stay with the browser.
- Bypass: popup button **"Let the browser take the next download"** and the `commands` shortcut, default **Alt+Shift+B**. It is one-shot and expires after **60 s**.
- Extension permissions are exactly `downloads`, `cookies`, `nativeMessaging`, `storage`. The only host permission is `<all_urls>` (needed by `cookies`). No content scripts and no `webRequest`.
- Settings live in `storage.sync`; the recent list lives in `storage.session`.
- The host is `puppy native-host`, hidden from help, and accepts the browser's extra argv (origin on Linux/macOS, `--parent-window=` on Windows). Framing is a 4-byte native-endian length followed by UTF-8 JSON. Host→browser messages are at most **1 MiB**.
- Host name `dev.puppy.native_host`. `allowed_origins` contains only the fixed extension ID. The manifest `key` is the committed public key; the private key is never committed.
- Supported browsers are Chrome, Chromium, Brave, Edge and Vivaldi. On Linux and macOS the manifests go in the per-user NativeMessagingHosts dirs; on Windows under HKCU via `winreg` behind `[target.'cfg(windows)'.dependencies]`. Flatpak/Snap browsers are out of scope (documented). Install writes for every browser whose config dir exists; `--browser <name>` forces one.
- Files go to puppy's own download dir (`output_dir: None`). The browser's "ask where to save" is not mirrored.
- Firefox is designed in but not shipped: there is one source tree, a compat layer, and the `allowed_extensions` manifest form (built and unit-tested, never installed).
- The daemon protocol does not change (`puppy_proto::PROTOCOL_VERSION` stays 1). The host↔extension protocol is separate (`HOST_PROTOCOL = 1`).
- Cross-platform: Linux, macOS and Windows, on x86_64 and aarch64.
- Verify locally only, with `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all --check` and `node --test 'extension/tests/*.test.js'`. Never trigger GitHub Actions.

## Decisions the spec left open

- **Message schema** (host protocol 1):
  - Browser→host messages are `{"id": "<uuid>", "protocol": 1, "type": "hello", "extension_version": "0.1.0"}`, `{…, "type": "add", "url", "final_url"?, "filename"?, "referrer"?, "cookies"?, "user_agent"?}` and `{…, "type": "remove", "download_id": 7}`.
  - Host→browser replies are `{"id", "type": "hello", "protocol": 1, "puppy_version", "daemon_version": "…"|null, "daemon_error": "…"|null}`, `{"id", "type": "accepted", "download_id"}`, `{"id", "type": "removed"}` and `{"id", "type": "error", "code": "refused"|"unreachable"|"protocol_mismatch"|"bad_message", "message"}`. An error reply has `id: null` when the request's id could not be read.
- **One host process per message:** the extension uses `runtime.sendNativeMessage`, not a long-lived `connectNative` port. A permanent port would keep a daemon client connected for as long as the browser runs, so an autostarted daemon could never idle-exit. It would also tie the takeover to service-worker lifetime.
- **Handshake:** with no long-lived port, `hello` is the first exchange of each service-worker start (`runtime.onStartup` / `onInstalled`) and of every options or popup page open. The result is cached in `storage.session.status`. A cached `mismatch` makes the extension leave downloads alone without pausing them. As a guard, every `add` and `remove` also carries `protocol`, and the host answers a different one with `protocol_mismatch`. The host always answers `hello`.
- **URL:** puppy gets the download's original `url` (puppy follows redirects itself and re-resolves short-lived signed redirect targets on resume). Cookies are read for that `url`. `final_url` is sent for diagnostics only; the extension also uses it for site exclusions. `user:pass@` is percent-decoded into `basic_auth` and stripped from the URL, so `puppy ls` never shows it.
- **Event hook:** on Chrome, `downloads.onDeterminingFilename`. The listener returns `true` (async suggest), pauses first, and only then calls `suggest()`. That holds the download before it gets a final path and gives Chrome's final filename. Firefox, which lacks the event, falls back to `onCreated`. Both go through `compat.onNewDownload`.
- **POST detection:** this is not detectable with the permissions above, because `DownloadItem` has no method and `webRequest` is excluded. It is documented as a known limit.
- **Also left to the browser:** incognito downloads (their cookies are in another store, and puppy's state file would keep them), downloads started by an extension (`byExtensionId`), downloads already complete or not `in_progress` after the pause, and any download whose cookies cannot be read.
- **Site exclusions** match the host of `url`, `final_url` or `referrer`. An entry `example.com` matches `example.com` and `*.example.com` but not `badexample.com`. A leading `*.`, a scheme or a path in an entry is stripped. File-type entries are extensions, case-insensitive, with an optional leading dot.
- **Windows spawn from the host:** Chrome runs native hosts in a job object. The detached daemon is spawned with `CREATE_BREAKAWAY_FROM_JOB` added, and retried without it if the spawn fails with access denied. Linux and macOS already `setsid`.
- **Windows manifest files** go in `%LOCALAPPDATA%\puppy\NativeMessagingHosts\<browser>\dev.puppy.native_host.json`, with one file per browser so uninstall is per browser. The HKCU key's default value points to that file. On Windows, "config dir exists" means `%LOCALAPPDATA%\<vendor>\User Data`.
- **Host path:** `canonicalize(current_exe())`, with a Windows `\\?\` verbatim prefix stripped (Chrome cannot start `\\?\C:\…`; `\\?\UNC\…` is kept as is).
- **Late remove** uses `delete_files: false`, as the spec says. A `remove` for an unknown id replies `removed`, so it is idempotent.
- **Old state files:** completed records restored with a context get it cleared on load. A `RequestContext` `Debug` impl redacts cookies, header values and passwords, as a guard against future log lines.
- **UI details:** the bypass badge text is `1` while the bypass is armed, cleared when it is consumed or found expired. The recent list keeps 5 entries. The minimum size is entered in MiB. `puppy browser status` always exits 0.

## Review Focus

1. **A tiny download that finishes before the pause takes effect.** It is already `complete` in the browser, so handing it to puppy would download it twice. It must be left to the browser with nothing sent. Test: Task 4 `completed_before_pause_is_left`.
2. **Two downloads started together** (a page that triggers several). Each takeover must act only on the reply carrying its own request id, and one host error must not resume or cancel the other. Test: Task 4 `concurrent_downloads_keep_their_replies`.
3. **Credentials in the URL with escapes** (`https://us%40er:p%3Ass@host/f`). They must reach puppy as `basic_auth ("us@er", "p:ss")`, and neither `puppy ls` nor `info` may show them. Test: Task 2 `userinfo_moves_to_basic_auth` and `native_host_binary_autostarts_daemon`.
4. **Incognito downloads.** They must stay in the browser untouched (no pause), so incognito cookies never land in `state.json`. Test: Task 4 `unsupported_downloads_are_never_paused`.
5. **Odd filenames from the browser:** a full path (`C:\Users\me\Downloads\a.zip`, `dir/a.zip`), an empty string, `..`, or Unicode (`отчёт 2024.pdf`). Puppy must get just a name or none. Test: Task 2 `filename_is_reduced_to_a_name`.

---

## File Structure

```
crates/puppy-core/src/types.rs                 RequestContext: redacting Debug
crates/puppy-daemon/src/manager/mod.rs         data_completed clears rec.request.context
crates/puppy-daemon/src/manager/restore.rs     completed records restored without context
crates/puppy-daemon/tests/manager.rs           context-clearing tests
crates/puppy/tests/secrets.rs                  e2e leak tests (real binary)
crates/puppy-client/src/spawn.rs               Windows: CREATE_BREAKAWAY_FROM_JOB with fallback
crates/puppy/Cargo.toml                        url, percent-encoding; cfg(windows) winreg
crates/puppy/src/native_host/mod.rs            serve loop, Connector, run()
crates/puppy/src/native_host/framing.rs        read_frame / write_frame
crates/puppy/src/native_host/message.rs        envelope, replies, AddMsg → AddRequest
crates/puppy/src/browser/mod.rs                constants, Browser, Os, locations, host manifest, check_manifest
crates/puppy/src/browser/registry.rs           cfg(windows) HKCU helpers
crates/puppy/src/cli/browser.rs                install / uninstall / status commands
crates/puppy/src/main.rs                       NativeHost (hidden), Browser subcommands
crates/puppy/tests/browser.rs                  CLI tests: native-host binary, install under temp HOME
extension/manifest.json                        MV3, key, permissions, commands
extension/package.json                         {"type":"module","private":true} for node
extension/background.js                        wiring only
extension/lib/{compat,settings,rules,request,native,state,takeover,ui}.js
extension/{options,popup}.{html,js}, extension/ui.css
extension/tests/{fake-chrome.js,*.test.js}
README.md                                      "Browser downloads" section, layout row
.gitignore                                     *.pem
```

---

### Task 1: Daemon forgets a download's context when it completes or is removed

**Files:**
- Modify: `crates/puppy-core/src/types.rs` (replace `Debug` in the `RequestContext` derive with a manual impl; test inline), `crates/puppy-daemon/src/manager/mod.rs` (`data_completed`), `crates/puppy-daemon/src/manager/restore.rs` (restore loop)
- Test: `crates/puppy-daemon/tests/manager.rs`, create `crates/puppy/tests/secrets.rs`

**Interfaces:**
- Produces: `impl std::fmt::Debug for RequestContext`, printing header names with value `"<redacted>"`, `cookies: Some("<redacted>")`, `basic_auth: Some((user, Some("<redacted>")))`, and `referer` and `user_agent` as they are. After `data_completed`, `e.rec.request.context == RequestContext::default()`; the caller already persists (`transition` returns true). A restored `StoredState::Completed` record gets the same treatment, and the manager persists once after restore if any record was cleared.

- [ ] **Step 1: Write the failing tests**

`types.rs`:
```rust
#[test]
fn debug_redacts_secrets() {
    let c = RequestContext {
        headers: vec![("Authorization".into(), "Bearer HDRSECRET".into())],
        cookies: Some("sid=COOKIESECRET".into()),
        referer: Some("https://example.com/p".into()),
        user_agent: Some("UA/1".into()),
        basic_auth: Some(("alice".into(), Some("PWSECRET".into()))),
    };
    let s = format!("{c:?}");
    for secret in ["HDRSECRET", "COOKIESECRET", "PWSECRET"] { assert!(!s.contains(secret), "{s}"); }
    for shown in ["Authorization", "alice", "https://example.com/p", "UA/1"] { assert!(s.contains(shown), "{s}"); }
}
```
`tests/manager.rs` (helper `stored(env, id) -> serde_json::Value`, like `stored_limit`; `Env::req_with_cookie(&s, name)` sets `context.cookies = Some("sid=COOKIESECRET")`, `user_agent`, `basic_auth`):
```rust
#[tokio::test]
async fn completed_download_forgets_its_context() {
    // add req_with_cookie, wait_completed, m.save_state(TIMEOUT).await
    // stored(&env, id)["request"]["context"] == json!({"headers":[],"cookies":null,"referer":null,"user_agent":null,"basic_auth":null})
    // and the state file text does not contain "COOKIESECRET"
}
#[tokio::test]
async fn unfinished_download_keeps_its_context() {
    // throttled server (bytes_per_sec 64 KiB, len 4 MiB): wait_active_with_bytes, save_state
    // stored(...)["request"]["context"]["cookies"] == "sid=COOKIESECRET"   (option A: persisted privately)
}
#[tokio::test]
async fn restored_completed_download_forgets_its_context() {
    // write a state.json (PersistedState with one StoredDownload, state Completed, context with cookies) to env.state_path()
    // start the manager, save_state → file does not contain "COOKIESECRET"; the download is listed Completed
}
#[tokio::test]
async fn removed_download_leaves_no_context() {
    // throttled active download with cookies; m.remove(id, true); save_state → file has no "COOKIESECRET"
}
```
`crates/puppy/tests/secrets.rs` (uses `common::Env`; every command run with `RUST_LOG=debug` added, so the autostarted daemon logs verbosely):
```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cookies_never_leak_and_go_after_completion() {
    // server len 4 MiB, bytes_per_sec 128 KiB
    // puppy add --cookie "sid=COOKIESECRET" -H "Authorization: Bearer HDRSECRET" --referer https://example.com/p <url>
    // poll `puppy --json info 1` until done_bytes > 0
    // the server saw a request with Cookie "sid=COOKIESECRET" (requests_seen)
    // while active: state.json contains COOKIESECRET; the sidecar (puppy_core::http::resume::sidecar_path of the final path) does not;
    //   stdout of `puppy info 1`, `puppy --json info 1`, `puppy --json ls`, `puppy ls` contains neither secret
    // s.set_cfg(|c| c.bytes_per_sec = None); `puppy wait 1` exits 0
    // poll (≤ TIMEOUT) until state.json contains neither secret
    // log_dir/puppyd.log contains neither secret (and is non-empty)
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p puppy-core debug_redacts && cargo test -p puppy-daemon --test manager context && cargo test -p puppy --test secrets`
Expected: `debug_redacts_secrets` fails (the derived Debug prints the secrets), the context tests fail on `COOKIESECRET` still being in `state.json`, and `unfinished_download_keeps_its_context` passes already.

- [ ] **Step 3: Implement**

Write the manual `Debug` impl (`f.debug_struct("RequestContext")`). In `data_completed`, reset `e.rec.request.context` to the default; do it unconditionally, also when `finished_unix` was already set. In `restore`, bind `mut rec` and clear the context for `StoredState::Completed`. Removal already drops the record; the test proves the persisted file follows. If the e2e test finds a secret in `puppyd.log` from a dependency's debug output, fix it at the source (do not weaken the test).

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p puppy-core && cargo test -p puppy-daemon && cargo test -p puppy --test secrets`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/puppy-core/src/types.rs crates/puppy-daemon/src/manager crates/puppy-daemon/tests/manager.rs crates/puppy/tests/secrets.rs
git commit -m "feat(daemon): clear a download's request context once it completes or is removed"
```

---

### Task 2: `puppy native-host`

**Files:**
- Create: `crates/puppy/src/native_host/{mod.rs,framing.rs,message.rs}` (unit tests inline; the serve tests in `mod.rs` run an in-process daemon as `crates/puppy-daemon/tests/lifecycle.rs` does: `run_daemon(DaemonOptions { paths: Some(Paths::with_home(..)), http: Some(..), torrent_test: Some(Default::default()), .. })`)
- Modify: `crates/puppy/src/main.rs` (`mod native_host;`, hidden `NativeHost` variant), `crates/puppy/Cargo.toml` (`url.workspace = true`, `percent-encoding.workspace = true`), `crates/puppy-client/src/spawn.rs` (Windows `detach`)
- Test: create `crates/puppy/tests/browser.rs` (`mod common;`)

**Interfaces:**
- Consumes: `puppy_client::{Client, ClientError}`, `Client::connect_or_spawn(paths, "puppy-native-host", exe, &[])`, `Client::add`, `Client::call(Request::Remove { id, delete_files: false })`, `Client::daemon_version()`, `cli::is_mismatch`.
- Produces:
```rust
// framing.rs
pub const MAX_OUT: usize = 1 << 20;   // host → browser (Chrome's limit)
pub const MAX_IN: usize = 1 << 20;    // browser → host (our cap; requests are small)
#[derive(thiserror::Error, Debug)]
pub enum FrameError { #[error(transparent)] Io(#[from] std::io::Error), #[error("message of {len} bytes is over {max}")] TooLarge { len: usize, max: usize }, #[error("message cut short")] Truncated }
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Vec<u8>>, FrameError>;  // None: clean EOF before a length
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, json: &[u8]) -> Result<(), FrameError>; // u32::to_ne_bytes + body + flush

// message.rs
pub const HOST_PROTOCOL: u32 = 1;
pub const MAX_MESSAGE_CHARS: usize = 1000;          // error messages are cut to this
#[derive(Deserialize, Debug)] pub struct Envelope { pub id: String, pub protocol: u32, #[serde(flatten)] pub body: Incoming }
#[derive(Deserialize, Debug)] #[serde(tag = "type", rename_all = "snake_case")]
pub enum Incoming { Hello { extension_version: String }, Add(AddMsg), Remove { download_id: DownloadId } }
#[derive(Deserialize, Debug, Default)] pub struct AddMsg { pub url: String, #[serde(default)] pub final_url: Option<String>, #[serde(default)] pub filename: Option<String>, #[serde(default)] pub referrer: Option<String>, #[serde(default)] pub cookies: Option<String>, #[serde(default)] pub user_agent: Option<String> }
#[derive(Serialize, Debug, PartialEq)] pub struct Reply { pub id: Option<String>, #[serde(flatten)] pub body: Outgoing }
#[derive(Serialize, Debug, PartialEq)] #[serde(tag = "type", rename_all = "snake_case")]
pub enum Outgoing { Hello { protocol: u32, puppy_version: String, daemon_version: Option<String>, daemon_error: Option<String> }, Accepted { download_id: DownloadId }, Removed, Error { code: ErrorKind, message: String } }
#[derive(Serialize, Debug, PartialEq, Clone, Copy)] #[serde(rename_all = "snake_case")]
pub enum ErrorKind { Refused, Unreachable, ProtocolMismatch, BadMessage }
pub fn parse(bytes: &[u8]) -> Result<Envelope, (Option<String>, String)>;  // Err: (id if readable, why)
pub fn to_add_request(m: &AddMsg) -> Result<AddRequest, String>;
pub fn basename(name: &str) -> Option<String>;
pub fn error_kind(e: &ClientError) -> ErrorKind;   // Daemon{code != ProtocolMismatch} → Refused; everything else → Unreachable

// mod.rs
pub trait Connector { fn connect(&mut self) -> impl Future<Output = Result<Client, ClientError>> + Send; }
pub async fn serve<R, W, C>(input: R, output: W, connector: C) -> Result<(), FrameError>
where R: AsyncRead + Unpin, W: AsyncWrite + Unpin, C: Connector;
pub async fn run() -> Result<(), CliError>;   // tokio stdin/stdout, Paths::resolve + current_exe, connect_or_spawn
```

- [ ] **Step 1: Write the failing tests**

`framing.rs`:
```rust
#[tokio::test] async fn frame_roundtrip_native_endian() {
    // write_frame(br#"{"a":1}"#) → buffer == [7u32.to_ne_bytes(), body].concat(); read_frame → Some(body); read again → None
}
#[tokio::test] async fn oversized_reply_is_refused() {
    // vec![b' '; MAX_OUT + 1] → Err(TooLarge { len: MAX_OUT + 1, max: 1 << 20 }) and the buffer stays empty; MAX_OUT bytes → Ok
}
#[tokio::test] async fn oversized_or_cut_input_is_an_error() {
    // header (MAX_IN + 1) → TooLarge; header 10 + 3 bytes + EOF → Truncated; 2 header bytes + EOF → Truncated
}
```
`message.rs`:
```rust
#[test] fn add_maps_to_add_request() {
    // AddMsg { url: "https://dl.example.com/a/b.iso?x=1", filename: Some("b.iso"), referrer: Some("https://example.com/page"),
    //          cookies: Some("sid=SECRET; t=2"), user_agent: Some("Mozilla/5.0 X"), final_url: Some("https://cdn.example.net/b.iso") }
    // → AddRequest { source: Http { url: "https://dl.example.com/a/b.iso?x=1" }, output_dir: None, filename: Some("b.iso"),
    //     context: RequestContext { headers: vec![], cookies: Some("sid=SECRET; t=2"), referer: Some("https://example.com/page"),
    //              user_agent: Some("Mozilla/5.0 X"), basic_auth: None }, files: FileChoice::All, seed: None }
}
#[test] fn userinfo_moves_to_basic_auth() {
    // "https://us%40er:p%3Ass@h.example/f" → url "https://h.example/f", basic_auth Some(("us@er", Some("p:ss")))
    // "https://user@h.example/f" → Some(("user", None)); "https://:pw@h.example/f" → Some(("", Some("pw")))
}
#[test] fn non_http_is_refused() {
    // "ftp://x/f", "blob:https://x/1", "data:,hi", "file:///etc/passwd" → Err containing "unsupported"; "not a url" → Err containing "invalid URL"
}
#[test] fn filename_is_reduced_to_a_name() {
    // "dir/sub/a.zip" → "a.zip"; r"C:\Users\me\Downloads\a.zip" → "a.zip"; "отчёт 2024.pdf" → same
    // "", ".", "..", "a/", "   " → None
}
#[test] fn empty_strings_are_none() { /* referrer/cookies/user_agent Some("") → None in the context */ }
#[test] fn envelopes_and_replies() {
    // parse(r#"{"id":"r1","protocol":1,"type":"remove","download_id":7}"#) → Remove { download_id: 7 }
    // parse(r#"{"id":"r1","protocol":1,"type":"nope"}"#) → Err((Some("r1"), _)); parse(b"not json") → Err((None, _))
    // serde_json::to_value(Reply { id: Some("r1".into()), body: Outgoing::Accepted { download_id: 7 } }) == json!({"id":"r1","type":"accepted","download_id":7})
    // Error { code: ProtocolMismatch, .. } → "code":"protocol_mismatch"; Removed → {"id":"r1","type":"removed"}
}
#[test] fn client_errors_map_to_kinds() {
    // Daemon { code: BadRequest, .. } → Refused; Daemon { code: ProtocolMismatch, .. } → Unreachable; Io(..), Timeout, Disconnected → Unreachable
}
```
`mod.rs` (helper `exchange(connector, frames: &[serde_json::Value]) -> Vec<serde_json::Value>` over `tokio::io::duplex`. `Counting` connector: `Client::connect(&socket, "test")` plus a call counter. `Failing` connector: returns `ClientError::Io(ConnectionRefused)`):
```rust
#[tokio::test] async fn hello_reports_versions()            // reply == {"id","type":"hello","protocol":1,"puppy_version":CARGO_PKG_VERSION,"daemon_version":CARGO_PKG_VERSION,"daemon_error":null}
#[tokio::test] async fn hello_without_daemon_still_answers() // Failing → daemon_version null, daemon_error contains "cannot talk to the daemon"
#[tokio::test] async fn add_is_accepted_with_an_id()        // add (http://127.0.0.1:9/never, cookies "sid=SECRET") → {"id":same,"type":"accepted","download_id":1}; state.json eventually has that download with output_dir null and cookies "sid=SECRET"
#[tokio::test] async fn unreachable_daemon_is_reported()     // Failing: add → code "unreachable"; a following hello is still answered
#[tokio::test] async fn refused_url_never_reaches_the_daemon() // add "ftp://x/f" → code "refused"; Counting.calls == 0
#[tokio::test] async fn remove_removes_without_deleting()   // add then remove → "removed"; list empty; remove of id 999 → "removed"
#[tokio::test] async fn protocol_mismatch_is_refused()       // add with "protocol":2 → code "protocol_mismatch"; calls == 0; hello with protocol 2 → hello reply
#[tokio::test] async fn bad_message_is_reported_and_serving_goes_on() // "not json" → {"id":null,"code":"bad_message"}; then hello answered
#[tokio::test] async fn connects_once_for_many_messages()   // two adds → Counting.calls == 1
#[tokio::test] async fn eof_ends_serving()                  // input dropped → serve returns Ok(())
```
`crates/puppy/tests/browser.rs`:
```rust
#[test] fn native_host_is_hidden_and_takes_browser_args() {
    // `puppy --help` stdout does not contain "native-host"
    // `puppy native-host chrome-extension://abcdefghijklmnopabcdefghijklmnop/ --parent-window=0` with stdin null → exit 0, empty stdout
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_host_binary_autostarts_daemon() {
    // Env::new(); no daemon running. Spawn `puppy native-host chrome-extension://x/` (env.command, stdin/stdout piped)
    // write a hello frame and an add frame (url "https://us%40er:pw@127.0.0.1:9/never.bin", filename "never.bin"), close stdin
    // read two frames: hello with daemon_version Some; accepted with download_id
    // `puppy --json ls` → one snapshot whose source is "https://127.0.0.1:9/never.bin" (no userinfo)
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p puppy native_host && cargo test -p puppy --test browser`
Expected: compile errors (`native_host` module missing). After the module skeleton exists, `native_host_is_hidden_…` fails on an unknown subcommand.

- [ ] **Step 3: Implement**

- `serve` loop: read a frame (`None` → return `Ok`). Parse it. Dispatch it. Write exactly one reply per message.
- On `TooLarge` input, reply `bad_message` with `id: null` and return the error. `run` then exits 1.
- `hello` connects lazily: a failed connect fills `daemon_error`. A cached `Option<Client>` is reused across messages and dropped after an `Unreachable` error so the next message reconnects. Never retry an `add`: a retried add after a dropped reply could add twice.
- Daemon `NotFound` on `remove` → `removed`.
- An `is_mismatch` error's message gets the CLI's hint ("an incompatible puppy daemon is running — run `puppy stop` first").
- Messages are cut to `MAX_MESSAGE_CHARS`. If a serialized reply still exceeds `MAX_OUT`, send a `bad_message` error instead.
- Nothing may write to stdout except `write_frame`, and the host logs nothing.
- In `main.rs` add the variant:
```rust
/// Native messaging host started by the browser extension.
#[command(hide = true)]
NativeHost { #[arg(trailing_var_arg = true, allow_hyphen_values = true)] browser_args: Vec<String> },
```
  dispatched to `native_host::run()`.
- Windows `detach` in `spawn.rs`: spawn with `CREATE_BREAKAWAY_FROM_JOB` added to the flags; on an `io::Error` with `raw_os_error() == Some(5)`, spawn again without it. This needs the `detach`/spawn split reshaped so the command can be retried; keep Unix behaviour unchanged.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p puppy && cargo clippy -p puppy -p puppy-client --all-targets -- -D warnings`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/puppy/Cargo.toml Cargo.lock crates/puppy/src/main.rs crates/puppy/src/native_host crates/puppy/tests/browser.rs crates/puppy-client/src/spawn.rs
git commit -m "feat(browser): puppy native-host speaking native messaging to the daemon"
```

---

### Task 3: `puppy browser install | uninstall | status` and the fixed extension ID

**Files:**
- Create: `crates/puppy/src/browser/mod.rs` (unit tests inline), `crates/puppy/src/browser/registry.rs` (`#[cfg(windows)]`), `crates/puppy/src/cli/browser.rs`, `extension/manifest.json` (key only, completed in Task 4)
- Modify: `crates/puppy/src/main.rs` (`mod browser;`, `Browser` subcommand), `crates/puppy/src/cli/mod.rs` (`pub mod browser;`), `crates/puppy/Cargo.toml` (`[target.'cfg(windows)'.dependencies] winreg = "0.55"`, using the newest 0.x that builds), `.gitignore` (`*.pem`)
- Test: `crates/puppy/tests/browser.rs`

**Interfaces:**
- Consumes: nothing from Task 2 except that `puppy native-host` exists (the manifest points at `puppy` and the browser adds the argv).
- Produces:
```rust
// browser/mod.rs
pub const HOST_NAME: &str = "dev.puppy.native_host";
pub const HOST_DESCRIPTION: &str = "puppy download manager";
pub const EXTENSION_ID: &str = "<32 chars a-p, from Step 1>";
#[derive(clap::ValueEnum, Serialize, Clone, Copy, Debug, PartialEq, Eq)] #[serde(rename_all = "lowercase")]
pub enum Browser { Chrome, Chromium, Brave, Edge, Vivaldi }
impl Browser { pub const ALL: [Browser; 5]; pub fn name(self) -> &'static str; }   // "chrome", …
#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum Os { Linux, MacOs, Windows }
impl Os { pub fn current() -> Os; }                  // other Unixes use the Linux layout
pub struct BrowserEnv { pub home: PathBuf, pub xdg_config_home: Option<PathBuf>, pub local_app_data: Option<PathBuf> }
impl BrowserEnv { pub fn from_env() -> anyhow::Result<Self>; }   // HOME / XDG_CONFIG_HOME (empty = unset) / LOCALAPPDATA
#[derive(Debug, Clone, PartialEq, Eq)] pub struct Location { pub config_dir: PathBuf, pub manifest: PathBuf, pub registry_key: Option<String> }
pub fn location(b: Browser, os: Os, env: &BrowserEnv) -> Location;
pub enum Allowed<'a> { ChromeOrigin(&'a str), FirefoxExtension(&'a str) }   // Firefox: designed in, not installed
pub fn host_manifest(exe: &Path, allowed: Allowed<'_>) -> serde_json::Value;
pub fn manifest_exe_path(p: &str) -> String;          // strips "\\?\" before a drive letter
pub fn canonical_exe() -> anyhow::Result<PathBuf>;    // canonicalize(current_exe) then manifest_exe_path
#[derive(Debug, Clone, PartialEq, Eq, Serialize)] #[serde(tag = "state", rename_all = "snake_case")]
pub enum HostState { Missing, Ok, StalePath { found: PathBuf }, WrongOrigin, Unreadable { error: String } }
pub fn check_manifest(manifest: &Path, exe: &Path) -> HostState;
// registry.rs (windows): pub fn set(key: &str, manifest: &Path) -> io::Result<()>; pub fn get(key: &str) -> io::Result<Option<PathBuf>>; pub fn delete(key: &str) -> io::Result<bool>;
// cli/browser.rs
pub async fn install(json: bool, only: Option<Browser>) -> Result<(), CliError>;
pub async fn uninstall(json: bool, only: Option<Browser>) -> Result<(), CliError>;
pub async fn status(json: bool) -> Result<(), CliError>;
```
- Locations (`H` = home, `C` = `xdg_config_home` or `H/.config`, `L` = `local_app_data`; the manifest file is always `<dir>/dev.puppy.native_host.json`):

| Browser | Linux config dir (+`/NativeMessagingHosts`) | macOS `H/Library/Application Support/` + (+`/NativeMessagingHosts`) | Windows key `Software\…\NativeMessagingHosts\dev.puppy.native_host` | Windows config dir `L\…\User Data` |
|---|---|---|---|---|
| chrome | `C/google-chrome` | `Google/Chrome` | `Google\Chrome` | `Google\Chrome` |
| chromium | `C/chromium` | `Chromium` | `Chromium` | `Chromium` |
| brave | `C/BraveSoftware/Brave-Browser` | `BraveSoftware/Brave-Browser` | `BraveSoftware\Brave-Browser` | `BraveSoftware\Brave-Browser` |
| edge | `C/microsoft-edge` | `Microsoft Edge` | `Microsoft\Edge` | `Microsoft\Edge` |
| vivaldi | `C/vivaldi` | `Vivaldi` | `Vivaldi` | `Vivaldi` |

  Windows manifest file: `L/puppy/NativeMessagingHosts/<name>/dev.puppy.native_host.json`. Before committing, check each row against the vendor's documentation; if Vivaldi on Windows reads Chrome's key instead, write that one for Vivaldi and note it in the README.

- [ ] **Step 1: Generate the extension key (never commit the private key)**

```bash
K="$(mktemp -d)/puppy-ext-key.pem"   # outside the repo
openssl genrsa -out "$K" 2048
openssl rsa -in "$K" -pubout -outform DER 2>/dev/null | base64 -w0          # → manifest "key"
openssl rsa -in "$K" -pubout -outform DER 2>/dev/null | sha256sum | head -c32 | tr 0-9a-f a-p   # → EXTENSION_ID
rm "$K"
```
Write `extension/manifest.json` as `{"manifest_version": 3, "name": "puppy", "version": "0.1.0", "key": "<base64>"}`, put the ID in `EXTENSION_ID`, and add `*.pem` to `.gitignore`.

- [ ] **Step 2: Write the failing tests**

`browser/mod.rs`:
```rust
#[test] fn linux_locations()     // every row of the table, with xdg None and with xdg Some("/x") (→ /x/google-chrome/…); registry_key None
#[test] fn macos_locations()     // every row, H = "/h"
#[test] fn windows_locations()   // registry_key == Some(r"Software\Google\Chrome\NativeMessagingHosts\dev.puppy.native_host") etc.; manifest under L/puppy/NativeMessagingHosts/chrome/; config_dir L/Google/Chrome/User Data
#[test] fn chrome_manifest_json() // host_manifest("/opt/puppy", ChromeOrigin(EXTENSION_ID)) == json!({"name":"dev.puppy.native_host","description":"puppy download manager","path":"/opt/puppy","type":"stdio","allowed_origins":[format!("chrome-extension://{EXTENSION_ID}/")]})
#[test] fn firefox_manifest_json() // FirefoxExtension("puppy@example") → "allowed_extensions":["puppy@example"], no "allowed_origins"
#[test] fn verbatim_prefix_stripped() // r"\\?\C:\bin\puppy.exe" → r"C:\bin\puppy.exe"; r"\\?\UNC\srv\x\puppy.exe" and "/usr/bin/puppy" unchanged
#[test] fn extension_id_shape()   // 32 chars, all in 'a'..='p'
#[test] fn check_manifest_states() // temp dir: missing → Missing; written for exe A, checked with A → Ok, with B → StalePath { found: A }; other origin → WrongOrigin; "{" → Unreadable
```
`tests/browser.rs` (`#[cfg(unix)]`; runs the binary with `HOME=<tmp>`, `XDG_CONFIG_HOME` removed; expected dirs come from a local helper using the Linux or macOS layout per `cfg!(target_os = "macos")`; `exe = std::fs::canonicalize(BIN)`):
```rust
#[test] fn install_status_uninstall_under_temp_home() {
    // create the chrome and brave config dirs only (not NativeMessagingHosts)
    // `puppy browser install` → exit 0; both manifests exist with path == exe and allowed_origins == [chrome-extension://ID/]; no chromium dir created
    // `puppy --json browser status` → browsers[chrome].host.state == "ok", found true; chromium found false, host.state "missing"
    // `puppy browser status` text: a "chrome" line containing "ok"; last line "extension id: <ID>"
    // `puppy browser uninstall` → both manifests gone; status shows "missing"
}
#[test] fn install_without_browsers_fails_unless_forced() {
    // empty HOME: `puppy browser install` → exit 1, stderr contains "--browser"
    // `puppy browser install --browser chromium` → exit 0 and the chromium manifest exists (dirs created)
}
#[test] fn stale_path_is_flagged() {
    // install for chrome, then rewrite the manifest's "path" to "/old/puppy"
    // `puppy browser status` → exit 0, chrome line contains "stale", "/old/puppy" and "re-run puppy browser install"
    // `--json`: host == {"state":"stale_path","found":"/old/puppy"}
}
```

- [ ] **Step 3: Run them to verify they fail**

Run: `cargo test -p puppy browser:: && cargo test -p puppy --test browser`
Expected: compile errors (`browser` module missing).

- [ ] **Step 4: Implement**

- Install loops over `only.map_or(Browser::ALL.to_vec(), |b| vec![b])`. It skips browsers whose `config_dir` is missing unless forced. It creates the manifest's parent dirs and writes pretty JSON with `puppy_core::fsutil::atomic_write(path, bytes, false)`. On Windows it then calls `registry::set`.
- If nothing was written and nothing was forced, fail with `CliError::Other` "no Chrome-family browser found (Flatpak and Snap browsers are not supported); use --browser <name> to install for one anyway".
- Uninstall removes each existing manifest (and the registry key), whether or not the browser is found. If nothing was removed it prints `nothing to remove` and exits 0.
- Text output:
  - install: `installed for chrome: <manifest path>`
  - status, per browser: `<name padded to 9>  ok  <path>`, `not found`, `not installed`, `stale: points to <found> — re-run puppy browser install`, `wrong extension id — re-run puppy browser install`, or `unreadable: <error>`; then `extension id: <ID>`
- JSON output:
  - install: `{"installed":[{"browser","manifest","registry_key"}],"skipped":["…"]}`
  - uninstall: `{"removed":[{"browser","manifest"}]}`
  - status: `{"host_path","extension_id","browsers":[{"browser","found","manifest","registry_key","host":HostState}]}`
- On Windows, status reads the key first: no key → `Missing`; a key whose value points to a file other than `manifest` → `StalePath`.
- The `main.rs` variant is `Browser { #[command(subcommand)] action: BrowserAction }` with `Install { #[arg(long)] browser: Option<Browser> }`, `Uninstall { … }` and `Status`. Doc comments: "Set up browser download pickup (the puppy extension)", "Register puppy's native messaging host with Chrome-family browsers", "Remove the registration", "Show what is registered, per browser".

- [ ] **Step 5: Run the tests and the Windows check**

Run: `cargo test -p puppy && cargo clippy -p puppy --all-targets -- -D warnings && cargo check -p puppy --target x86_64-pc-windows-msvc`
Expected: tests and clippy PASS. The Windows `check` passes, or fails only in a C dependency's build script (e.g. `aws-lc-sys`, no MSVC toolchain here). Report which, and fix any error that points into `crates/puppy`.

- [ ] **Step 6: Commit**

```bash
git add .gitignore extension/manifest.json crates/puppy/Cargo.toml Cargo.lock crates/puppy/src crates/puppy/tests/browser.rs
git commit -m "feat(browser): puppy browser install/uninstall/status and the fixed extension id"
```

---

### Task 4: Extension core — manifest, takeover, exclusions, bypass

**Files:**
- Modify: `extension/manifest.json` (complete)
- Create: `extension/package.json` (`{"private": true, "type": "module", "scripts": {"test": "node --test \"tests/*.test.js\""}}`), `extension/background.js`, `extension/lib/{compat,settings,rules,request,native,state,takeover}.js`, `extension/tests/fake-chrome.js`, `extension/tests/{manifest,rules,takeover,background}.test.js`

**Interfaces:**
- Consumes: Task 2's host messages and replies (see "Decisions"); Task 3's `EXTENSION_ID` (read from `crates/puppy/src/browser/mod.rs` by the manifest test) and `HOST_NAME`.
- Produces (ES modules; nothing touches `chrome`/`browser` globals at import except `background.js`):
```js
// lib/compat.js
export function extensionApi(g = globalThis)          // g.browser ?? g.chrome
export function onNewDownload(api, handler)            // handler(item, release): Promise; release() calls suggest() once (no-op on Firefox/onCreated)
// lib/settings.js
export const DEFAULTS = { enabled: true, minSizeBytes: 0, skipExtensions: [], skipSites: [] };
export async function loadSettings(api)                // storage.sync "settings", merged over DEFAULTS
export async function saveSettings(api, settings)
export function normalizeExtensions(textOrList)        // "zip, .ISO  pdf" → ["zip","iso","pdf"] (commas/whitespace, dedup)
export function normalizeSites(textOrList)             // "Example.com\n*.foo.org/path, https://bar.net" → ["example.com","foo.org","bar.net"]
// lib/rules.js
export function unsupportedReason(item, ownId)         // string|null: non-http(s) scheme, incognito, byExtensionId, state !== "in_progress"
export function excludedReason(item, settings)         // string|null: disabled, below min size (totalBytes > 0 only), file type, site
export function siteMatches(host, site)
export function fileExtension(name)                    // lowercased, of the basename (split on / and \), "" if none
export function basename(name)
// lib/request.js
export function cookieHeader(cookies)                  // "a=1; b=2", longer cookie paths first, stable; "" → null
export function addMessage(item, cookie, userAgent, id) // {id, protocol: PROTOCOL, type:"add", url, final_url, filename: basename|null, referrer|null, cookies, user_agent}
// lib/native.js
export const HOST_NAME = "dev.puppy.native_host", PROTOCOL = 1, TIMEOUT_MS = 10_000;
export async function send(api, msg)                   // runtime.sendNativeMessage; rejects if reply.id !== msg.id
export async function hello(api, extensionVersion)     // → Status, with TIMEOUT_MS
//   Status = {state: "ok"|"no_host"|"no_daemon"|"mismatch"|"error", puppyVersion?, daemonVersion?, hostProtocol?, message?, at}
//   "no_host": rejection text contains "not found" or "forbidden"
// lib/state.js
export const BYPASS_MS = 60_000, RECENT_MAX = 5;
export async function armBypass(api, now)              // storage.session bypassUntil = now + BYPASS_MS; action badge "1"
export async function takeBypass(api, now)             // true once if armed and unexpired; clears it (and the badge) either way when set
export async function addRecent(api, entry)            // {name, ok, detail, at}, newest first, ≤ RECENT_MAX
export async function getRecent(api); export async function getStatus(api); export async function setStatus(api, status)
// lib/takeover.js
export async function handleDownload(api, item, release, { now = Date.now, userAgent = navigator.userAgent, ownId } = {})
//   → {kind: "taken", downloadId} | {kind: "left", reason} | {kind: "resumed", reason}
```
- Takeover order: `unsupportedReason` → `excludedReason` → `takeBypass` → cached status `mismatch` (each of these → `left`, no pause) → `downloads.pause`, then `release()`. A rejected pause → `left`. Then `downloads.search({id})`; not `in_progress` → `left`. Then `cookies.getAll({url})`; rejected → resume, `resumed`. Then `send(addMessage(...))` raced against `TIMEOUT_MS`:
  - `accepted` → `downloads.cancel`, then `downloads.erase({id})`; recent `ok: true`; `taken`.
  - error reply → resume; recent `ok: false, detail: message`; `protocol_mismatch` also caches status `mismatch`.
  - rejection → resume; recent `ok: false, detail: "puppy is not reachable: <error>"`.
  - timeout → resume; recent `ok: false, detail: "puppy did not answer in 10 s"`. A later `accepted` → `send({type: "remove", download_id, protocol, id: new})`, errors swallowed.
  - `release()` runs exactly once on every path (`finally`).
- `background.js`:
  - `onNewDownload(api, (item, release) => handleDownload(api, item, release, {ownId: api.runtime.id}))`
  - `api.commands.onCommand` `"bypass-next"` → `armBypass(api, Date.now())`
  - `runtime.onStartup` and `runtime.onInstalled` → `setStatus(api, await hello(api, api.runtime.getManifest().version))`
- Manifest (complete, keeping Task 3's `key`):
```json
{ "manifest_version": 3, "name": "puppy", "version": "0.1.0", "description": "Hands browser downloads to the puppy download manager",
  "minimum_chrome_version": "102", "key": "…",
  "permissions": ["downloads", "cookies", "nativeMessaging", "storage"], "host_permissions": ["<all_urls>"],
  "background": { "service_worker": "background.js", "type": "module" },
  "action": { "default_title": "puppy" },
  "commands": { "bypass-next": { "suggested_key": { "default": "Alt+Shift+B" }, "description": "Let the browser take the next download" } } }
```
  Task 5 adds `action.default_popup` and `options_ui`.

- [ ] **Step 1: Write the fake and the failing tests**

`tests/fake-chrome.js` exports `fakeApi({ cookies = [], reply, settings, session, downloads })`:
- a `calls` log, in order: `["pause", id]`, `["resume", id]`, `["cancel", id]`, `["erase", {id}]`, `["send", host, msg]`, `["badge", text]`, …
- `reply`: `(msg) => object | Promise | Error` (an `Error` rejects).
- `downloads.search` returns the items in `downloads` (state settable).
- events with `addListener` and `fire(...args)`; `storage.sync` and `storage.session` in memory (promise `get`/`set`/`remove`).
- `runtime.id = "ownid"` and `getManifest()` returning `{version: "0.1.0"}`; `onDeterminingFilename` is present unless `{firefox: true}`.

Item helper `item({id: 7, url: "https://dl.example.com/f.iso", filename: "f.iso", totalBytes: 5e6, state: "in_progress", incognito: false, referrer: "https://example.com/p", finalUrl: …})`. Timers via `t.mock.timers.enable({ apis: ["setTimeout", "Date"] })`.

`manifest.test.js`:
```js
test("permissions are exactly the four, plus <all_urls>, no content scripts", …)  // deepEqual sorted permissions; host_permissions ["<all_urls>"]; no content_scripts / webRequest
test("bypass command defaults to Alt+Shift+B", …)
test("key derives the extension id puppy installs", …)
  // id = sha256(Buffer.from(key, "base64")).hex.slice(0,32) mapped 0-9a-f → a-p (node:crypto)
  // equals /EXTENSION_ID: &str = "([a-p]{32})"/ in ../../crates/puppy/src/browser/mod.rs
```
`rules.test.js`:
```js
test("unsupported schemes and contexts", …)   // blob:, data:, file:, ftp:, filesystem: → reason; https/http → null; incognito → reason; byExtensionId "other" → reason; state "complete" → reason
test("size, type and site exclusions", …)     // min 2048: totalBytes 1000 → reason; -1 and 0 → null; skipExtensions ["iso"] vs "F.ISO" → reason; "a/b.tar.gz" ext "gz"
test("site matching", …)                      // "example.com" matches example.com, dl.example.com; not badexample.com; matched via url, final_url or referrer host
test("normalizers", …)                        // the examples from the Interfaces block; empty text → []
test("loadSettings merges defaults over partial stored settings", …)  // stored {minSizeBytes: 5} → {...DEFAULTS, minSizeBytes: 5}
test("cookieHeader", …)                       // [{name:"a",value:"1",path:"/"},{name:"b",value:"2",path:"/x"}] → "b=2; a=1"; [] → null
```
`takeover.test.js`:
```js
test("accepted download is paused, sent, then cancelled and erased", …)
  // calls order: pause(7) < release < send < cancel(7) < erase({id:7}); msg == {id: <uuid>, protocol: 1, type: "add", url, final_url,
  // filename: "f.iso", referrer, cookies: "sid=1", user_agent: "UA"}; outcome {kind:"taken", downloadId: 3}; recent[0] {name:"f.iso", ok:true}
test("error reply resumes the browser download", …)     // reply {type:"error", code:"refused", message:"no"} → resume(7), no cancel; recent ok:false detail "no"
test("missing host resumes the browser download", …)    // reply Error("Specified native messaging host not found.") → resume; detail starts "puppy is not reachable"
test("reply with another id is treated as an error", …)  // resume, no cancel
test("no reply in 10 s resumes; a late accept is removed in puppy", …)
  // reply = promise resolved by the test; tick(10_000) → resume(7); then resolve {type:"accepted", download_id: 9}
  // → a second send with {type:"remove", download_id: 9, protocol: 1}; never cancel
test("reply at 9999 ms is still in time", …)            // taken
test("protocol mismatch resumes and caches it; the next download is left unpaused", …)
test("unsupported_downloads_are_never_paused", …)       // blob:, data:, file:, incognito, byExtensionId → left; no pause, no send; release called once
test("completed_before_pause_is_left", …)               // search returns state "complete" after pause → left; no send, no resume
test("pause failure leaves the download alone", …)      // pause rejects → left; nothing sent
test("cookie read failure resumes", …)
test("bypass skips exactly one download within 60 s", …)
  // armBypass at t0; download at t0+59s → left "bypass"; next → taken; armed at t0, download at t0+61s → taken; badge "1" then ""
test("bypass is not used up by a download the browser keeps anyway", …)  // blob: download then a normal one → normal one left by bypass
test("disabled extension leaves everything", …)
test("concurrent_downloads_keep_their_replies", …)
  // ids 7 and 8 at once; host accepts 8, errors 7 (each echoing its own id) → cancel(8) only, resume(7) only
test("release is called exactly once on every path", …)  // across the outcomes above (counter per item)
```
`background.test.js`:
```js
test("background registers its listeners", …)  // globalThis.chrome = fakeApi(); await import("../background.js")
  // onDeterminingFilename, commands.onCommand, runtime.onStartup and onInstalled each have one listener;
  // firing onCommand("bypass-next") sets storage.session bypassUntil
test("Firefox falls back to onCreated", …)     // onNewDownload(fakeApi({firefox:true}), h) → onCreated listener; fire(item) → h(item, release) with release a no-op
test("determining-filename listener returns true and suggests after the pause", …)
```

- [ ] **Step 2: Run them to verify they fail**

Run: `node --test 'extension/tests/*.test.js'`
Expected: FAIL with module-not-found for `lib/*.js` and `background.js`, and the manifest tests failing on missing keys.

- [ ] **Step 3: Implement the modules and the manifest as in Interfaces**

Request ids come from `crypto.randomUUID()`. The timeout race uses `setTimeout`, cleared when the reply arrives. Store reasons as short English strings (`"not http(s)"`, `"incognito"`, `"bypass"`, …); Task 5's popup shows only recent handovers, not reasons.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `node --test 'extension/tests/*.test.js'`
Expected: PASS, with 0 failures.

- [ ] **Step 5: Commit**

```bash
git add extension
git commit -m "feat(extension): MV3 takeover of browser downloads with exclusions and bypass"
```

---

### Task 5: Options page, popup, README

**Files:**
- Create: `extension/lib/ui.js`, `extension/options.html`, `extension/options.js`, `extension/popup.html`, `extension/popup.js`, `extension/ui.css`, `extension/tests/ui.test.js`
- Modify: `extension/manifest.json` (`"action": {"default_title": "puppy", "default_popup": "popup.html"}`, `"options_ui": {"page": "options.html", "open_in_tab": false}`), `extension/tests/manifest.test.js` (assert both), `README.md` (new `## Browser downloads` before `## Project layout`; an `extension/` row in the layout table)

**Interfaces:**
- Consumes: Task 4's `loadSettings`, `saveSettings`, `normalizeExtensions`, `normalizeSites`, `hello`, `getStatus`, `setStatus`, `getRecent`, `armBypass`, `PROTOCOL`.
- Produces:
```js
// lib/ui.js
export function statusLine(status, extensionVersion)   // exact copy below
export function formToSettings({ enabled, minSizeMiB, extensions, sites })   // minSizeMiB text → bytes (Math.round(x * 1048576)); invalid/negative → 0
export function settingsToForm(settings)               // inverse: minSizeMiB as a string without trailing zeros ("1.5"), extensions joined ", ", sites joined "\n"
export function recentLines(recent)                    // [{text: "✓ f.iso", ok: true}, {text: "✗ g.zip — no", ok: false}]
```
- `statusLine` copy:
  - `null`/`undefined`: `Checking puppy…`
  - `ok`: `Ready: puppy ${puppyVersion}, daemon ${daemonVersion}, extension ${ext}`
  - `no_host`: `puppy is not set up for this browser. Run: puppy browser install`
  - `no_daemon`: `puppy is installed but its daemon does not answer: ${message}`
  - `mismatch`: `puppy ${puppyVersion} speaks protocol ${hostProtocol}, this extension ${PROTOCOL}: update one of them. Downloads stay in the browser.`
  - `error`: `Cannot reach puppy: ${message}`
- Options page:
  - fields: "Hand downloads to puppy" (checkbox), "Leave files smaller than (MiB) to the browser", "File types to leave to the browser" (textarea), "Sites to leave to the browser" (textarea, one per line)
  - saved on change via `saveSettings(formToSettings(...))`
  - the status line (`hello` on open, also `setStatus`)
  - the current shortcut from `commands.getAll()`, with the hint "Change it in chrome://extensions/shortcuts"
- Popup:
  - on/off toggle (writes `enabled`)
  - button "Let the browser take the next download" (`armBypass`, then the label turns to "The next download stays in the browser (60 s)")
  - status line (cached, then refreshed by `hello`)
  - recent list (`recentLines`), or "No downloads handed over yet"
- No inline scripts (MV3 CSP): each page loads its `*.js` as `type="module"`.

- [ ] **Step 1: Write the failing tests**

`ui.test.js`:
```js
test("status lines", …)            // each state's exact string above, ext "0.1.0"; null → "Checking puppy…"
test("form to settings", …)        // {enabled: true, minSizeMiB: "1.5", extensions: "zip, .ISO", sites: "example.com\n*.foo.org/path"}
                                   // → {enabled: true, minSizeBytes: 1572864, skipExtensions: ["zip","iso"], skipSites: ["example.com","foo.org"]}
                                   // minSizeMiB "" / "-3" / "abc" → 0
test("settings to form round-trips", …)  // formToSettings(settingsToForm(s)) deepEquals s for DEFAULTS and the example above
test("recent lines", …)            // ok and error entries as in Interfaces; [] → []
```
`manifest.test.js` additions: `action.default_popup === "popup.html"`, `options_ui.page === "options.html"`, and every `<script>` in both HTML files has a `src` and `type="module"` (no inline code).

- [ ] **Step 2: Run them to verify they fail**

Run: `node --test 'extension/tests/*.test.js'`
Expected: `ui.test.js` fails on module not found; the manifest additions fail.

- [ ] **Step 3: Implement `lib/ui.js`, the two pages and their DOM glue; update the manifest**

The pages use plain DOM APIs and `extensionApi()`. Keep `ui.css` small, using system fonts and `prefers-color-scheme` for dark mode.

- [ ] **Step 4: Write the README section**

`## Browser downloads` covers:
1. Install: `puppy browser install` (all found browsers; `--browser chrome|chromium|brave|edge|vivaldi` forces one), `puppy browser status`, `puppy browser uninstall`; re-run install after moving the puppy binary (status says "stale").
2. Load the extension: `chrome://extensions` → Developer mode → Load unpacked → the repo's `extension/` folder. The card must show the ID `<EXTENSION_ID>`.
3. What happens: every download goes to puppy's download folder, with cookies, referrer and user agent. The browser's copy is cancelled only after puppy accepted it, and the browser keeps it if puppy is not reachable within 10 s.
4. Bypass with the popup button or Alt+Shift+B (one download, 60 s). Settings live in the options page.
5. Privacy: a browser download's cookies are kept in puppy's private state file (0600) until it completes or is removed, never in logs, `info`/`--json` or the sidecar.
6. Limits: `blob:`/`data:`/`file:` downloads, incognito windows and other extensions' downloads stay in the browser. POST/form downloads cannot be detected and may fail in puppy: bypass them. "Ask where to save" is not mirrored. Flatpak/Snap browsers are not supported. Firefox is not supported yet.
7. Manual check (Chrome):
   1. `cargo build --release -p puppy && ./target/release/puppy browser install && ./target/release/puppy browser status` → chrome `ok`.
   2. Load unpacked; the ID matches; the options status reads "Ready: …".
   3. Download a large public file: it leaves the browser's list, and `puppy ls` shows it under the same name.
   4. Download something behind a login (e.g. a private GitHub release asset): it completes in puppy.
   5. Press Alt+Shift+B, then download: it stays in the browser.
   6. `puppy browser uninstall`, then download: it stays in the browser, and the popup lists it as failed with "not reachable".
   7. A `blob:` download (e.g. "export" in a web app) stays in the browser.
   8. Add `pdf` to the file types: a PDF stays in the browser.
   9. Midway through a logged-in download, `puppy stop` then `puppy resume <id>`: it resumes.

- [ ] **Step 5: Run full local verification**

Run: `node --test 'extension/tests/*.test.js' && cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all --check`
Expected: all PASS.

- [ ] **Step 6: Manual Chrome check (report the outcome, do not block on it)**

Follow README step 7 on Linux with Chrome or Chromium where available, and report each item's result.

- [ ] **Step 7: Commit**

```bash
git add extension README.md
git commit -m "feat(extension): options page, popup and README for browser downloads"
```
