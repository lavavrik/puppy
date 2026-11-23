// An in-memory stand-in for the `chrome.*` APIs the extension uses. Every call that
// matters is appended to `api.calls`, in order.

function event() {
  const listeners = [];
  return {
    listeners,
    addListener(fn) {
      listeners.push(fn);
    },
    fire(...args) {
      return listeners.map((fn) => fn(...args));
    },
  };
}

function area(initial = {}) {
  const data = { ...initial };
  return {
    data,
    async get(key) {
      if (key == null) return { ...data };
      const keys = Array.isArray(key) ? key : [key];
      return Object.fromEntries(keys.filter((k) => k in data).map((k) => [k, data[k]]));
    },
    async set(items) {
      Object.assign(data, structuredClone(items));
    },
    async remove(key) {
      for (const k of Array.isArray(key) ? key : [key]) delete data[k];
    },
  };
}

/** The host's default answer: accept as download 3, echoing the request id. */
export const acceptAs = (downloadId) => (msg) => ({ id: msg.id, type: "accepted", download_id: downloadId });

export function fakeApi({
  cookies = [],
  reply = acceptAs(3),
  settings,
  session = {},
  downloads = [],
  firefox = false,
  failPause = false,
  failCookies = false,
} = {}) {
  const calls = [];
  const items = downloads;
  const log = (...entry) => calls.push(entry);
  const api = {
    calls,
    items,
    downloads: {
      onCreated: event(),
      async pause(id) {
        log("pause", id);
        if (failPause) throw new Error("Download must be in progress");
      },
      async resume(id) {
        log("resume", id);
      },
      async cancel(id) {
        log("cancel", id);
      },
      async erase(query) {
        log("erase", query);
        return [query.id];
      },
      async search(query) {
        return items.filter((d) => d.id === query.id).map((d) => ({ ...d }));
      },
    },
    cookies: {
      async getAll(details) {
        log("cookies", details.url);
        if (failCookies) throw new Error("No host permissions for cookies");
        return cookies;
      },
    },
    runtime: {
      id: "ownid",
      getManifest: () => ({ version: "0.1.0" }),
      onStartup: event(),
      onInstalled: event(),
      async sendNativeMessage(host, msg) {
        log("send", host, msg);
        const r = await reply(msg);
        if (r instanceof Error) throw r;
        return r;
      },
    },
    commands: { onCommand: event() },
    action: {
      async setBadgeText({ text }) {
        log("badge", text);
      },
    },
    storage: {
      sync: area(settings === undefined ? {} : { settings }),
      session: area(session),
    },
  };
  if (!firefox) api.downloads.onDeterminingFilename = event();
  return api;
}

export function item(over = {}) {
  return {
    id: 7,
    url: "https://dl.example.com/f.iso",
    finalUrl: "https://cdn.example.net/f.iso",
    filename: "f.iso",
    totalBytes: 5e6,
    state: "in_progress",
    incognito: false,
    referrer: "https://example.com/p",
    ...over,
  };
}

/** Lets pending promise callbacks run (setImmediate is not mocked by the tests). */
export const flush = () => new Promise((r) => setImmediate(r));

/** Flushes until `pred()` holds (bounded). */
export async function until(pred) {
  for (let i = 0; i < 100 && !pred(); i++) await flush();
  if (!pred()) throw new Error("condition never held");
}
