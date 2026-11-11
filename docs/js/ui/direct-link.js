// Browser direct link (row 179): `<site URL>#join=<encodeURIComponent(join link)>`, shared with the native client's
// "Copy browser direct link". Pure helpers, no DOM state. Page: superdoc/features/website/direct-link.md.
// Why the fragment: it is never sent to the web server, so the room key stays out of Pages / proxy logs.

/** The direct link for `joinLink` on the site at `siteUrl` (origin + path, no query or hash). */
export const buildDirectLink = (siteUrl, joinLink) => `${siteUrl}#join=${encodeURIComponent(joinLink)}`;

/** The join link carried by a location hash (`#join=...`), or null when the hash is not a direct link (or is malformed). */
export function parseDirectHash(hash) {
  const m = /^#join=(.+)$/.exec(String(hash || ""));
  if (!m) return null;
  try {
    return decodeURIComponent(m[1]);
  } catch {
    return null;
  }
}
