import { extensionApi } from "./lib/compat.js";
import { loadSettings, saveSettings } from "./lib/settings.js";
import { hello } from "./lib/native.js";
import { armBypass, getRecent, getStatus, setStatus } from "./lib/state.js";
import { statusLine, recentLines } from "./lib/ui.js";

const api = extensionApi();
const $ = (id) => document.getElementById(id);
const version = api.runtime.getManifest().version;
const showStatus = (status) => ($("status").textContent = statusLine(status, version));

function showRecent(recent) {
  const list = $("recent");
  list.replaceChildren();
  const lines = recentLines(recent);
  if (lines.length === 0) lines.push({ text: "No downloads handed over yet", empty: true });
  for (const line of lines) {
    const li = document.createElement("li");
    li.textContent = line.text;
    li.className = line.empty ? "empty" : line.ok ? "ok" : "bad";
    list.append(li);
  }
}

async function init() {
  $("enabled").checked = (await loadSettings(api)).enabled;
  $("enabled").addEventListener("change", async () => {
    await saveSettings(api, { ...(await loadSettings(api)), enabled: $("enabled").checked });
  });

  $("bypass").addEventListener("click", async () => {
    await armBypass(api, Date.now());
    $("bypass").textContent = "The next download stays in the browser (60 s)";
    $("bypass").disabled = true;
  });

  showRecent(await getRecent(api));
  showStatus(await getStatus(api));
  const status = await hello(api, version);
  await setStatus(api, status);
  showStatus(status);
}

init().catch((e) => ($("status").textContent = `Cannot reach puppy: ${e?.message ?? e}`));
