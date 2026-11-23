// Handing a browser download to puppy. The browser download is cancelled only after
// puppy accepted it; on any failure it is resumed and the browser keeps it.

import { unsupportedReason, excludedReason, basename } from "./rules.js";
import { loadSettings } from "./settings.js";
import { cookieHeader, addMessage } from "./request.js";
import { send, within, PROTOCOL, TIMEOUT_TEXT } from "./native.js";
import { takeBypass, addRecent, getStatus, setStatus } from "./state.js";

const left = (reason) => ({ kind: "left", reason });
const resumed = (reason) => ({ kind: "resumed", reason });
const swallow = () => {};

/**
 * → {kind: "taken", downloadId} | {kind: "left", reason} | {kind: "resumed", reason}.
 * `release()` (which lets the browser go on) runs exactly once: right after the pause,
 * or when the browser keeps the download.
 */
export async function handleDownload(api, item, release, { now = Date.now, userAgent = navigator.userAgent, ownId } = {}) {
  let released = false;
  const releaseOnce = () => {
    if (released) return;
    released = true;
    release();
  };
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
    releaseOnce();

    const [current] = await api.downloads.search({ id: item.id });
    if (current?.state !== "in_progress") return left("finished in the browser");

    let cookies;
    try {
      cookies = cookieHeader(await api.cookies.getAll({ url: item.url }));
    } catch {
      await api.downloads.resume(item.id).catch(swallow);
      return resumed("cookies unreadable");
    }

    const name = basename(current.filename || item.filename) || basename(new URL(item.url).pathname) || item.url;
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
      pending.then((late) => (late.type === "accepted" ? removeInPuppy(api, late.download_id) : null)).catch(swallow);
      return await giveBack(TIMEOUT_TEXT);
    }
    const reply = r.value;
    if (reply.type === "accepted") {
      await api.downloads.cancel(item.id).catch(swallow);
      await api.downloads.erase({ id: item.id }).catch(swallow);
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
    releaseOnce();
  }
}

/** Takes back a download puppy accepted after the browser already resumed it. */
function removeInPuppy(api, downloadId) {
  return send(api, { id: crypto.randomUUID(), protocol: PROTOCOL, type: "remove", download_id: downloadId }).catch(swallow);
}
