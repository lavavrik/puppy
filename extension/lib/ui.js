// Pure helpers for the options page and the popup.

import { normalizeExtensions, normalizeSites } from "./settings.js";
import { PROTOCOL } from "./native.js";

const MIB = 1048576;
const MAX_MIB = 1048576; // 1 TiB

/** The one-line handshake status, `extensionVersion` being this extension's own. */
export function statusLine(status, extensionVersion) {
  switch (status?.state) {
    case undefined:
      return "Checking puppy…";
    case "ok":
      return `Ready: puppy ${status.puppyVersion}, daemon ${status.daemonVersion}, extension ${extensionVersion}`;
    case "no_host":
      return "puppy is not set up for this browser. Run: puppy browser install";
    case "no_daemon":
      return `puppy is installed but its daemon does not answer: ${status.message}`;
    case "mismatch":
      if (status.puppyVersion === undefined || status.hostProtocol === undefined) {
        return `puppy and this extension speak different protocols${status.message ? ` (${status.message})` : ""}: update one of them. Downloads stay in the browser.`;
      }
      return `puppy ${status.puppyVersion} speaks protocol ${status.hostProtocol}, this extension ${PROTOCOL}: update one of them. Downloads stay in the browser.`;
    default:
      return `Cannot reach puppy: ${status.message ?? "unknown error"}`;
  }
}

/** Form field values (all text but `enabled`) → settings. */
export function formToSettings({ enabled, minSizeMiB, extensions, sites }) {
  const mib = Number(String(minSizeMiB ?? "").trim().replace(",", ".") || 0);
  return {
    enabled: Boolean(enabled),
    minSizeBytes: Number.isFinite(mib) && mib > 0 ? Math.round(Math.min(mib, MAX_MIB) * MIB) : 0,
    skipExtensions: normalizeExtensions(extensions),
    skipSites: normalizeSites(sites),
  };
}

/** Settings → form field values. */
export function settingsToForm(settings) {
  return {
    enabled: settings.enabled,
    minSizeMiB: String(parseFloat((settings.minSizeBytes / MIB).toFixed(10))),
    extensions: settings.skipExtensions.join(", "),
    sites: settings.skipSites.join("\n"),
  };
}

/** The recent handovers as display lines. */
export function recentLines(recent) {
  return (recent ?? []).map((r) => ({
    text: r.ok ? `✓ ${r.name}` : `✗ ${r.name}${r.detail ? ` — ${r.detail}` : ""}`,
    ok: Boolean(r.ok),
  }));
}

/** The form's settings laid over freshly loaded ones, so a field edited elsewhere (the popup's toggle) survives. */
export function mergeForm(current, edited, fields) {
  const next = { ...current };
  for (const f of fields) next[f] = edited[f];
  return next;
}
