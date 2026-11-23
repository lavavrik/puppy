// The service worker: hands new downloads to puppy, arms the bypass from the
// keyboard shortcut, caches the handshake status and resumes downloads a dead
// worker left paused.

import { extensionApi, onNewDownload } from "./lib/compat.js";
import { handleDownload, resumeStranded } from "./lib/takeover.js";
import { hello } from "./lib/native.js";
import { armBypass, setStatus } from "./lib/state.js";

const api = extensionApi();

// Queued before any download event of this worker is handled (state.js serializes it).
resumeStranded(api).catch((e) => console.warn("puppy: could not resume stranded downloads", e?.message ?? e));

onNewDownload(api, (item, release) => handleDownload(api, item, release, { ownId: api.runtime.id }));

api.commands.onCommand.addListener((command) => {
  if (command === "bypass-next") return armBypass(api, Date.now());
});

const refreshStatus = async () => setStatus(api, await hello(api, api.runtime.getManifest().version));
// Stranded downloads were already resumed at worker start, above.
api.runtime.onStartup.addListener(refreshStatus);
api.runtime.onInstalled.addListener(refreshStatus);
