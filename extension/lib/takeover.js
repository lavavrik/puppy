// Handing a browser download to puppy. The browser download is cancelled only after
// puppy accepted it; on any failure it is resumed and the browser keeps it.

import { unsupportedReason, excludedReason, basename } from "./rules.js";
import { loadSettings } from "./settings.js";
import { cookieHeader, addMessage } from "./request.js";
import { send, within, hello, PROTOCOL, TIMEOUT_TEXT } from "./native.js";
import { takeBypass, addRecent, getStatus, setStatus, markPaused, markAccepted, unmarkPaused, takePaused } from "./state.js";

const left = (reason) => ({ kind: "left", reason });
const resumed = (reason) => ({ kind: "resumed", reason });
const swallow = () => {};
export const FINISHED_FIRST = "the browser finished it first";
/** A cached protocol mismatch older than this is checked again before a download is left. */
export const MISMATCH_RECHECK_MS = 60_000;

/**
 * → {kind: "taken", downloadId} | {kind: "left", reason} | {kind: "resumed", reason}.
 * `release()` (which lets the browser determine the target) runs exactly once, when the
 * outcome is known: after the cancel for a taken download, so Chrome never shows its
 * "where to save" dialog for it, and after the resume otherwise.
 */
export async function handleDownload(api, item, release, { now = Date.now, userAgent = navigator.userAgent, ownId } = {}) {
  let paused = false;
  // Paused, and neither resumed nor handed over yet: a throw must resume it.
  let holding = false;
  try {
    const unsupported = unsupportedReason(item, ownId);
    if (unsupported) return left(unsupported);
    const excluded = excludedReason(item, await loadSettings(api));
    if (excluded) return left(excluded);
    if (await takeBypass(api, now())) return left("bypass");
    if (await mismatched(api, now())) return left("protocol mismatch");

    try {
      await api.downloads.pause(item.id);
    } catch {
      return left("could not pause");
    }
    paused = true;
    holding = true;
    await markPaused(api, item.id);

    const [current] = await api.downloads.search({ id: item.id });
    if (current?.state !== "in_progress") return left("finished in the browser");

    let cookies;
    try {
      cookies = cookieHeader(await api.cookies.getAll({ url: item.url }));
    } catch {
      holding = false;
      await api.downloads.resume(item.id).catch(swallow);
      return resumed("cookies unreadable");
    }

    const name = basename(current.filename || item.filename) || basename(new URL(item.url).pathname) || new URL(item.url).hostname;
    const record = (ok, detail) => addRecent(api, { name, ok, detail, at: now() });
    const giveBack = async (detail) => {
      holding = false;
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
      // Recorded before the cancel: a worker dying in between leaves both copies, and
      // the next worker's resumeStranded removes puppy's.
      await markAccepted(api, item.id, reply.download_id).catch(swallow);
      const finishedFirst = await browserFinishedFirst(api, item.id);
      holding = false;
      if (finishedFirst) {
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
  } catch (e) {
    if (holding) await api.downloads.resume(item.id).catch(swallow);
    throw e;
  } finally {
    if (paused) await unmarkPaused(api, item.id).catch(swallow);
    release();
  }
}

/**
 * Whether puppy is known to speak another protocol. A cached mismatch older than
 * MISMATCH_RECHECK_MS is checked again with a fresh hello (puppy may have been updated),
 * and the answer cached.
 */
async function mismatched(api, nowMs) {
  const status = await getStatus(api);
  if (status?.state !== "mismatch") return false;
  if (nowMs - (status.at ?? 0) <= MISMATCH_RECHECK_MS) return true;
  const fresh = await hello(api, api.runtime.getManifest().version);
  await setStatus(api, { ...fresh, at: nowMs });
  return fresh.state === "mismatch";
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
 * died mid-takeover). Runs once per worker start, before any new download is handled.
 * One puppy had already accepted is taken back from puppy (with its files) while the
 * browser still has its copy; if that copy is gone, it was cancelled for puppy, which
 * keeps it.
 */
export async function resumeStranded(api) {
  for (const { id, puppyId } of await takePaused(api)) {
    const [d] = await api.downloads.search({ id }).catch(() => []);
    if (puppyId != null && (d?.state === "in_progress" || d?.state === "complete")) await removeInPuppy(api, puppyId);
    if (d?.state === "in_progress" && d.paused) await api.downloads.resume(id).catch(swallow);
  }
}
