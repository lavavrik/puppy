import test from "node:test";
import assert from "node:assert/strict";
import { statusLine, formToSettings, settingsToForm, recentLines } from "../lib/ui.js";
import { DEFAULTS } from "../lib/settings.js";

test("status lines", () => {
  assert.equal(statusLine(null, "0.1.0"), "Checking puppy…");
  assert.equal(statusLine(undefined, "0.1.0"), "Checking puppy…");
  assert.equal(
    statusLine({ state: "ok", puppyVersion: "0.2.0", daemonVersion: "0.2.1" }, "0.1.0"),
    "Ready: puppy 0.2.0, daemon 0.2.1, extension 0.1.0",
  );
  assert.equal(
    statusLine({ state: "no_host" }, "0.1.0"),
    "puppy is not set up for this browser. Run: puppy browser install",
  );
  assert.equal(
    statusLine({ state: "no_daemon", message: "socket gone" }, "0.1.0"),
    "puppy is installed but its daemon does not answer: socket gone",
  );
  assert.equal(
    statusLine({ state: "mismatch", puppyVersion: "9.0.0", hostProtocol: 2 }, "0.1.0"),
    "puppy 9.0.0 speaks protocol 2, this extension 1: update one of them. Downloads stay in the browser.",
  );
  assert.equal(statusLine({ state: "error", message: "boom" }, "0.1.0"), "Cannot reach puppy: boom");
});

const example = { enabled: true, minSizeBytes: 1572864, skipExtensions: ["zip", "iso"], skipSites: ["example.com", "foo.org"] };

test("form to settings", () => {
  assert.deepEqual(
    formToSettings({ enabled: true, minSizeMiB: "1.5", extensions: "zip, .ISO", sites: "example.com\n*.foo.org/path" }),
    example,
  );
  for (const minSizeMiB of ["", "-3", "abc"]) {
    assert.equal(formToSettings({ enabled: false, minSizeMiB, extensions: "", sites: "" }).minSizeBytes, 0, minSizeMiB);
  }
  assert.equal(formToSettings({ enabled: false, minSizeMiB: "", extensions: "", sites: "" }).enabled, false);
});

test("settings to form round-trips", () => {
  assert.deepEqual(settingsToForm(example), { enabled: true, minSizeMiB: "1.5", extensions: "zip, iso", sites: "example.com\nfoo.org" });
  assert.equal(settingsToForm({ ...DEFAULTS, minSizeBytes: 2097152 }).minSizeMiB, "2");
  for (const s of [{ ...DEFAULTS }, example]) assert.deepEqual(formToSettings(settingsToForm(s)), s);
});

test("recent lines", () => {
  assert.deepEqual(
    recentLines([
      { name: "f.iso", ok: true, detail: "id 3" },
      { name: "g.zip", ok: false, detail: "no" },
    ]),
    [
      { text: "✓ f.iso", ok: true },
      { text: "✗ g.zip — no", ok: false },
    ],
  );
  assert.deepEqual(recentLines([{ name: "h", ok: false }]), [{ text: "✗ h", ok: false }]);
  assert.deepEqual(recentLines([]), []);
  assert.deepEqual(recentLines(undefined), []);
});
