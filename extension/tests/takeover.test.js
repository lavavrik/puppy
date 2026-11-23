import test from "node:test";
import assert from "node:assert/strict";
import { handleDownload, resumeStranded, FINISHED_FIRST } from "../lib/takeover.js";
import { armBypass, getRecent, getStatus } from "../lib/state.js";
import { HOST_NAME } from "../lib/native.js";
import { fakeApi, item, until } from "./fake-chrome.js";

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

/** An api whose browser knows `items` (default: the one standard item). */
function setup(over = {}, items = [item()]) {
  return fakeApi({ downloads: items, cookies: [{ name: "sid", value: "1", path: "/" }], ...over });
}

/** Starts a takeover; `releases()` counts the release calls for it. */
function start(api, it = item(), opts = {}) {
  let n = 0;
  const release = () => {
    n++;
    api.calls.push(["release", it.id]);
  };
  const outcome = handleDownload(api, it, release, { userAgent: "UA", ownId: "ownid", ...opts });
  return { outcome, releases: () => n };
}

async function run(api, it = item(), opts = {}) {
  const r = start(api, it, opts);
  const outcome = await r.outcome;
  assert.equal(r.releases(), 1, "release runs exactly once");
  return outcome;
}

const names = (api) => api.calls.map((c) => c[0]);
const sends = (api) => api.calls.filter((c) => c[0] === "send").map((c) => c[2]);
const has = (api, name, arg) => api.calls.some((c) => c[0] === name && (arg === undefined || c[1] === arg));
const at = (api, name) => names(api).indexOf(name);

test("accepted download is paused, sent, then cancelled and erased", async () => {
  const api = setup();
  const outcome = await run(api);
  assert.deepEqual(outcome, { kind: "taken", downloadId: 3 });
  // The target stays undetermined (no suggest) until puppy has it and the browser
  // download is cancelled.
  assert.ok(at(api, "pause") < at(api, "send"));
  assert.ok(at(api, "send") < at(api, "cancel"));
  assert.ok(at(api, "cancel") < at(api, "erase"));
  assert.ok(at(api, "erase") < at(api, "release"));
  assert.ok(has(api, "pause", 7) && has(api, "cancel", 7));
  assert.deepEqual(api.calls.find((c) => c[0] === "erase")[1], { id: 7 });
  assert.ok(!has(api, "resume"));
  const [send] = api.calls.filter((c) => c[0] === "send");
  assert.equal(send[1], HOST_NAME);
  const msg = send[2];
  assert.match(msg.id, UUID);
  assert.deepEqual(msg, {
    id: msg.id,
    protocol: 1,
    type: "add",
    url: "https://dl.example.com/f.iso",
    final_url: "https://cdn.example.net/f.iso",
    filename: "f.iso",
    referrer: "https://example.com/p",
    cookies: "sid=1",
    user_agent: "UA",
  });
  assert.ok(has(api, "cookies", "https://dl.example.com/f.iso"));
  const [recent] = await getRecent(api);
  assert.equal(recent.name, "f.iso");
  assert.equal(recent.ok, true);
});

test("recent name never falls back to the raw url", async () => {
  const url = "https://us:pw@host.example/?token=secret";
  const it = item({ url, filename: "" });
  const api = setup({}, [it]);
  await run(api, it);
  const [recent] = await getRecent(api);
  assert.equal(recent.name, "host.example");
});

test("error reply resumes the browser download", async () => {
  const api = setup({ reply: (m) => ({ id: m.id, type: "error", code: "refused", message: "no" }) });
  const outcome = await run(api);
  assert.equal(outcome.kind, "resumed");
  assert.ok(has(api, "resume", 7));
  assert.ok(at(api, "resume") < at(api, "release"));
  assert.ok(!has(api, "cancel") && !has(api, "erase"));
  const [recent] = await getRecent(api);
  assert.equal(recent.ok, false);
  assert.equal(recent.detail, "no");
  assert.equal(await getStatus(api), undefined, "a refusal does not change the status");
});

test("missing host resumes the browser download", async () => {
  const api = setup({ reply: () => new Error("Specified native messaging host not found.") });
  const outcome = await run(api);
  assert.equal(outcome.kind, "resumed");
  assert.ok(has(api, "resume", 7));
  assert.ok(!has(api, "cancel"));
  const [recent] = await getRecent(api);
  assert.equal(recent.ok, false);
  assert.ok(recent.detail.startsWith("puppy is not reachable"), recent.detail);
  assert.ok(recent.detail.includes("not found"));
});

