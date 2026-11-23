// Pure helpers for the options page and the popup.

import { normalizeExtensions, normalizeSites } from "./settings.js";
import { PROTOCOL } from "./native.js";

const MIB = 1048576;

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
      return `puppy ${status.puppyVersion} speaks protocol ${status.hostProtocol}, this extension ${PROTOCOL}: update one of them. Downloads stay in the browser.`;
    default:
      return `Cannot reach puppy: ${status.message}`;
  }
}

/** Form field values (all text but `enabled`) → settings. */
export function formToSettings({ enabled, minSizeMiB, extensions, sites }) {
  const mib = Number(String(minSizeMiB ?? "").trim() || 0);
  return {
    enabled: Boolean(enabled),
    minSizeBytes: Number.isFinite(mib) && mib > 0 ? Math.round(mib * MIB) : 0,
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
