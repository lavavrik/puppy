// Handing a browser download to puppy. The browser download is cancelled only after
// puppy accepted it; on any failure it is resumed and the browser keeps it.

import { unsupportedReason, excludedReason, basename } from "./rules.js";
import { loadSettings } from "./settings.js";
import { cookieHeader, addMessage } from "./request.js";
import { send, within, PROTOCOL, TIMEOUT_TEXT } from "./native.js";
import { takeBypass, addRecent, getStatus, setStatus, markPaused, unmarkPaused, takePaused } from "./state.js";

const left = (reason) => ({ kind: "left", reason });
const resumed = (reason) => ({ kind: "resumed", reason });
const swallow = () => {};
export const FINISHED_FIRST = "the browser finished it first";

/**
 * → {kind: "taken", downloadId} | {kind: "left", reason} | {kind: "resumed", reason}.
 * `release()` (which lets the browser determine the target) runs exactly once, when the
 * outcome is known: after the cancel for a taken download, so Chrome never shows its
 * "where to save" dialog for it, and after the resume otherwise.
 */
export async function handleDownload(api, item, release, { now = Date.now, userAgent = navigator.userAgent, ownId } = {}) {
  let paused = false;
  try {
    const unsupported = unsupportedReason(item, ownId);
    if (unsupported) return left(unsupported);
    const excluded = excludedReason(item, await loadSettings(api));
    if (excluded) return left(excluded);
    if (await takeBypass(api, now())) return left("bypass");
    if ((await getStatus(api))?.state === "mismatch") return left("protocol mismatch");

    try {
      await api.downloads.pause(item.id);
    } catch {
      return left("could not pause");
    }
    paused = true;
    await markPaused(api, item.id);

    const [current] = await api.downloads.search({ id: item.id });
    if (current?.state !== "in_progress") return left("finished in the browser");

    let cookies;
    try {
      cookies = cookieHeader(await api.cookies.getAll({ url: item.url }));
    } catch {
      await api.downloads.resume(item.id).catch(swallow);
      return resumed("cookies unreadable");
    }

    const name = basename(current.filename || item.filename) || basename(new URL(item.url).pathname) || new URL(item.url).hostname;
    const record = (ok, detail) => addRecent(api, { name, ok, detail, at: now() });
    const giveBack = async (detail) => {
      await api.downloads.resume(item.id).catch(swallow);
      await record(false, detail);
      return resumed(detail);
    };

    const pending = send(api, addMessage(item, cookies, userAgent, crypto.randomUUID()));
    let r;
    try {
      r = await within(pending);
    } catch (e) {
      return await giveBack(`puppy is not reachable: ${e?.message ?? e}`);
    }
    if (r.timedOut) {
      // The browser keeps it; a late accept would be a second copy in puppy.
      pending.then((late) => (late.type === "accepted" ? removeInPuppy(api, late.download_id) : null)).catch(swallow);
      return await giveBack(TIMEOUT_TEXT);
    }
    const reply = r.value;
    if (reply.type === "accepted") {
      if (await browserFinishedFirst(api, item.id)) {
        await removeInPuppy(api, reply.download_id);
        await record(false, FINISHED_FIRST);
        return left(FINISHED_FIRST);
      }
      await api.downloads.erase({ id: item.id }).catch((e) => {
        console.warn(`puppy: could not erase browser download ${item.id}: ${e?.message ?? e}`);
      });
      await record(true, null);
      return { kind: "taken", downloadId: reply.download_id };
    }
    if (reply.type === "error") {
      if (reply.code === "protocol_mismatch") {
        await setStatus(api, { state: "mismatch", message: reply.message, at: now() });
      }
      return await giveBack(reply.message);
    }
    return await giveBack(`unexpected reply "${reply.type}"`);
  } finally {
    if (paused) await unmarkPaused(api, item.id).catch(swallow);
    release();
  }
}

/**
 * Cancels the browser download puppy took. True only when the cancel failed because
 * the browser already completed it; a download gone, interrupted or cancelled is puppy's.
 */
async function browserFinishedFirst(api, id) {
  try {
    await api.downloads.cancel(id);
    return false;
  } catch {
    const [now] = await api.downloads.search({ id }).catch(() => []);
    return now?.state === "complete";
  }
}

/** Takes back a download puppy accepted while the browser has its own copy. */
function removeInPuppy(api, downloadId) {
  return send(api, {
    id: crypto.randomUUID(),
    protocol: PROTOCOL,
    type: "remove",
    download_id: downloadId,
    delete_files: true,
  }).catch(swallow);
}

/**
 * Resumes the downloads a previous service worker paused and never finished with (it
 * died mid-takeover). Runs at every worker start, before any new download is handled.
 */
export async function resumeStranded(api) {
  for (const id of await takePaused(api)) {
    const [d] = await api.downloads.search({ id }).catch(() => []);
    if (d?.state === "in_progress" && d.paused) await api.downloads.resume(id).catch(swallow);
  }
}