test("reply with another id is treated as an error", async () => {
  const api = setup({ reply: () => ({ id: "someone-else", type: "accepted", download_id: 3 }) });
  const outcome = await run(api);
  assert.equal(outcome.kind, "resumed");
  assert.ok(has(api, "resume", 7));
  assert.ok(!has(api, "cancel") && !has(api, "erase"));
});

test("an error the host could not tie to a request is still ours", async () => {
  const api = setup({ reply: () => ({ id: null, type: "error", code: "bad_message", message: "unreadable message: x" }) });
  assert.equal((await run(api)).kind, "resumed");
  assert.equal((await getRecent(api))[0].detail, "unreadable message: x");
  const other = setup({ reply: () => ({ id: null, type: "accepted", download_id: 3 }) });
  assert.equal((await run(other)).kind, "resumed");
  assert.ok(!has(other, "cancel"));
});

test("an unexpected reply type resumes", async () => {
  const api = setup({ reply: (m) => ({ id: m.id, type: "removed" }) });
  assert.equal((await run(api)).kind, "resumed");
  assert.ok(!has(api, "cancel"));
});

test("no reply in 10 s resumes; a late accept is removed in puppy", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout", "Date"] });
  let answer;
  const pending = new Promise((r) => (answer = r));
  const api = setup({ reply: (m) => (m.type === "add" ? pending.then((v) => ({ id: m.id, ...v })) : { id: m.id, type: "removed" }) });
  const r = start(api);
  await until(() => has(api, "send"));
  t.mock.timers.tick(10_000);
  const outcome = await r.outcome;
  assert.equal(outcome.kind, "resumed");
  assert.equal(r.releases(), 1);
  assert.ok(has(api, "resume", 7));
  const [recent] = await getRecent(api);
  assert.equal(recent.detail, "puppy did not answer in 10 s");
  answer({ type: "accepted", download_id: 9 });
  await until(() => sends(api).length === 2);
  const [add, remove] = sends(api);
  assert.equal(remove.type, "remove");
  assert.equal(remove.download_id, 9);
  assert.equal(remove.protocol, 1);
  assert.equal(remove.delete_files, true);
  assert.match(remove.id, UUID);
  assert.notEqual(remove.id, add.id);
  assert.ok(!has(api, "cancel") && !has(api, "erase"));
});

test("a late remove that fails is swallowed", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout", "Date"] });
  let answer;
  const pending = new Promise((r) => (answer = r));
  const api = setup({ reply: (m) => (m.type === "add" ? pending.then((v) => ({ id: m.id, ...v })) : new Error("gone")) });
  const r = start(api);
  await until(() => has(api, "send"));
  t.mock.timers.tick(10_000);
  await r.outcome;
  answer({ type: "accepted", download_id: 9 });
  await until(() => sends(api).length === 2);
});

test("reply at 9999 ms is still in time", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout", "Date"] });
  let answer;
  const pending = new Promise((r) => (answer = r));
  const api = setup({ reply: (m) => pending.then(() => ({ id: m.id, type: "accepted", download_id: 4 })) });
  const r = start(api);
  await until(() => has(api, "send"));
  t.mock.timers.tick(9_999);
  answer();
  assert.deepEqual(await r.outcome, { kind: "taken", downloadId: 4 });
  t.mock.timers.tick(1);
  assert.ok(!has(api, "resume"));
});

/** Makes the browser's cancel fail, leaving the item in `state` (or gone). */
function failCancel(api, state) {
  api.downloads.cancel = async (id) => {
    api.calls.push(["cancel", id]);
    if (state === "gone") api.items.length = 0;
    else api.items[0].state = state;
    throw new Error("Download must be in progress");
  };
}

test("a download the browser finished first is removed from puppy with its files", async () => {
  const api = setup({ reply: (m) => (m.type === "add" ? { id: m.id, type: "accepted", download_id: 5 } : { id: m.id, type: "removed" }) });
  failCancel(api, "complete");
  assert.deepEqual(await run(api), { kind: "left", reason: FINISHED_FIRST });
  const remove = sends(api)[1];
  assert.equal(remove.type, "remove");
  assert.equal(remove.download_id, 5);
  assert.equal(remove.delete_files, true);
  assert.notEqual(remove.id, sends(api)[0].id);
  assert.ok(!has(api, "erase") && !has(api, "resume"));
  const [recent] = await getRecent(api);
  assert.equal(recent.ok, false);
  assert.equal(recent.detail, FINISHED_FIRST);
});

test("a failed cancel of a download gone, interrupted or cancelled keeps it taken", async () => {
  for (const state of ["gone", "interrupted"]) {
    const api = setup();
    failCancel(api, state);
    assert.deepEqual(await run(api), { kind: "taken", downloadId: 3 }, state);
    assert.equal(sends(api).length, 1, state);
  }
});

