import { extensionApi } from "./lib/compat.js";
import { loadSettings, saveSettings } from "./lib/settings.js";
import { hello } from "./lib/native.js";
import { getStatus, setStatus } from "./lib/state.js";
import { statusLine, formToSettings, settingsToForm, mergeForm } from "./lib/ui.js";

const api = extensionApi();
const $ = (id) => document.getElementById(id);
const version = api.runtime.getManifest().version;
const showStatus = (status) => ($("status").textContent = statusLine(status, version));

function readForm() {
  return formToSettings({
    enabled: $("enabled").checked,
    minSizeMiB: $("minSizeMiB").value,
    extensions: $("extensions").value,
    sites: $("sites").value,
  });
}

async function init() {
  const form = settingsToForm(await loadSettings(api));
  $("enabled").checked = form.enabled;
  $("minSizeMiB").value = form.minSizeMiB;
  $("extensions").value = form.extensions;
  $("sites").value = form.sites;
  const FIELDS = {
    enabled: ["enabled"],
    minSizeMiB: ["minSizeBytes"],
    extensions: ["skipExtensions"],
    sites: ["skipSites"],
  };
  // Only the edited field is written over freshly loaded settings, so a toggle in the popup survives.
  const save = async (id) => saveSettings(api, mergeForm(await loadSettings(api), readForm(), FIELDS[id]));
  const timers = {};
  for (const id of Object.keys(FIELDS)) {
    $(id).addEventListener("change", () => {
      clearTimeout(timers[id]);
      save(id);
    });
    if (id !== "enabled") {
      $(id).addEventListener("input", () => {
        clearTimeout(timers[id]);
        timers[id] = setTimeout(() => save(id), 300);
      });
    }
  }

  showStatus(await getStatus(api));
  const status = await hello(api, version);
  await setStatus(api, status);
  showStatus(status);

  const commands = await api.commands.getAll();
  $("shortcut").textContent = commands.find((c) => c.name === "bypass-next")?.shortcut || "not set";
}

init().catch((e) => ($("status").textContent = `Cannot reach puppy: ${e?.message ?? e}`));
