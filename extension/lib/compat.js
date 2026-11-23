// The few places Chrome and Firefox differ.

/** The WebExtensions API object: Firefox's `browser`, else Chrome's `chrome`. */
export function extensionApi(g = globalThis) {
  return g.browser ?? g.chrome;
}

/**
 * Calls `handler(item, release)` for every new download. On Chrome this hooks
 * `downloads.onDeterminingFilename`, which holds the download's target until
 * `release()` calls `suggest()` (once, and at the latest when the handler settles;
 * suggest() on a download cancelled meanwhile is harmless). Firefox has
 * no such event, so it falls back to `onCreated`, where `release` does nothing.
 */
export function onNewDownload(api, handler) {
  const determining = api.downloads.onDeterminingFilename;
  if (determining) {
    determining.addListener((item, suggest) => {
      let suggested = false;
      const release = () => {
        if (suggested) return;
        suggested = true;
        try {
          suggest();
        } catch (e) {
          console.warn("puppy: suggest failed", e?.message ?? e);
        }
      };
      Promise.resolve()
        .then(() => handler(item, release))
        .catch((e) => console.error("puppy: download handling failed", e))
        .finally(release);
      return true; // suggest() is called asynchronously
    });
  } else {
    api.downloads.onCreated.addListener((item) => {
      Promise.resolve()
        .then(() => handler(item, () => {}))
        .catch((e) => console.error("puppy: download handling failed", e));
    });
  }
}
