# Terminology

Project-specific vocabulary. When the user uses a term defined here, use the same
meaning.

**If the user uses a non-standard term that is not defined here, ask what they mean by
it before acting on it.** Once its meaning is clear, add it to this file. Keep this
file up to date automatically: whenever the user introduces or clarifies a term, add or
amend the entry here.

## Terms

- **Term** — definition. Link to the doc that shows it in use if useful.
- **Join link** — `server|room-id|key` or `server|room-id|key|certhash`; everything a client needs to enter a room. `protocol-spec.md` §11.4.
- **Cert hash** (`certhash`) — SHA-256 of the relay's leaf TLS certificate (DER), base64url without padding; what a browser pins via WebTransport `serverCertificateHashes`. Changes whenever the relay rotates its self-signed cert. `protocol-spec.md` §11.3.
