// Per-session state in storage.session: the one-shot bypass, the recent handovers and
// the cached handshake status.

export const BYPASS_MS = 60_000;
export const RECENT_MAX = 5;

// Read-modify-write updates run one at a time, so concurrent downloads neither both
// take the bypass nor drop each other's recent entry.
let queue = Promise.resolve();
function serialized(fn) {
  const run = queue.then(fn);
  queue = run.catch(() => {});
  return run;
}

export function armBypass(api, now) {
  return serialized(async () => {
    await api.storage.session.set({ bypassUntil: now + BYPASS_MS });
    await api.action.setBadgeText({ text: "1" });
  });
}

/** True once if the bypass is armed and unexpired; clears it (and the badge) whenever it is set. */
export function takeBypass(api, now) {
  return serialized(async () => {
    const { bypassUntil } = await api.storage.session.get("bypassUntil");
    if (bypassUntil === undefined) return false;
    await api.storage.session.remove("bypassUntil");
    await api.action.setBadgeText({ text: "" });
    return now < bypassUntil;
  });
}

export function addRecent(api, entry) {
  return serialized(async () => {
    const recent = await getRecent(api);
    await api.storage.session.set({ recent: [entry, ...recent].slice(0, RECENT_MAX) });
  });
}

/** Remembers a download the extension paused, until it is done with it. */
export function markPaused(api, id) {
  return serialized(async () => {
    const { paused = [] } = await api.storage.session.get("paused");
    await api.storage.session.set({ paused: [...paused.filter((p) => p !== id), id] });
  });
}

export function unmarkPaused(api, id) {
  return serialized(async () => {
    const { paused = [] } = await api.storage.session.get("paused");
    await api.storage.session.set({ paused: paused.filter((p) => p !== id) });
  });
}

/** The remembered paused ids, forgetting them. */
export function takePaused(api) {
  return serialized(async () => {
    const { paused = [] } = await api.storage.session.get("paused");
    await api.storage.session.remove("paused");
    return paused;
  });
}

export async function getRecent(api) {
  const { recent } = await api.storage.session.get("recent");
  return recent ?? [];
}

export async function getStatus(api) {
  const { status } = await api.storage.session.get("status");
  return status;
}

export async function setStatus(api, status) {
  await api.storage.session.set({ status });
}