test("a failed erase is warned about without the URL or cookies", async (t) => {
  const warn = t.mock.method(console, "warn", () => {});
  const api = setup();
  api.downloads.erase = async () => {
    throw new Error("erase broke");
  };
  assert.deepEqual(await run(api), { kind: "taken", downloadId: 3 });
  assert.equal(warn.mock.callCount(), 1);
  const text = warn.mock.calls[0].arguments.join(" ");
  assert.ok(text.includes("erase broke"), text);
  assert.ok(!text.includes("example") && !text.includes("sid"), text);
});

test("paused downloads are remembered until the takeover is done", async () => {
  let answer;
  const api = setup({ reply: (m) => new Promise((r) => (answer = () => r({ id: m.id, type: "accepted", download_id: 3 }))) });
  const r = start(api);
  await until(() => has(api, "send"));
  assert.deepEqual(api.storage.session.data.paused, [7]);
  answer();
  await r.outcome;
  assert.deepEqual(api.storage.session.data.paused, []);
  const left = setup({ settings: { enabled: false } });
  await run(left);
  assert.equal(left.storage.session.data.paused, undefined, "never paused, never remembered");
});

test("stranded paused downloads are resumed", async () => {
  const api = fakeApi({
    session: { paused: [5, 6, 7] },
    downloads: [
      item({ id: 5, paused: true }),
      item({ id: 6, state: "complete", paused: false }),
      item({ id: 7, paused: false }),
    ],
  });
  await resumeStranded(api);
  assert.deepEqual(api.calls.filter((c) => c[0] === "resume"), [["resume", 5]]);
  assert.equal(api.storage.session.data.paused, undefined);
  await resumeStranded(api);
  assert.equal(api.calls.filter((c) => c[0] === "resume").length, 1);
});

test("protocol mismatch resumes and caches it; the next download is left unpaused", async () => {
  const api = setup(
    { reply: (m) => ({ id: m.id, type: "error", code: "protocol_mismatch", message: "speaks 2" }) },
    [item(), item({ id: 8 })],
  );
  assert.equal((await run(api)).kind, "resumed");
  assert.ok(has(api, "resume", 7));
  assert.equal((await getStatus(api)).state, "mismatch");
  const before = api.calls.length;
  const next = await run(api, item({ id: 8 }));
  assert.equal(next.kind, "left");
  assert.deepEqual(names({ calls: api.calls.slice(before) }), ["release"]);
});

test("unsupported_downloads_are_never_paused", async () => {
  for (const over of [
    { url: "blob:https://x.com/1" },
    { url: "data:text/plain,hi" },
    { url: "file:///tmp/a" },
    { incognito: true },
    { byExtensionId: "someextension" },
  ]) {
    const it = item(over);
    const api = setup({}, [it]);
    const outcome = await run(api, it);
    assert.equal(outcome.kind, "left", JSON.stringify(over));
    assert.deepEqual(names(api), ["release"], JSON.stringify(over));
  }
});

test("completed_before_pause_is_left", async () => {
  const api = setup({}, [item({ state: "complete" })]);
  const outcome = await run(api);
  assert.equal(outcome.kind, "left");
  assert.ok(has(api, "pause", 7));
  assert.ok(!has(api, "send") && !has(api, "resume") && !has(api, "cancel"));
});

test("a download gone before the search is left", async () => {
  const api = setup({}, []);
  assert.equal((await run(api)).kind, "left");
  assert.ok(!has(api, "send"));
});

test("pause failure leaves the download alone", async () => {
  const api = setup({ failPause: true });
  const outcome = await run(api);
  assert.equal(outcome.kind, "left");
  assert.ok(!has(api, "send") && !has(api, "resume") && !has(api, "cookies"));
});

test("cookie read failure resumes", async () => {
  const api = setup({ failCookies: true });
  const outcome = await run(api);
  assert.equal(outcome.kind, "resumed");
  assert.ok(has(api, "resume", 7));
  assert.ok(!has(api, "send"));
});

test("no cookies sends none", async () => {
  const api = setup({ cookies: [] });
  await run(api);
  assert.equal(sends(api)[0].cookies, null);
});

test("bypass skips exactly one download within 60 s", async () => {
  const t0 = 1_000_000;
  const api = setup({}, [item(), item({ id: 8 })]);
  await armBypass(api, t0);
  const first = await run(api, item(), { now: () => t0 + 59_000 });
  assert.deepEqual(first, { kind: "left", reason: "bypass" });
  assert.ok(!has(api, "pause"));
  const second = await run(api, item({ id: 8 }), { now: () => t0 + 59_500 });
  assert.equal(second.kind, "taken");
  const badges = api.calls.filter((c) => c[0] === "badge").map((c) => c[1]);
  assert.deepEqual(badges, ["1", ""]);

  const late = setup();
  await armBypass(late, t0);
  assert.equal((await run(late, item(), { now: () => t0 + 61_000 })).kind, "taken");
  assert.deepEqual(late.calls.filter((c) => c[0] === "badge").map((c) => c[1]), ["1", ""]);
});

