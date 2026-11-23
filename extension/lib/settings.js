// The user's settings, kept in storage.sync under "settings".

export const DEFAULTS = Object.freeze({ enabled: true, minSizeBytes: 0, skipExtensions: [], skipSites: [] });

export async function loadSettings(api) {
  const { settings } = await api.storage.sync.get("settings");
  return { ...DEFAULTS, ...(settings ?? {}) };
}

export async function saveSettings(api, settings) {
  await api.storage.sync.set({ settings });
}

const words = (textOrList) =>
  (Array.isArray(textOrList) ? textOrList.join(" ") : String(textOrList ?? "")).split(/[\s,]+/).filter(Boolean);

const dedup = (list) => [...new Set(list.filter(Boolean))];

/** "zip, .ISO  pdf" → ["zip", "iso", "pdf"]. */
export function normalizeExtensions(textOrList) {
  return dedup(words(textOrList).map((w) => w.toLowerCase().replace(/^\.+/, "")));
}

/** "Example.com\n*.foo.org/path, https://bar.net" → ["example.com", "foo.org", "bar.net"]. */
export function normalizeSites(textOrList) {
  return dedup(
    words(textOrList).map((w) =>
      w
        .toLowerCase()
        .replace(/^[a-z][a-z0-9+.-]*:\/\//, "") // scheme
        .replace(/[/?#].*$/, "") // path, query, fragment
        .replace(/^.*@/, "") // userinfo
        .replace(/:\d*$/, "") // port
        .replace(/^(\*\.)+/, "")
        .replace(/^\.+|\.+$/g, ""),
    ),
  );
}
