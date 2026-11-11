# Website: browser direct link

A link that opens a room in the browser client with everything in it. Part of the browser client, see `superdoc/features/website.md`; the join-dialog path is `join-links.md`.

## Format

`<site URL>#join=<encodeURIComponent(join link)>`, where the join link is `server|room-id|key[|certhash[|name]]` (`architecture/protocol-spec.md` §11.4). In the browser the site URL is `location.origin + location.pathname`; the published site is the GitHub Pages one. The format is shared with the native client's "Copy browser direct link", so a native user can send a link that opens in any browser. Pure helpers: `docs/js/ui/direct-link.js` (`buildDirectLink`, `parseDirectHash`), unit-tested in `docs/dev/tests.js`.

**Why a fragment:** the fragment is never sent to the web server, so the room key stays out of GitHub Pages and proxy logs. (Same reason the key is not a query parameter.)

## Copying

The control bar's "Copy invite link" button (`bar.js` `#btn-invite`): a left click copies the join link as before; a right click (or a touch long press) opens a small menu (`menus.js` `inviteMenu`, `.menu.m-invite`) with "Copy join link" on top and "Copy browser direct link" below. The room menu (`roomMenu`, the bar's room settings and the header `...`) has "Copy browser direct link" right under "Copy invite link". Both are disabled until the room is joined, like the existing item: the link carries the relay's certificate hash, known only after connecting. The link is built from `RoomEntry.inviteLink()`, so it carries the room name suggestion too.

## Visiting a link

`app.js` `joinFromHash` runs at boot (before the first render) and on `hashchange` (a link pasted into the address bar of an open tab). It first clears the hash with `history.replaceState`, whatever follows: the key must not linger in the address bar and a reload must not re-add the room.

- **Valid link:** no dialog. `addRoom` saves it in the room store (`sss.rooms`), exactly like a join-dialog add, so it is listed after a reload; the room is selected and connected. If a saved room with the same relay and room id exists, no duplicate is made: its stored link (key, certhash) is replaced when the incoming one differs (a refreshed invite), and a live connection is reconnected with it. The relay address and cert hash are remembered as the dialog does (`rememberServer`).
- **No name chosen yet:** the normal "Choose your name" modal shows over the connecting room; its field is prefilled with the link's optional name suggestion (`namePrefill`), editable, not saved until the user presses Save.
- **Invalid link:** falls back to the join dialog prefilled with the text and the usual error.

Why connect without asking: the user asked that a visited link "automatically open the channel with the encryption key and so on", saved in the browser instance (user request 2026-11-11, row 179).

## Test

`node docs/dev/e2e/direct-link.mjs <relay cert hash> [port]` (relay from `docs/dev/relay.sh`): copy via context menu, fresh-profile visit, hash cleared, reload keeps the room, same link again and hashchange add no duplicate, invalid link opens the dialog, name prompt prefilled. Verified 2026-11-11, all pass.
