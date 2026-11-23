// Which downloads puppy may take. Reasons are short English strings.

function parseUrl(text) {
  try {
    return new URL(text);
  } catch {
    return null;
  }
}

/** Why the browser must keep this download whatever the settings say, or null. */
export function unsupportedReason(item, ownId) {
  const url = parseUrl(item.url);
  if (!url || (url.protocol !== "http:" && url.protocol !== "https:")) return "not http(s)";
  if (item.incognito) return "incognito";
  if (item.byExtensionId && item.byExtensionId !== ownId) return "started by an extension";
  if (item.state !== "in_progress") return "not in progress";
  return null;
}

/** Why the user's settings leave this download to the browser, or null. */
export function excludedReason(item, settings) {
  if (!settings.enabled) return "disabled";
  if (settings.minSizeBytes > 0 && item.totalBytes > 0 && item.totalBytes < settings.minSizeBytes) {
    return "smaller than the minimum size";
  }
  const name = item.filename || parseUrl(item.url)?.pathname || "";
  const ext = fileExtension(name);
  if (ext && settings.skipExtensions.includes(ext)) return "file type";
  const hosts = [item.url, item.finalUrl, item.referrer].map((u) => (u ? parseUrl(u)?.hostname : null)).filter(Boolean);
  if (hosts.some((h) => settings.skipSites.some((s) => siteMatches(h, s)))) return "site";
  return null;
}

/** `site` matches itself and its subdomains: "example.com" ⊇ "dl.example.com", not "badexample.com". */
export function siteMatches(host, site) {
  if (!site) return false;
  const h = host.toLowerCase();
  const s = site.toLowerCase();
  return h === s || h.endsWith("." + s);
}

/** The lowercased extension of the name's basename, "" if it has none. */
export function fileExtension(name) {
  const b = basename(name);
  const dot = b.lastIndexOf(".");
  return dot > 0 ? b.slice(dot + 1).toLowerCase() : "";
}

/** The last component of a `/` or `\` separated path. */
export function basename(name) {
  return String(name ?? "").split(/[\\/]/).pop();
}
