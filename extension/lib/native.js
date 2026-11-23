// Talking to `puppy native-host`: one process per message (runtime.sendNativeMessage).

export const HOST_NAME = "dev.puppy.native_host";
export const PROTOCOL = 1;
export const TIMEOUT_MS = 10_000;
export const TIMEOUT_TEXT = "puppy did not answer in 10 s";

/** Sends `msg` and returns the host's reply; rejects if the reply is for another request. */
export async function send(api, msg) {
  const reply = await api.runtime.sendNativeMessage(HOST_NAME, msg);
  if (!reply || reply.id !== msg.id) throw new Error("the host answered another request");
  return reply;
}

/** `{value}` when `promise` resolves within `ms`, `{timedOut: true}` otherwise; rejects with it. */
export function within(promise, ms = TIMEOUT_MS) {
  let timer;
  const timeout = new Promise((resolve) => {
    timer = setTimeout(() => resolve({ timedOut: true }), ms);
  });
  return Promise.race([promise.then((value) => ({ value })), timeout]).finally(() => clearTimeout(timer));
}

/** The handshake. Never rejects: failures become a Status. */
export async function hello(api, extensionVersion) {
  const msg = { id: crypto.randomUUID(), protocol: PROTOCOL, type: "hello", extension_version: extensionVersion };
  let status;
  try {
    const r = await within(send(api, msg));
    status = r.timedOut ? { state: "error", message: TIMEOUT_TEXT } : fromReply(r.value);
  } catch (e) {
    const message = String(e?.message ?? e);
    const missing = /not found|forbidden/i.test(message);
    status = { state: missing ? "no_host" : "error", message };
  }
  return { ...status, at: Date.now() };
}

function fromReply(reply) {
  if (reply.type === "hello") {
    const s = { puppyVersion: reply.puppy_version, daemonVersion: reply.daemon_version ?? null, hostProtocol: reply.protocol };
    if (reply.protocol !== PROTOCOL) return { state: "mismatch", ...s, message: `the host speaks protocol ${reply.protocol}` };
    if (reply.daemon_error) return { state: "no_daemon", ...s, message: reply.daemon_error };
    return { state: "ok", ...s };
  }
  if (reply.type === "error") {
    return { state: reply.code === "protocol_mismatch" ? "mismatch" : "error", message: reply.message };
  }
  return { state: "error", message: `unexpected reply "${reply.type}"` };
}
