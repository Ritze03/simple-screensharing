// Dev-only: lets a headless test page report back to dev/headless.mjs (POST /log, POST /result?id=N, GET/POST /kv/<name>).
// Outside the runner (no ?id= in the URL) everything is a no-op, so the pages also work when opened by hand.

const q = new URLSearchParams(location.search);
export const params = q;
export const pageId = q.get("id");
export const inRunner = pageId !== null;

export function log(...args) {
  const msg = args.map((a) => (typeof a === "string" ? a : JSON.stringify(a, (_, v) => (typeof v === "bigint" ? `${v}n` : v)))).join(" ");
  console.log(msg);
  if (inRunner) fetch(`/log?id=${pageId}`, { method: "POST", body: msg }).catch(() => {});
}

export async function postResult(obj) {
  const body = JSON.stringify(obj, (_, v) => (typeof v === "bigint" ? `${v}n` : v));
  if (inRunner) await fetch(`/result?id=${pageId}`, { method: "POST", body });
}

export async function kvSet(name, value) {
  await fetch(`/kv/${name}`, { method: "POST", body: typeof value === "string" ? value : JSON.stringify(value) });
}

/** Long-poll until another page (or the runner) sets `name`. */
export async function kvGet(name, { timeoutMs = 60_000 } = {}) {
  const t0 = Date.now();
  while (Date.now() - t0 < timeoutMs) {
    const r = await fetch(`/kv/${name}`);
    if (r.status === 200) return await r.text();
  }
  throw new Error(`timeout waiting for kv ${name}`);
}

window.addEventListener("error", (e) => log("window error:", e.message));
window.addEventListener("unhandledrejection", (e) => log("unhandled rejection:", String(e.reason && e.reason.stack ? e.reason.stack : e.reason)));
