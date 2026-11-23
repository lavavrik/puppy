import test from "node:test";
import assert from "node:assert/strict";
import { fakeApi, item, until, flush } from "./fake-chrome.js";
import { onNewDownload, extensionApi } from "../lib/compat.js";
import { hello } from "../lib/native.js";

const api = fakeApi({ downloads: [item()] });
globalThis.chrome = api;
await import("../background.js");

test("background registers its listeners", async () => {
  assert.equal(api.downloads.onDeterminingFilename.listeners.length, 1);
  assert.equal(api.downloads.onCreated.listeners.length, 0);
  assert.equal(api.commands.onCommand.listeners.length, 1);
  assert.equal(api.runtime.onStartup.listeners.length, 1);
  assert.equal(api.runtime.onInstalled.listeners.length, 1);
  await Promise.all(api.commands.onCommand.fire("bypass-next"));
  assert.ok(api.storage.session.data.bypassUntil > Date.now());
  await api.storage.session.remove("bypassUntil");
  await Promise.all(api.commands.onCommand.fire("something-else"));
  assert.equal(api.storage.session.data.bypassUntil, undefined);
});

test("startup and install record the hello status", async () => {
  const before = api.calls.length;
  await Promise.all(api.runtime.onStartup.fire());
  // The default fake reply is not a hello reply; the status records an error.
  const msg = api.calls.slice(before).find((c) => c[0] === "send")[2];
  assert.equal(msg.type, "hello");
  assert.equal(msg.extension_version, "0.1.0");
  assert.equal(msg.protocol, 1);
  assert.equal(api.storage.session.data.status.state, "error");
});

test("determining-filename listener returns true and suggests after the pause", async () => {
  const before = api.calls.length;
  const suggest = () => api.calls.push(["suggest"]);
  const [ret] = api.downloads.onDeterminingFilename.fire(item(), suggest);
  assert.equal(ret, true);
  await until(() => api.calls.slice(before).some((c) => c[0] === "erase"));
  const names = api.calls.slice(before).map((c) => c[0]);
  assert.equal(names.filter((n) => n === "suggest").length, 1);
  assert.ok(names.indexOf("pause") < names.indexOf("suggest"));
});

test("Firefox falls back to onCreated", async () => {
  const ff = fakeApi({ firefox: true });
  const seen = [];
  onNewDownload(ff, (it, release) => {
    seen.push([it, release]);
    return Promise.resolve();
  });
  assert.equal(ff.downloads.onCreated.listeners.length, 1);
  const it = item();
  ff.downloads.onCreated.fire(it);
  await flush();
  assert.equal(seen.length, 1);
  assert.equal(seen[0][0], it);
  assert.equal(seen[0][1](), undefined);
});

test("a failing handler still suggests exactly once", async (t) => {
  const logged = t.mock.method(console, "error", () => {});
  const ch = fakeApi();
  let suggested = 0;
  onNewDownload(ch, async (_it, release) => {
    release();
    throw new Error("boom");
  });
  ch.downloads.onDeterminingFilename.fire(item(), () => suggested++);
  await flush();
  await flush();
  assert.equal(suggested, 1);
  assert.equal(logged.mock.callCount(), 1);
});

test("extensionApi prefers browser over chrome", () => {
  assert.equal(extensionApi({ browser: 1, chrome: 2 }), 1);
  assert.equal(extensionApi({ chrome: 2 }), 2);
});

test("hello statuses", async () => {
  const ok = await hello(
    fakeApi({ reply: (m) => ({ id: m.id, type: "hello", protocol: 1, puppy_version: "0.3.0", daemon_version: "0.3.0", daemon_error: null }) }),
    "0.1.0",
  );
  assert.equal(ok.state, "ok");
  assert.equal(ok.puppyVersion, "0.3.0");
  assert.equal(ok.daemonVersion, "0.3.0");
  assert.equal(ok.hostProtocol, 1);
  assert.equal(typeof ok.at, "number");

  const noDaemon = await hello(
    fakeApi({ reply: (m) => ({ id: m.id, type: "hello", protocol: 1, puppy_version: "0.3.0", daemon_version: null, daemon_error: "connection refused" }) }),
    "0.1.0",
  );
  assert.equal(noDaemon.state, "no_daemon");
  assert.equal(noDaemon.message, "connection refused");

  const mismatch = await hello(
    fakeApi({ reply: (m) => ({ id: m.id, type: "hello", protocol: 2, puppy_version: "9", daemon_version: "9", daemon_error: null }) }),
    "0.1.0",
  );
  assert.equal(mismatch.state, "mismatch");
  assert.equal(mismatch.hostProtocol, 2);

  for (const text of ["Specified native messaging host not found.", "Access to the specified native messaging host is forbidden."]) {
    assert.equal((await hello(fakeApi({ reply: () => new Error(text) }), "0.1.0")).state, "no_host");
  }
  const other = await hello(fakeApi({ reply: () => new Error("Native host has exited.") }), "0.1.0");
  assert.equal(other.state, "error");
  assert.ok(other.message.includes("exited"));
});

test("hello times out after 10 s", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout", "Date"] });
  const a = fakeApi({ reply: () => new Promise(() => {}) });
  const p = hello(a, "0.1.0");
  await until(() => a.calls.some((c) => c[0] === "send"));
  t.mock.timers.tick(10_000);
  const s = await p;
  assert.equal(s.state, "error");
  assert.equal(s.message, "puppy did not answer in 10 s");
});
