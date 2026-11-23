import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createHash } from "node:crypto";

const here = (p) => new URL(p, import.meta.url);
const manifest = JSON.parse(readFileSync(here("../manifest.json"), "utf8"));

test("permissions are exactly the four, plus <all_urls>, no content scripts", () => {
  assert.equal(manifest.manifest_version, 3);
  assert.deepEqual([...manifest.permissions].sort(), ["cookies", "downloads", "nativeMessaging", "storage"]);
  assert.deepEqual(manifest.host_permissions, ["<all_urls>"]);
  assert.equal(manifest.content_scripts, undefined);
  assert.equal(manifest.optional_permissions, undefined);
  assert.ok(!JSON.stringify(manifest).includes("webRequest"));
  assert.deepEqual(manifest.background, { service_worker: "background.js", type: "module" });
});

test("bypass command defaults to Alt+Shift+B", () => {
  assert.equal(manifest.commands["bypass-next"].suggested_key.default, "Alt+Shift+B");
  assert.equal(manifest.commands["bypass-next"].description, "Let the browser take the next download");
});

test("key derives the extension id puppy installs", () => {
  const hex = createHash("sha256").update(Buffer.from(manifest.key, "base64")).digest("hex").slice(0, 32);
  const id = [...hex].map((c) => String.fromCharCode(97 + parseInt(c, 16))).join("");
  const rust = readFileSync(here("../../crates/puppy/src/browser/mod.rs"), "utf8");
  const want = rust.match(/EXTENSION_ID: &str = "([a-p]{32})"/)[1];
  assert.equal(id, want);
});