test("bypass is not used up by a download the browser keeps anyway", async () => {
  const blob = item({ id: 6, url: "blob:https://x.com/1" });
  const api = setup({}, [blob, item()]);
  await armBypass(api, 0);
  assert.equal((await run(api, blob, { now: () => 1 })).reason, "not http(s)");
  assert.deepEqual(await run(api, item(), { now: () => 2 }), { kind: "left", reason: "bypass" });
});

test("excluded downloads do not use up the bypass", async () => {
  const api = setup({ settings: { skipExtensions: ["zip"] } }, [item({ filename: "a.zip" }), item()]);
  await armBypass(api, 0);
  assert.equal((await run(api, item({ filename: "a.zip" }), { now: () => 1 })).kind, "left");
  assert.deepEqual(await run(api, item(), { now: () => 2 }), { kind: "left", reason: "bypass" });
});

test("disabled extension leaves everything", async () => {
  const api = setup({ settings: { enabled: false } });
  const outcome = await run(api);
  assert.equal(outcome.kind, "left");
  assert.deepEqual(names(api), ["release"]);
});

test("concurrent_downloads_keep_their_replies", async () => {
  const waiting = new Map();
  const api = setup(
    {
      reply: (m) =>
        new Promise((resolve) => {
          waiting.set(m.id, resolve);
        }),
    },
    [item(), item({ id: 8, url: "https://dl.example.com/g.iso", filename: "g.iso" })],
  );
  const a = start(api, item());
  const b = start(api, item({ id: 8, url: "https://dl.example.com/g.iso", filename: "g.iso" }));
  await until(() => waiting.size === 2);
  const byUrl = Object.fromEntries(sends(api).map((m) => [m.url, m.id]));
  const id7 = byUrl["https://dl.example.com/f.iso"];
  const id8 = byUrl["https://dl.example.com/g.iso"];
  waiting.get(id8)({ id: id8, type: "accepted", download_id: 11 });
  waiting.get(id7)({ id: id7, type: "error", code: "refused", message: "nope" });
  assert.equal((await a.outcome).kind, "resumed");
  assert.deepEqual(await b.outcome, { kind: "taken", downloadId: 11 });
  assert.equal(a.releases(), 1);
  assert.equal(b.releases(), 1);
  const acted = api.calls.filter((c) => ["cancel", "resume"].includes(c[0])).map((c) => c.join(":"));
  assert.deepEqual(acted.sort(), ["cancel:8", "resume:7"]);
  const recent = await getRecent(api);
  assert.equal(recent.length, 2, "both handovers are recorded");
});

test("release is called exactly once on every path", async (t) => {
  const paths = [
    () => setup(),
    () => setup({ reply: (m) => ({ id: m.id, type: "error", code: "refused", message: "no" }) }),
    () => setup({ reply: () => new Error("boom") }),
    () => setup({ failPause: true }),
    () => setup({ failCookies: true }),
    () => setup({ settings: { enabled: false } }),
    () => setup({}, [item({ state: "complete" })]),
    () => setup({ session: { status: { state: "mismatch" } } }),
  ];
  for (const make of paths) await run(make());
  await run(setup(), item({ url: "data:," }));
  // A throwing API (an unexpected failure) still releases once.
  const broken = setup();
  broken.storage.sync.get = async () => {
    throw new Error("storage broke");
  };
  const r = start(broken);
  await assert.rejects(r.outcome);
  assert.equal(r.releases(), 1);
  // The timeout path.
  t.mock.timers.enable({ apis: ["setTimeout", "Date"] });
  const slow = setup({ reply: () => new Promise(() => {}) });
  const s = start(slow);
  await until(() => has(slow, "send"));
  t.mock.timers.tick(10_000);
  await s.outcome;
  assert.equal(s.releases(), 1);
});

test("recent keeps the newest five", async () => {
  const items = [1, 2, 3, 4, 5, 6].map((id) => item({ id, filename: `f${id}.iso` }));
  const api = setup({}, items);
  for (const it of items) await run(api, it);
  const recent = await getRecent(api);
  assert.deepEqual(recent.map((r) => r.name), ["f6.iso", "f5.iso", "f4.iso", "f3.iso", "f2.iso"]);
});
