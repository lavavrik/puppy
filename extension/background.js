// The service worker: hands new downloads to puppy, arms the bypass from the
// keyboard shortcut and caches the handshake status.

import { extensionApi, onNewDownload } from "./lib/compat.js";
import { handleDownload } from "./lib/takeover.js";
import { hello } from "./lib/native.js";
import { armBypass, setStatus } from "./lib/state.js";

const api = extensionApi();

onNewDownload(api, (item, release) => handleDownload(api, item, release, { ownId: api.runtime.id }));

api.commands.onCommand.addListener((command) => {
  if (command === "bypass-next") return armBypass(api, Date.now());
});

const refreshStatus = async () => setStatus(api, await hello(api, api.runtime.getManifest().version));
api.runtime.onStartup.addListener(refreshStatus);
api.runtime.onInstalled.addListener(refreshStatus);
