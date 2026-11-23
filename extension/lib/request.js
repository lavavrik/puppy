// What the extension tells the host about a download.

import { basename } from "./rules.js";
import { PROTOCOL } from "./native.js";

/** A Cookie header, cookies with longer paths first (as browsers send them); null when empty. */
export function cookieHeader(cookies) {
  const text = [...cookies]
    .sort((a, b) => (b.path?.length ?? 0) - (a.path?.length ?? 0))
    .map((c) => (c.name ? `${c.name}=${c.value}` : c.value))
    .join("; ");
  return text || null;
}

export function addMessage(item, cookie, userAgent, id) {
  return {
    id,
    protocol: PROTOCOL,
    type: "add",
    url: item.url,
    final_url: item.finalUrl || null,
    filename: basename(item.filename) || null,
    referrer: item.referrer || null,
    cookies: cookie || null,
    user_agent: userAgent || null,
  };
}
