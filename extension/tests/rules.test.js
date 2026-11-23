import test from "node:test";
import assert from "node:assert/strict";
import { unsupportedReason, excludedReason, siteMatches, fileExtension, basename } from "../lib/rules.js";
import { DEFAULTS, loadSettings, saveSettings, normalizeExtensions, normalizeSites } from "../lib/settings.js";
import { cookieHeader, addMessage } from "../lib/request.js";
import { fakeApi, item } from "./fake-chrome.js";

test("unsupported schemes and contexts", () => {
  for (const url of ["blob:https://x.com/1-2", "data:text/plain,hi", "file:///tmp/a", "ftp://x.com/a", "filesystem:https://x.com/t/a"]) {
    assert.ok(unsupportedReason(item({ url }), "ownid"), url);
  }
  assert.equal(unsupportedReason(item({ url: "not a url" }), "ownid"), "not http(s)");
  assert.equal(unsupportedReason(item(), "ownid"), null);
  assert.equal(unsupportedReason(item({ url: "http://x.com/a" }), "ownid"), null);
  assert.ok(unsupportedReason(item({ incognito: true }), "ownid"));
  assert.ok(unsupportedReason(item({ byExtensionId: "other" }), "ownid"));
  assert.equal(unsupportedReason(item({ byExtensionId: "ownid" }), "ownid"), null);
  assert.ok(unsupportedReason(item({ state: "complete" }), "ownid"));
});

test("size, type and site exclusions", () => {
  const s = (over) => ({ ...DEFAULTS, ...over });
  assert.equal(excludedReason(item(), s({})), null);
  assert.ok(excludedReason(item(), s({ enabled: false })));
  assert.ok(excludedReason(item({ totalBytes: 1000 }), s({ minSizeBytes: 2048 })));
  assert.equal(excludedReason(item({ totalBytes: 4096 }), s({ minSizeBytes: 2048 })), null);
  assert.equal(excludedReason(item({ totalBytes: -1 }), s({ minSizeBytes: 2048 })), null);
  assert.equal(excludedReason(item({ totalBytes: 0 }), s({ minSizeBytes: 2048 })), null);
  assert.ok(excludedReason(item({ filename: "F.ISO" }), s({ skipExtensions: ["iso"] })));
  assert.ok(excludedReason(item({ filename: "C:\\dl\\F.ISO" }), s({ skipExtensions: ["iso"] })));
  assert.equal(excludedReason(item({ filename: "f.zip" }), s({ skipExtensions: ["iso"] })), null);
  // No filename yet (onCreated): the URL's last path segment decides.
  assert.ok(excludedReason(item({ filename: "" }), s({ skipExtensions: ["iso"] })));
  assert.equal(fileExtension("a/b.tar.gz"), "gz");
  assert.equal(fileExtension("dir.d\\noext"), "");
  assert.equal(fileExtension(".bashrc"), "");
  assert.equal(basename("C:\\Users\\me\\a.zip"), "a.zip");
  assert.equal(basename("dir/a.zip"), "a.zip");
});

test("site matching", () => {
  assert.ok(siteMatches("example.com", "example.com"));
  assert.ok(siteMatches("dl.example.com", "example.com"));
  assert.ok(siteMatches("DL.Example.com", "example.com"));
  assert.ok(!siteMatches("badexample.com", "example.com"));
  assert.ok(!siteMatches("example.com", ""));
  const s = { ...DEFAULTS, skipSites: ["example.com"] };
  const none = { url: "https://a.net/f", finalUrl: "https://b.net/f", referrer: "https://c.net/" };
  assert.equal(excludedReason(item(none), s), null);
  assert.ok(excludedReason(item({ ...none, url: "https://dl.example.com/f" }), s));
  assert.ok(excludedReason(item({ ...none, finalUrl: "https://example.com/f" }), s));
  assert.ok(excludedReason(item({ ...none, referrer: "https://www.example.com/p" }), s));
  assert.equal(excludedReason(item({ ...none, referrer: "", finalUrl: undefined }), s), null);
});

test("normalizers", () => {
  assert.deepEqual(normalizeExtensions("zip, .ISO  pdf"), ["zip", "iso", "pdf"]);
  assert.deepEqual(normalizeExtensions(["zip", "ZIP", ".zip"]), ["zip"]);
  assert.deepEqual(normalizeExtensions(""), []);
  assert.deepEqual(normalizeSites("Example.com\n*.foo.org/path, https://bar.net"), ["example.com", "foo.org", "bar.net"]);
  assert.deepEqual(normalizeSites("example.com example.com:8080"), ["example.com"]);
  assert.deepEqual(normalizeSites(""), []);
  assert.deepEqual(normalizeSites("  \n "), []);
});

test("loadSettings merges defaults over partial stored settings", async () => {
  assert.deepEqual(await loadSettings(fakeApi({ settings: { minSizeBytes: 5 } })), { ...DEFAULTS, minSizeBytes: 5 });
  assert.deepEqual(await loadSettings(fakeApi()), DEFAULTS);
  const api = fakeApi();
  await saveSettings(api, { ...DEFAULTS, enabled: false });
  assert.equal((await loadSettings(api)).enabled, false);
});

test("cookieHeader", () => {
  assert.equal(
    cookieHeader([
      { name: "a", value: "1", path: "/" },
      { name: "b", value: "2", path: "/x" },
    ]),
    "b=2; a=1",
  );
  assert.equal(
    cookieHeader([
      { name: "a", value: "1", path: "/" },
      { name: "c", value: "3", path: "/" },
    ]),
    "a=1; c=3",
  );
  assert.equal(cookieHeader([]), null);
});

test("addMessage carries what puppy needs and nulls for the rest", () => {
  assert.deepEqual(addMessage(item({ filename: "/home/me/Downloads/f.iso" }), "sid=1", "UA", "rid"), {
    id: "rid",
    protocol: 1,
    type: "add",
    url: "https://dl.example.com/f.iso",
    final_url: "https://cdn.example.net/f.iso",
    filename: "f.iso",
    referrer: "https://example.com/p",
    cookies: "sid=1",
    user_agent: "UA",
  });
  const bare = addMessage(item({ filename: "", referrer: "", finalUrl: undefined }), null, "", "rid");
  assert.equal(bare.filename, null);
  assert.equal(bare.referrer, null);
  assert.equal(bare.final_url, null);
  assert.equal(bare.cookies, null);
  assert.equal(bare.user_agent, null);
});
